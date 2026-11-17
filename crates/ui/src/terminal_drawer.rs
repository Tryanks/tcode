#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
    time::Duration,
};
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::{ContextMenuExt as _, DropdownMenu as _};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
    widgets::tabs::{TabBar, TabItem},
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Bounds, ClipboardItem, ContentMask, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, FontFeatures, FontStyle, FontWeight, Hsla, InputHandler,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, Role, ScrollEvent,
    SharedString, Styled as _, Task, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window,
    canvas, div, fill, font, point, prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_base::{ElementExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable};
use tcode_protocol::terminal::{
    CellFlags, CellWidth, CursorShape, TerminalColor, TerminalMode as Mode,
    mappings::{self, GridPoint, Modifiers as TermModifiers, MouseButton as TermMouseButton},
};

use crate::{
    material,
    store::{
        TerminalChange, WorkspaceStore,
        terminal::{HyperlinkMatch, SelectionKind, SelectionSide, TerminalModel},
    },
    terminal_key_bar::{
        TerminalKey, TerminalKeyBar, TerminalKeyBarEvent, should_show_terminal_key_bar,
    },
};
use tcode_core::ui::{MAX_TERMINALS_PER_SESSION, TerminalSplitDirection};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) const TERMINAL_FONT_SIZE: f32 = 13.;
pub(crate) const TERMINAL_CELL_WIDTH: f32 = 7.83;
pub(crate) const TERMINAL_CELL_HEIGHT: f32 = 17.;
#[cfg(target_os = "macos")]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) const TERMINAL_FONT_FAMILY: &str = "Lilex";
const PANE_PADDING: f32 = 8.;
const SELECTION_DRAG_THRESHOLD: f32 = 2.;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenPoint {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionDragAction {
    None,
    ClearAndWait,
    Start {
        kind: SelectionKind,
        point: (usize, usize),
        side: SelectionSide,
    },
    Update {
        point: (usize, usize),
        side: SelectionSide,
    },
    StartSimpleAndUpdate {
        anchor: (usize, usize),
        anchor_side: SelectionSide,
        point: (usize, usize),
        side: SelectionSide,
    },
}

#[derive(Default)]
struct SelectionDrag {
    selecting: Option<u64>,
    pending_simple: Option<(u64, (usize, usize), SelectionSide)>,
    mouse_down: Option<(u64, ScreenPoint)>,
    last_reported_point: HashMap<u64, (usize, usize)>,
}

impl SelectionDrag {
    fn on_down(
        &mut self,
        terminal_id: u64,
        position: ScreenPoint,
        point: (usize, usize),
        side: SelectionSide,
        click_count: usize,
        shift: bool,
    ) -> SelectionDragAction {
        self.mouse_down = Some((terminal_id, position));
        self.selecting = None;
        self.pending_simple = None;
        let kind = match click_count {
            1 => SelectionKind::Simple,
            2 => SelectionKind::Semantic,
            3 => SelectionKind::Lines,
            _ => return SelectionDragAction::None,
        };
        if kind == SelectionKind::Simple && shift {
            return SelectionDragAction::Update { point, side };
        }
        if kind == SelectionKind::Simple {
            self.pending_simple = Some((terminal_id, point, side));
            return SelectionDragAction::ClearAndWait;
        }
        SelectionDragAction::Start { kind, point, side }
    }

    fn on_move(
        &mut self,
        terminal_id: u64,
        position: ScreenPoint,
        point: (usize, usize),
        side: SelectionSide,
    ) -> SelectionDragAction {
        if !self.pressed_on(terminal_id) {
            return SelectionDragAction::None;
        }
        if self.selecting != Some(terminal_id)
            && let Some((_, mouse_down)) = self.mouse_down
        {
            if !selection_drag_started(position.x - mouse_down.x, position.y - mouse_down.y) {
                return SelectionDragAction::None;
            }
            self.selecting = Some(terminal_id);
            if self
                .pending_simple
                .is_some_and(|(pending_id, _, _)| pending_id == terminal_id)
                && let Some((_, anchor, anchor_side)) = self.pending_simple.take()
            {
                return SelectionDragAction::StartSimpleAndUpdate {
                    anchor,
                    anchor_side,
                    point,
                    side,
                };
            }
        }
        self.selecting = Some(terminal_id);
        SelectionDragAction::Update { point, side }
    }

    fn on_up(&mut self, terminal_id: u64) -> bool {
        let was_selecting = self.selecting == Some(terminal_id);
        if was_selecting {
            self.selecting = None;
        }
        if self
            .mouse_down
            .is_some_and(|(mouse_id, _)| mouse_id == terminal_id)
        {
            self.mouse_down = None;
        }
        if self
            .pending_simple
            .is_some_and(|(pending_id, _, _)| pending_id == terminal_id)
        {
            self.pending_simple = None;
        }
        self.last_reported_point.remove(&terminal_id);
        was_selecting
    }

    /// Whether the left button went down on `terminal_id` to select and is
    /// still held.
    fn pressed_on(&self, terminal_id: u64) -> bool {
        self.mouse_down
            .is_some_and(|(mouse_id, _)| mouse_id == terminal_id)
    }

    fn should_report_mouse_move(&mut self, terminal_id: u64, point: (usize, usize)) -> bool {
        if self.last_reported_point.get(&terminal_id) == Some(&point) {
            false
        } else {
            self.last_reported_point.insert(terminal_id, point);
            true
        }
    }
}

pub(crate) const CONTEXT: &str = "Terminal";

#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalCopy(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalPaste(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalSelectAll(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalClear(u64);
#[derive(Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalAddContext(u64);

/// A tab's context-menu action. Restart and the splits act on the active
/// terminal, so the tab is activated first.
#[derive(Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
struct TerminalTabMenu {
    id: u64,
    kind: TerminalTabMenuKind,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
enum TerminalTabMenuKind {
    Restart,
    SplitHorizontal,
    SplitVertical,
    Close,
}

/// A drawer action that does not fit the compact toolbar row and lives in its
/// overflow menu, which addresses items by action rather than by callback.
#[derive(Action, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_terminal, no_json)]
enum TerminalOverflow {
    SplitHorizontal,
    SplitVertical,
    Restart,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardShortcut {
    Copy,
    Paste,
}

#[derive(Clone, Copy)]
struct GridGeometry {
    bounds: Bounds<Pixels>,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
}

#[derive(Clone)]
struct MarkedText {
    terminal_id: u64,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridTextStyle {
    pub(crate) fg: TerminalColor,
    pub(crate) bg: TerminalColor,
    bold: bool,
    italic: bool,
    underline: bool,
    underline_wavy: bool,
    selected: bool,
    cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchedTextRun {
    pub(crate) start_col: usize,
    pub(crate) text: SharedString,
    /// The number of non-spacer grid cells, matching Zed's batching model.
    cell_count: usize,
    pub(crate) style: GridTextStyle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalPalette {
    pub(crate) foreground: Hsla,
    pub(crate) background: Hsla,
    pub(crate) selection: Hsla,
}

#[derive(Clone, Copy)]
struct BackgroundRect {
    start_col: usize,
    cell_count: usize,
    color: Hsla,
}

#[derive(Clone, Copy)]
struct CursorPaint {
    row: usize,
    start_col: usize,
    cell_count: usize,
    color: Hsla,
    visible: bool,
    shape: CursorShape,
    focused: bool,
}

#[derive(Clone)]
pub(crate) struct GridPaintData {
    /// Each screen row's layout, top first.
    pub(crate) rows: Vec<Rc<RowPaintData>>,
    cursor: Option<CursorPaint>,
}

#[derive(Default)]
pub(crate) struct RowPaintData {
    pub(crate) text_runs: Vec<BatchedTextRun>,
    backgrounds: Vec<BackgroundRect>,
    selections: Vec<BackgroundRect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CursorRowKey {
    position: (usize, usize),
    shape: CursorShape,
    blinking: bool,
    marked_text: Option<String>,
    focused: bool,
    blink_phase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowLayoutKey {
    /// The selected column span on this row, if any.
    selection: Option<(usize, usize)>,
    hovered_link: Option<((usize, usize), (usize, usize))>,
    cursor: Option<CursorRowKey>,
}

#[derive(Clone)]
struct CachedRowLayout {
    key: RowLayoutKey,
    paint: Rc<RowPaintData>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GridCacheKey {
    cols: usize,
    screen_lines: usize,
    display_offset: usize,
    palette: TerminalPalette,
}

struct TerminalGridCache {
    key: GridCacheKey,
    rows: Vec<Option<CachedRowLayout>>,
}

pub struct TerminalDrawer {
    workspace_store: Entity<WorkspaceStore>,
    /// The thread whose terminals the drawer shows.
    session_id: Option<String>,
    focus_handle: FocusHandle,
    key_bar: Entity<TerminalKeyBar>,
    grid_bounds: Rc<RefCell<HashMap<u64, GridGeometry>>>,
    /// The body's panes, one group whether or not the active terminal is
    /// split, so its pane keeps its place as the second opens beside or
    /// under it and closes again: one state for each way a split lies.
    split_states: [Entity<gpui_base::ResizableState>; 2],
    /// Glides the second pane open and shut.
    split_glide: crate::motion::resize_glide::ResizeGlide,
    /// The scene of the thread the drawer shows, which names the element
    /// its content is drawn under: what it draws keeps its motion (a tab
    /// folding in, the selection's glide, a control coming and going) in
    /// element state keyed by that path, so another thread's starts at rest
    /// rather than from where the last thread's namesakes were. The compact
    /// window draws the drawer as a page of its own, outside its chat.
    scene: crate::motion::Epoch,
    /// The split last shown, which a closing pane is drawn from and lies
    /// along.
    last_split: Option<tcode_protocol::TerminalSplitStatus>,
    /// The body's size when last laid out, which a new split halves.
    body_size: Rc<std::cell::Cell<gpui::Size<Pixels>>>,
    row_layout_cache: RefCell<HashMap<u64, TerminalGridCache>>,
    cell_width: f32,
    cell_height: f32,
    scroll_remainder: HashMap<u64, f32>,
    selection_drag: SelectionDrag,
    /// Scrolls the scrollback while a selection is dragged past the grid's
    /// top or bottom, held there or not.
    drag_scroll: gpui_base::AutoScroll,
    _focus_subscriptions: Vec<gpui::Subscription>,
    marked_text: Option<MarkedText>,
    bell_tabs: HashSet<u64>,
    hovered_link: Option<(u64, HyperlinkMatch)>,
    pressed_link: Option<(u64, String)>,
    last_link_hover: Option<Instant>,
    /// Whether a blinking caret is in its shown half.
    cursor_phase: bool,
    /// Whether a terminal drawn this render has a caret that blinks.
    caret_blinks: Cell<bool>,
    /// Whether the drawer was drawn since the blink last turned.
    blink_drawn: bool,
    blink: Option<Task<()>>,
    terminal_focused: bool,
    current_size: Option<(f32, f32)>,
    _store_subscriptions: Vec<gpui::Subscription>,
}

impl TerminalDrawer {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Bell and OSC 52 arrive with the grid delta rather than on a private
        // event stream, so they are drained where the change lands, for the
        // drawer's own terminals.
        let notice_observer = cx.subscribe_in(
            &workspace_store,
            window,
            |this, store, change: &TerminalChange, window, cx| {
                let own = store
                    .read(cx)
                    .with_terminal_workspace_for(
                        this.session_id.as_deref(),
                        |workspace| workspace.terminal(change.terminal_id).is_some(),
                        cx,
                    )
                    .unwrap_or(false);
                if own {
                    this.drain_terminal_notices(window, cx);
                }
            },
        );
        let focus_handle = cx.focus_handle().tab_index(0).tab_stop(true);
        let key_bar = cx.new(|_| TerminalKeyBar::new(focus_handle.clone()));
        let focus_store = workspace_store.clone();
        let drawer = cx.entity().downgrade();
        let focus_in = window.on_focus_in(&focus_handle, cx, move |_, cx| {
            let Ok(session_id) = drawer.read_with(cx, |drawer, _| drawer.session_id.clone()) else {
                return;
            };
            focus_store.read(cx).with_terminal_workspace_for(
                session_id.as_deref(),
                |workspace| {
                    if let Some(entry) = workspace.active()
                        && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                    {
                        entry.terminal.write_raw(b"\x1b[I".to_vec());
                    }
                },
                cx,
            );
        });
        let focus_store = workspace_store.clone();
        let drawer = cx.entity().downgrade();
        let focus_out = window.on_focus_out(&focus_handle, cx, move |_, _, cx| {
            let Ok(session_id) = drawer.read_with(cx, |drawer, _| drawer.session_id.clone()) else {
                return;
            };
            focus_store.read(cx).with_terminal_workspace_for(
                session_id.as_deref(),
                |workspace| {
                    if let Some(entry) = workspace.active()
                        && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                    {
                        entry.terminal.write_raw(b"\x1b[O".to_vec());
                    }
                },
                cx,
            );
        });
        let key_bar_subscription = cx.subscribe_in(
            &key_bar,
            window,
            |this, _, event: &TerminalKeyBarEvent, window, cx| {
                this.send_key_bar_key(event.0, cx);
                this.focus_handle.focus(window, cx);
            },
        );
        let scene = crate::motion::Epoch::default();
        scene.show(session_id.as_deref());
        Self {
            session_id,
            workspace_store,
            focus_handle,
            key_bar,
            grid_bounds: Rc::new(RefCell::new(HashMap::new())),
            split_states: [
                cx.new(|_| gpui_base::ResizableState::default()),
                cx.new(|_| gpui_base::ResizableState::default()),
            ],
            split_glide: split_glide(&scene),
            scene,
            last_split: None,
            body_size: Rc::default(),
            row_layout_cache: RefCell::new(HashMap::new()),
            cell_width: TERMINAL_CELL_WIDTH,
            cell_height: TERMINAL_CELL_HEIGHT,
            scroll_remainder: HashMap::new(),
            selection_drag: SelectionDrag::default(),
            drag_scroll: gpui_base::AutoScroll::default(),
            _focus_subscriptions: vec![focus_in, focus_out, key_bar_subscription],
            marked_text: None,
            bell_tabs: HashSet::new(),
            hovered_link: None,
            pressed_link: None,
            last_link_hover: None,
            cursor_phase: true,
            caret_blinks: Cell::new(false),
            blink_drawn: false,
            blink: None,
            terminal_focused: false,
            current_size: None,
            _store_subscriptions: vec![notice_observer],
        }
    }

    /// Show the terminals of `session_id`'s thread, or none.
    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.scene.show(session_id.as_deref());
            self.session_id = session_id;
            // No split of the last thread's closes in this one's.
            self.last_split = None;
            cx.notify();
        }
    }

    /// Keep the drawer's motion in `scene`, its host's: what the host shows
    /// is what the drawer shows.
    pub(crate) fn share_scene(&mut self, scene: &crate::motion::Epoch) {
        scene.show(self.session_id.as_deref());
        self.scene = scene.clone();
        self.split_glide = split_glide(scene);
    }

    pub fn is_size(&self, width: f32, height: f32) -> bool {
        self.current_size == Some((width, height))
    }

    /// The drawer is laid out `width` by `height`: a height the thread does
    /// not hold yet is the thread's, and the host's, from now on. Each render
    /// hands the drawer the height the thread holds, which it does not send
    /// back.
    pub fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if self.is_size(width, height) {
            return;
        }
        self.current_size = Some((width, height));
        let session_id = self.session_id.clone().unwrap_or_default();
        let held = self
            .workspace_store
            .read(cx)
            .panel_state_for(Some(&session_id))
            .terminal_height;
        if held != height {
            self.workspace_store.update(cx, |store, cx| {
                store.set_terminal_height_for(session_id, height, cx)
            });
        }
    }

    fn with_terminal(&self, cx: &mut Context<Self>, f: impl FnOnce(&crate::store::ClientTerminal)) {
        self.workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                if let Some(entry) = workspace.active() {
                    f(&entry.terminal);
                }
            },
            cx,
        );
    }

    fn with_terminal_id(
        &self,
        terminal_id: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&crate::store::ClientTerminal),
    ) {
        self.workspace_store.read(cx).with_terminal_workspace_for(
            self.session_id.as_deref(),
            |workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    f(&entry.terminal);
                }
            },
            cx,
        );
    }

    fn paste_to_terminal(&self, terminal_id: u64, text: &str, cx: &mut Context<Self>) {
        self.with_terminal_id(terminal_id, cx, |terminal| {
            let mode = terminal.mode();
            let text = prepare_terminal_paste(text, mode.contains(Mode::BRACKETED_PASTE));
            terminal.write_input(text.into_bytes());
        });
    }

    fn send_key_bar_key(&mut self, key: TerminalKey, cx: &mut Context<Self>) {
        let terminal = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| workspace.active().map(|entry| entry.terminal.clone()),
                cx,
            )
            .flatten();
        let Some(terminal) = terminal else {
            return;
        };
        let bytes = self.key_bar.update(cx, |bar, _| {
            bar.encode_key(
                key,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            )
        });
        if let Some(bytes) = bytes {
            terminal.write_input(bytes);
            self.note_input(cx);
        }
    }

    fn send_committed_text(&mut self, terminal_id: u64, text: &str, cx: &mut Context<Self>) {
        let terminal = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| {
                    workspace
                        .terminal(terminal_id)
                        .map(|entry| entry.terminal.clone())
                },
                cx,
            )
            .flatten();
        let Some(terminal) = terminal else {
            return;
        };
        let bytes = self.key_bar.update(cx, |bar, cx| {
            let bytes = bar.encode_text(
                text,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            );
            cx.notify();
            bytes
        });
        if !bytes.is_empty() {
            terminal.write_input(bytes);
        }
    }

    fn on_terminal_copy(
        &mut self,
        action: &TerminalCopy,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| {
                    workspace
                        .terminal(action.0)
                        .and_then(|entry| entry.terminal.selected_text())
                        .map(|selection| selection.text)
                },
                cx,
            )
            .flatten()
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn on_terminal_paste(
        &mut self,
        action: &TerminalPaste,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_to_terminal(action.0, &text, cx);
        }
    }

    fn on_terminal_select_all(
        &mut self,
        action: &TerminalSelectAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_terminal_id(action.0, cx, |terminal| terminal.select_all());
    }

    fn on_terminal_clear(
        &mut self,
        action: &TerminalClear,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_terminal_id(action.0, cx, |terminal| terminal.clear());
    }

    fn on_terminal_add_context(
        &mut self,
        action: &TerminalAddContext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store.update(cx, |store, _cx| {
            store.capture_terminal_selection_for(
                self.session_id.clone().unwrap_or_default(),
                action.0,
            )
        });
    }

    fn on_overflow(
        &mut self,
        action: &TerminalOverflow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store.update(cx, |store, cx| match action {
            TerminalOverflow::SplitHorizontal => store.split_terminal_for(
                self.session_id.clone().unwrap_or_default(),
                TerminalSplitDirection::Horizontal,
            ),
            TerminalOverflow::SplitVertical => store.split_terminal_for(
                self.session_id.clone().unwrap_or_default(),
                TerminalSplitDirection::Vertical,
            ),
            TerminalOverflow::Restart => {
                store.restart_terminal_for(self.session_id.clone().unwrap_or_default())
            }
            TerminalOverflow::Close => {
                store.close_terminal_panel_for(self.session_id.clone().unwrap_or_default(), cx)
            }
        });
    }

    fn on_tab_menu(&mut self, action: &TerminalTabMenu, _: &mut Window, cx: &mut Context<Self>) {
        let TerminalTabMenu { id, kind } = *action;
        self.workspace_store.update(cx, |store, cx| match kind {
            TerminalTabMenuKind::Restart => {
                store.activate_terminal_for(self.session_id.clone().unwrap_or_default(), id);
                store.restart_terminal_for(self.session_id.clone().unwrap_or_default());
            }
            TerminalTabMenuKind::SplitHorizontal => {
                store.activate_terminal_for(self.session_id.clone().unwrap_or_default(), id);
                store.split_terminal_for(
                    self.session_id.clone().unwrap_or_default(),
                    TerminalSplitDirection::Horizontal,
                );
            }
            TerminalTabMenuKind::SplitVertical => {
                store.activate_terminal_for(self.session_id.clone().unwrap_or_default(), id);
                store.split_terminal_for(
                    self.session_id.clone().unwrap_or_default(),
                    TerminalSplitDirection::Vertical,
                );
            }
            TerminalTabMenuKind::Close => {
                store.close_terminal_for(self.session_id.clone().unwrap_or_default(), id, cx)
            }
        });
    }

    /// Apply the bell and OSC 52 notices carried by the latest grid deltas.
    fn drain_terminal_notices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let notices = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| {
                    workspace
                        .terminals
                        .iter()
                        .map(|entry| (entry.id, entry.terminal.take_notices()))
                        .collect::<Vec<_>>()
                },
                cx,
            )
            .unwrap_or_default();
        for (terminal_id, (bell, clipboard)) in notices {
            if bell {
                self.bell_tabs.insert(terminal_id);
                window.play_system_bell();
            }
            // GPUI exposes the system clipboard but no primary-selection
            // clipboard. macOS has no primary selection, and on other platforms
            // substituting the system clipboard would be wrong.
            if let Some(clipboard) = clipboard.filter(|clipboard| !clipboard.selection) {
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard.text));
            }
        }
        window.invalidate_character_coordinates();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_input(cx);
        let keystroke = &event.keystroke;
        if let Some(shortcut) = terminal_clipboard_shortcut(
            &keystroke.key,
            keystroke.modifiers,
            cfg!(target_os = "macos"),
        ) {
            match shortcut {
                ClipboardShortcut::Copy => {
                    if let Some(text) = self
                        .workspace_store
                        .read(cx)
                        .with_terminal_workspace_for(
                            self.session_id.as_deref(),
                            |workspace| {
                                workspace
                                    .active()
                                    .and_then(|entry| entry.terminal.selected_text())
                                    .map(|selection| selection.text)
                            },
                            cx,
                        )
                        .flatten()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                // Left unhandled, the key makes the browser fire its paste
                // event, the only way a page receives the clipboard; it
                // arrives through `TerminalInputHandler::paste`.
                ClipboardShortcut::Paste if cfg!(target_family = "wasm") => return,
                ClipboardShortcut::Paste => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        && let Some(terminal_id) = self
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace_for(
                                self.session_id.as_deref(),
                                |workspace| workspace.active_id,
                                cx,
                            )
                            .flatten()
                    {
                        self.paste_to_terminal(terminal_id, &text, cx);
                    }
                }
            }
            cx.stop_propagation();
            return;
        }
        let mut handled = false;
        self.with_terminal(cx, |terminal| {
            if let Some(bytes) = terminal_key_bytes(
                keystroke,
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
            ) {
                terminal.write_input(bytes);
                handled = true;
            }
        });
        if handled {
            cx.stop_propagation();
        }
    }

    fn note_input(&mut self, cx: &mut Context<Self>) {
        self.restart_blink();
        if let Some(id) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| workspace.active_id,
                cx,
            )
            .flatten()
        {
            self.bell_tabs.remove(&id);
        }
        cx.notify();
    }

    /// Show the caret and start its blink over, as typing does.
    fn restart_blink(&mut self) {
        self.cursor_phase = true;
        self.blink = None;
    }

    /// Blink the caret only while the drawer is drawn with a caret that
    /// blinks: a tick that finds the drawer not drawn since the last one
    /// ends the blink, and the next render starts it again.
    fn sync_blink(&mut self, cx: &mut Context<Self>) {
        let blinks = self.caret_blinks.replace(false);
        let Some(interval) = blink_interval(cx).filter(|_| blinks) else {
            self.restart_blink();
            return;
        };
        self.blink_drawn = true;
        if self.blink.is_none() {
            self.blink = Self::blink(interval, cx);
        }
    }

    fn blink(mut interval: Duration, cx: &mut Context<Self>) -> Option<Task<()>> {
        Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let next = this.update(cx, |this, cx| {
                    if !std::mem::take(&mut this.blink_drawn) {
                        this.restart_blink();
                        return None;
                    }
                    let next = blink_interval(cx);
                    this.cursor_phase = next.is_none() || !this.cursor_phase;
                    cx.notify();
                    next
                });
                match next {
                    Ok(Some(next)) => interval = next,
                    _ => break,
                }
            }
        }))
    }

    /// The whole lines `pixels` of scrolling (positive toward the history)
    /// come to with what earlier scrolls left over.
    fn whole_lines(&mut self, terminal_id: u64, pixels: f32) -> i32 {
        let remainder = self.scroll_remainder.entry(terminal_id).or_default();
        let total = *remainder + pixels;
        let lines = (total / self.cell_height).trunc() as i32;
        *remainder = total - lines as f32 * self.cell_height;
        lines
    }

    fn on_scroll(
        &mut self,
        terminal_id: u64,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = f32::from(event.delta.pixel_delta(px(self.cell_height)).y);
        let lines = self.whole_lines(terminal_id, delta);
        if lines != 0 {
            self.workspace_store.read(cx).with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| {
                    if let Some(entry) = workspace.terminal(terminal_id) {
                        let mode = entry.terminal.mode();
                        let point = self
                            .grid_point_and_side(terminal_id, event.position)
                            .map(|((row, column), _)| GridPoint { row, column })
                            .unwrap_or(GridPoint { row: 0, column: 0 });
                        if mappings::routes_mouse(mode, event.modifiers.shift) {
                            if let Some(bytes) = mappings::scroll_report(
                                point,
                                lines,
                                term_modifiers(event.modifiers),
                                mode,
                            ) {
                                entry.terminal.write_raw(bytes);
                            }
                        } else if mode.contains(Mode::ALT_SCREEN)
                            && mode.contains(Mode::ALTERNATE_SCROLL)
                            && !event.modifiers.shift
                        {
                            entry.terminal.write_raw(mappings::alt_scroll(lines));
                        } else {
                            entry.terminal.scroll(lines);
                        }
                    }
                },
                cx,
            );
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_grid(
        &self,
        terminal_id: u64,
        state: &TerminalModel,
        register_input: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let marked_text = self
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == terminal_id)
            .map(|marked| marked.text.clone());
        let hovered_link = self
            .hovered_link
            .as_ref()
            .filter(|(id, _)| *id == terminal_id)
            .map(|(_, link)| link);
        let blink_phase = self.cursor_phase;
        let blinks = state.cursor_blinking()
            && marked_text.is_none()
            && state.cursor_shape() != CursorShape::Hidden
            && state.cursor().is_some();
        self.caret_blinks.set(self.caret_blinks.get() || blinks);
        let paint_data = layout_grid_cached(
            &mut self.row_layout_cache.borrow_mut(),
            terminal_id,
            state,
            palette,
            marked_text.as_deref(),
            hovered_link,
            self.terminal_focused,
            blink_phase,
        );
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;
        let cols = state.cols();
        let rows = state.rows();
        let focus_handle = self.focus_handle.clone();
        let drawer = cx.entity();
        let grid_bounds = self.grid_bounds.clone();

        canvas(
            |_bounds, _window, _cx| (),
            move |bounds, (), window, cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    let scale_factor = window.scale_factor();
                    let origin = snapped_grid_origin(bounds, scale_factor);
                    grid_bounds.borrow_mut().insert(
                        terminal_id,
                        GridGeometry {
                            bounds: Bounds::new(origin, bounds.size),
                            cols,
                            rows,
                            cell_width,
                            cell_height,
                        },
                    );
                    // The pane's own listeners hear the pointer only while it
                    // is over the pane; a selection being dragged follows it
                    // past the pane and the window, and ends wherever the
                    // button comes up.
                    window.on_mouse_event({
                        let drawer = drawer.clone();
                        move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase.capture()
                                && drawer.read(cx).selection_drag.pressed_on(terminal_id)
                            {
                                drawer.update(cx, |drawer, cx| {
                                    drawer.drag_selection(terminal_id, event, cx)
                                });
                            }
                        }
                    });
                    window.on_mouse_event({
                        let drawer = drawer.clone();
                        move |event: &MouseUpEvent, phase, _, cx| {
                            if phase.capture()
                                && event.button == MouseButton::Left
                                && drawer.read(cx).selection_drag.pressed_on(terminal_id)
                            {
                                drawer.update(cx, |drawer, cx| {
                                    drawer.end_selection_drag(terminal_id, cx)
                                });
                            }
                        }
                    });

                    let cursor_bounds = paint_terminal_grid(
                        bounds,
                        window,
                        cx,
                        &paint_data,
                        palette,
                        cell_width,
                        cell_height,
                        marked_text.is_none(),
                    );

                    if let Some(marked_text) = marked_text.as_ref().filter(|text| !text.is_empty())
                        && let Some(cursor_bounds) = cursor_bounds
                    {
                        let ime_run = TextRun {
                            len: marked_text.len(),
                            font: terminal_font(),
                            color: palette.foreground,
                            background_color: None,
                            strikethrough: None,
                            underline: Some(UnderlineStyle {
                                thickness: px(1.),
                                color: Some(palette.foreground),
                                wavy: false,
                            }),
                        };
                        let shaped = window.text_system().shape_line(
                            marked_text.clone().into(),
                            px(TERMINAL_FONT_SIZE),
                            &[ime_run],
                            None,
                        );
                        let covered_cells = (f32::from(shaped.width) / cell_width).ceil().max(1.);
                        let ime_bounds = Bounds::new(
                            cursor_bounds.origin,
                            size(px(covered_cells * cell_width), px(cell_height)),
                        );
                        window.paint_quad(fill(ime_bounds, palette.background));
                        let _ = shaped.paint(
                            cursor_bounds.origin,
                            px(cell_height),
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }

                    if register_input {
                        window.handle_input(
                            &focus_handle,
                            TerminalInputHandler {
                                drawer,
                                terminal_id,
                                cursor_bounds,
                                cell_width: px(cell_width),
                            },
                            cx,
                        );
                    }
                });
            },
        )
        // The pane is rarely an exact multiple of the cell metrics; it centers
        // this canvas, so the sub-cell remainder is split evenly on both axes
        // instead of accumulating at the right/bottom edge.
        .w(px(cols as f32 * cell_width))
        .h(px(rows as f32 * cell_height))
        .into_any_element()
    }

    fn render_terminal(
        &self,
        terminal_id: u64,
        terminals: &[(u64, Rc<crate::store::ClientTerminal>)],
        active_id: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(terminal) = terminals
            .iter()
            .find_map(|(id, terminal)| (*id == terminal_id).then_some(terminal))
        else {
            return div().into_any_element();
        };
        let register_input = active_id == Some(terminal_id);

        terminal.watch_grid(cx);
        let model = terminal.model();
        let label = model.title();
        let (exited, exit_code) = (model.exited(), model.exit_code());
        let has_selection = model.has_selection();
        let mut grid = v_flex().child(self.render_grid(terminal_id, &model, register_input, cx));
        drop(model);
        terminal.model_mut().clear_damage();
        // The exit line folds in under the grid.
        let status = exited.then(|| {
            exit_code
                .map(|code| crate::tr!("terminal.exited_code", code = code).into_owned())
                .unwrap_or_else(|| crate::tr!("terminal.exited").into_owned())
        });
        let (cell_height, muted) = (self.cell_height, cx.theme().muted_foreground);
        grid = grid.child(crate::motion::Sampled::new(
            move |window, cx| match crate::motion::shown(
                ("terminal-exited", terminal_id),
                status,
                SHOW,
                window,
                cx,
            ) {
                Some(shown) => shown.fold(
                    div()
                        .h(px(cell_height))
                        .text_color(muted)
                        .child(shown.value.clone()),
                ),
                None => gpui::Empty.into_any_element(),
            },
        ));

        // The add-to-context button is a pure overlay: it must never affect the
        // grid's geometry. Reserving space for it while a selection exists
        // resized the PTY mid-drag — rows jumped and blank lines appeared.
        let link_hovered = self
            .hovered_link
            .as_ref()
            .is_some_and(|(id, _)| *id == terminal_id);
        // PTY dimensions are deliberately NOT measured from this pane: its
        // percentage height does not resolve against the flex-sized drawer
        // body, so the pane hugs the grid's content height and any row count
        // derived from it is self-referential (frozen at its current value).
        // `render` measures the drawer body — which does track the real
        // height — and resizes every pane's PTY from there.
        div()
            .id(("terminal-grid", terminal_id))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .p(px(PANE_PADDING))
            .items_center()
            .justify_center()
            .when(link_hovered, |this| this.cursor_pointer())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_down(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_move(cx.listener(move |this, event, window, cx| {
                this.terminal_mouse_move(terminal_id, event, window, cx)
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, event, window, cx| {
                    this.terminal_mouse_up(terminal_id, event, window, cx)
                }),
            )
            .on_scroll(cx.listener(move |this, event, window, cx| {
                this.on_scroll(terminal_id, event, window, cx)
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                if paths.paths().is_empty() {
                    return;
                }
                this.focus_handle.focus(window, cx);
                let quoted = paths
                    .paths()
                    .iter()
                    .map(|path| shell_quote(&path.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(" ");
                this.paste_to_terminal(terminal_id, &format!(" {quoted} "), cx);
            }))
            .child(grid)
            // The action on a selection fades in over the grid's corner as
            // the selection is made, and out as it is cleared.
            .child({
                let button = Button::new(("terminal-add-context", terminal_id))
                    .small()
                    .label(crate::tr!("terminal.add_context"))
                    .tooltip(format!("{} · {}", label, crate::tr!("terminal.selection")))
                    // The terminal must keep the selection this action captures.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.capture_terminal_selection_for(
                                this.session_id.clone().unwrap_or_default(),
                                terminal_id,
                            )
                        });
                    }));
                crate::motion::Sampled::new(move |window, cx| {
                    match crate::motion::shown(
                        ("terminal-add-context-shown", terminal_id),
                        has_selection.then_some(()),
                        SHOW,
                        window,
                        cx,
                    ) {
                        Some(shown) => div()
                            .absolute()
                            .block_mouse_except_scroll()
                            .right(px(PANE_PADDING))
                            .top(px(PANE_PADDING))
                            .child(shown.fade(button))
                            .into_any_element(),
                        None => gpui::Empty.into_any_element(),
                    }
                })
            })
            .context_menu({
                let workspace_store = self.workspace_store.clone();
                let session_id = self.session_id.clone();
                move |menu, _window, cx| {
                    let has_selection = workspace_store
                        .read(cx)
                        .with_terminal_workspace_for(
                            session_id.as_deref(),
                            |workspace| {
                                workspace
                                    .terminal(terminal_id)
                                    .and_then(|entry| entry.terminal.selected_text())
                                    .is_some()
                            },
                            cx,
                        )
                        .unwrap_or(false);
                    menu.menu_with_enable(
                        crate::tr!("terminal.copy").into_owned(),
                        Box::new(TerminalCopy(terminal_id)),
                        has_selection,
                    )
                    // A browser hands the clipboard only to its own paste
                    // event, never to a menu the page draws.
                    .when(!cfg!(target_family = "wasm"), |menu| {
                        menu.menu(
                            crate::tr!("terminal.paste").into_owned(),
                            Box::new(TerminalPaste(terminal_id)),
                        )
                    })
                    .menu(
                        crate::tr!("terminal.select_all").into_owned(),
                        Box::new(TerminalSelectAll(terminal_id)),
                    )
                    .menu(
                        crate::tr!("terminal.clear").into_owned(),
                        Box::new(TerminalClear(terminal_id)),
                    )
                    .separator()
                    .menu_with_enable(
                        crate::tr!("terminal.add_context").into_owned(),
                        Box::new(TerminalAddContext(terminal_id)),
                        has_selection,
                    )
                }
            })
            .into_any_element()
    }
}

impl Focusable for TerminalDrawer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalDrawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.terminal_focused = self.focus_handle.is_focused(window);

        // PTY dimensions and mouse hit-testing use the exact advance and
        // vertical metrics of the same resolved face used by StyledText.
        let shaped_cell = window.text_system().shape_line(
            "MMMMMMMMMM".into(),
            px(TERMINAL_FONT_SIZE),
            &[TextRun {
                len: 10,
                font: terminal_font(),
                color: cx.theme().foreground,
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );
        self.cell_width = f32::from(shaped_cell.width) / 10.;
        self.cell_height = f32::from(shaped_cell.ascent + shaped_cell.descent)
            .ceil()
            .max(TERMINAL_FONT_SIZE + 2.);
        let (terminals, active_id, active_split) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace_for(
                self.session_id.as_deref(),
                |workspace| {
                    (
                        workspace
                            .terminals
                            .iter()
                            .map(|entry| (entry.id, entry.terminal.clone()))
                            .collect::<Vec<_>>(),
                        workspace.active_id,
                        workspace.active_id.and_then(|id| workspace.split_for(id)),
                    )
                },
                cx,
            )
            .unwrap_or_default();
        let tabs = terminals
            .iter()
            .map(|(id, terminal)| {
                terminal.watch_label(cx);
                (
                    *id,
                    terminal.label(),
                    terminal.exited(),
                    self.bell_tabs.contains(id),
                )
            })
            .collect::<Vec<_>>();

        if self
            .marked_text
            .as_ref()
            .is_some_and(|marked| Some(marked.terminal_id) != active_id)
        {
            self.marked_text = None;
        }

        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut tab_strip = TabBar::new("terminal-tab-list")
            .underline()
            .hairline(false)
            .compact(compact)
            .label_max_w(px(160.))
            .aria_label(crate::tr!("terminal.tabs"))
            .flex_1()
            .min_w_0()
            .mb(px(-1.));
        for (id, label, _, bell) in &tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            tab_strip = tab_strip.item(
                TabItem::new(("terminal-tab", id), label.clone())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.activate_terminal_for(
                                this.session_id.clone().unwrap_or_default(),
                                id,
                            )
                        });
                    }))
                    .trailing({
                        let drawer = cx.entity().downgrade();
                        let bell = *bell;
                        move |window, cx| {
                            let drawer = drawer.clone();
                            h_flex()
                                // A bell's dot folds in before the close button,
                                // and away once the terminal is answered.
                                .children(
                                    crate::motion::shown(
                                        ("terminal-bell", id),
                                        bell.then_some(()),
                                        SHOW,
                                        window,
                                        cx,
                                    )
                                    .map(|shown| {
                                        shown.fold_across(
                                            div()
                                                .pr(cx.theme().tokens.spacing.xxs)
                                                .text_token(&cx.theme().tokens.typography.xs)
                                                .text_color(cx.theme().warning)
                                                .child("●"),
                                        )
                                    }),
                                )
                                .child(
                                    Button::new(("terminal-tab-close", close_id))
                                        .ghost()
                                        .compact()
                                        .xsmall()
                                        .icon(
                                            Icon::new(IconName::Close)
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                        .tooltip(crate::tr!("terminal.close_tab"))
                                        .on_click(move |_, _, cx| {
                                            _ = drawer.update(cx, |this, cx| {
                                                this.workspace_store.update(cx, |store, cx| {
                                                    store.close_terminal_for(
                                                        this.session_id.clone().unwrap_or_default(),
                                                        close_id,
                                                        cx,
                                                    )
                                                });
                                            });
                                        }),
                                )
                                .into_any_element()
                        }
                    })
                    .context_menu({
                        let can_split = self
                            .workspace_store
                            .read(cx)
                            .session_status_for(self.session_id.as_deref(), cx)
                            .is_some_and(|status| status.terminal_split_available);
                        move |menu, _, _| {
                            let item = |kind| Box::new(TerminalTabMenu { id, kind });
                            menu.menu(
                                crate::tr!("terminal.restart").into_owned(),
                                item(TerminalTabMenuKind::Restart),
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_horizontal").into_owned(),
                                item(TerminalTabMenuKind::SplitHorizontal),
                                can_split,
                            )
                            .menu_with_enable(
                                crate::tr!("terminal.split_vertical").into_owned(),
                                item(TerminalTabMenuKind::SplitVertical),
                                can_split,
                            )
                            .separator()
                            .menu(
                                crate::tr!("terminal.close_tab").into_owned(),
                                item(TerminalTabMenuKind::Close),
                            )
                        }
                    }),
            );
        }

        let at_limit = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .is_some_and(|status| status.terminal_limit_reached);
        let can_split = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .is_some_and(|status| status.terminal_split_available);
        let active_exited = tabs
            .iter()
            .any(|(id, _, exited, _)| Some(*id) == active_id && *exited);
        // A compact page gives the drawer one toolbar row on the page inset:
        // the tab strip, a new-terminal target, and everything else in an
        // overflow menu rather than five dense controls fighting for 393pt.
        let new_tooltip = if at_limit {
            crate::tr!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
        } else {
            crate::tr!("terminal.new")
        };
        let mut header = material::bar_divider_below(
            material::bar("terminal-toolbar", material::Bar::Toolbar, compact, cx),
            cx,
        )
        .child(tab_strip);
        if compact {
            header = header
                .child(
                    material::toolbar_icon_button(
                        "terminal-new",
                        IconName::Plus,
                        new_tooltip,
                        true,
                    )
                    .flex_none()
                    .disabled(at_limit)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store.update(cx, |store, _cx| {
                            store.new_terminal_for(this.session_id.clone().unwrap_or_default())
                        });
                    })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-overflow",
                        IconName::Ellipsis,
                        crate::tr!("mobile.more_actions"),
                        true,
                    )
                    .flex_none()
                    .dropdown_menu(move |menu, _, _| {
                        menu.menu_with_enable(
                            crate::tr!("terminal.split_horizontal").into_owned(),
                            Box::new(TerminalOverflow::SplitHorizontal),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.split_vertical").into_owned(),
                            Box::new(TerminalOverflow::SplitVertical),
                            can_split,
                        )
                        .menu_with_enable(
                            crate::tr!("terminal.restart").into_owned(),
                            Box::new(TerminalOverflow::Restart),
                            active_exited,
                        )
                        .separator()
                        .menu(
                            crate::tr!("terminal.close").into_owned(),
                            Box::new(TerminalOverflow::Close),
                        )
                    }),
                );
        } else {
            let gap = cx.theme().tokens.spacing.xs;
            let restart = crate::motion::shown(
                "terminal-restart-shown",
                active_exited.then_some(()),
                SHOW,
                window,
                cx,
            );
            header = header
                .when_some(restart, |this, shown| {
                    this.child(
                        shown.fold_across_gap(
                            gap,
                            Button::new("terminal-restart")
                                .flex_none()
                                .ghost()
                                .small()
                                .label(crate::tr!("terminal.restart"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.workspace_store.update(cx, |store, _cx| {
                                        store.restart_terminal_for(
                                            this.session_id.clone().unwrap_or_default(),
                                        )
                                    });
                                })),
                        ),
                    )
                })
                .child(
                    Button::new("terminal-split-horizontal")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↔")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_horizontal"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    TerminalSplitDirection::Horizontal,
                                )
                            });
                        })),
                )
                .child(
                    Button::new("terminal-split-vertical")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("↕")
                        .disabled(!can_split)
                        .tooltip(crate::tr!("terminal.split_vertical"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.split_terminal_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    TerminalSplitDirection::Vertical,
                                )
                            });
                        })),
                )
                .child(
                    Button::new("terminal-new")
                        .flex_none()
                        .ghost()
                        .small()
                        .compact()
                        .label("+")
                        .disabled(at_limit)
                        .tooltip(new_tooltip)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.new_terminal_for(this.session_id.clone().unwrap_or_default())
                            });
                        })),
                )
                .child(
                    material::toolbar_icon_button(
                        "terminal-close-drawer",
                        IconName::Close,
                        crate::tr!("terminal.close"),
                        false,
                    )
                    .flex_none()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store.update(cx, |store, cx| {
                            store.close_terminal_panel_for(
                                this.session_id.clone().unwrap_or_default(),
                                cx,
                            )
                        });
                    })),
                );
        }

        // Splitting opens the second pane out of nothing beside or under the
        // first, which gives way; closing the split shrinks it away again.
        if active_split.is_some() {
            self.last_split = active_split;
        }
        let direction = active_split
            .or(self.last_split)
            .map_or(TerminalSplitDirection::Horizontal, |split| split.direction);
        let axis = match direction {
            TerminalSplitDirection::Horizontal => gpui::Axis::Horizontal,
            TerminalSplitDirection::Vertical => gpui::Axis::Vertical,
        };
        self.split_glide.turn(axis);
        let axis_ix = usize::from(axis == gpui::Axis::Vertical);
        let reduced = cx.reduce_motion();
        let state = self.split_states[axis_ix].clone();
        let body_size = self.body_size.get();
        let extent = match axis {
            gpui::Axis::Horizontal => body_size.width,
            gpui::Axis::Vertical => body_size.height,
        };
        // Both panes are sized, so the glide lands each where it was headed
        // (the 1px handle between them).
        let half = ((extent - px(1.)) / 2.).max(px(0.));
        let rest = if active_split.is_some() {
            (extent - px(1.) - half).max(px(0.))
        } else {
            extent
        };
        let frame = self.split_glide.sample(
            &[
                crate::motion::resize_glide::Panel::sized(rest, true),
                crate::motion::resize_glide::Panel::sized(half, active_split.is_some()),
            ],
            !reduced,
            &state,
            window,
            cx,
        );
        let (first, second) = match (active_split, self.last_split) {
            (Some(split), _) => (Some(split.first), Some(split.second)),
            (None, Some(last)) => (
                active_id,
                Some(if active_id == Some(last.first) {
                    last.second
                } else {
                    last.first
                }),
            ),
            (None, None) => (active_id, None),
        };
        let paired = frame.mounted(1);
        // Each pane of a pair carries a 2px gutter toward the handle.
        let gutter = |pane: AnyElement, leading: bool| {
            div()
                .debug_selector(move || {
                    if leading {
                        "terminal-pane-first".into()
                    } else {
                        "terminal-pane-second".into()
                    }
                })
                .size_full()
                .when(paired, |pane| match (axis, leading) {
                    (gpui::Axis::Horizontal, true) => pane.pr(px(2.)),
                    (gpui::Axis::Horizontal, false) => pane.pl(px(2.)),
                    (gpui::Axis::Vertical, true) => pane.pb(px(2.)),
                    (gpui::Axis::Vertical, false) => pane.pt(px(2.)),
                })
                .child(pane)
                .into_any_element()
        };
        // "Starting…" fades out under the terminal once it has one.
        let starting = crate::motion::CrossFade::new(
            "terminal-starting",
            usize::from(first.is_some()),
            2,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        if starting.shows(0) {
            layers.push((
                0,
                div()
                    .p(cx.theme().tokens.spacing.md)
                    .child(crate::tr!("terminal.starting"))
                    .into_any_element(),
            ));
        }
        if let Some(id) = first {
            layers.push((
                1,
                div()
                    .size_full()
                    .child(self.render_terminal(id, &terminals, active_id, cx))
                    .into_any_element(),
            ));
        }
        let first = starting
            .fill()
            .render(layers)
            .size_full()
            .into_any_element();
        let first = resizable_panel()
            .size_range(frame.range(0, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
            .child(frame.hold(0, gutter(first, true)));
        let second = second.filter(|_| paired).map(|id| {
            let pane = self.render_terminal(id, &terminals, active_id, cx);
            // No size of its own: at rest the group splits the body evenly
            // until a drag, as the glide lands it.
            resizable_panel()
                .size_range(frame.range(1, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
                .child(frame.hold(1, gutter(pane, false)))
        });
        let body: AnyElement = match axis {
            gpui::Axis::Horizontal => h_resizable("terminal-split-h"),
            gpui::Axis::Vertical => v_resizable("terminal-split-v"),
        }
        .with_state(&state)
        .child(first)
        // Not `children`: the group's replaces the panels added before.
        .when_some(second, |group, second| group.child(second))
        .into_any_element();

        let drawer = v_flex()
            .id(self.scene.id("terminal-scene"))
            .size_full()
            .min_h_0()
            .font_family(TERMINAL_FONT_FAMILY)
            .text_size(px(TERMINAL_FONT_SIZE))
            .on_action(cx.listener(Self::on_terminal_copy))
            .on_action(cx.listener(Self::on_terminal_paste))
            .on_action(cx.listener(Self::on_terminal_select_all))
            .on_action(cx.listener(Self::on_terminal_clear))
            .on_action(cx.listener(Self::on_terminal_add_context))
            .on_action(cx.listener(Self::on_overflow))
            .on_action(cx.listener(Self::on_tab_menu))
            .child(header)
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "terminal-content",
                    Role::Terminal,
                    crate::tr!("terminal.content"),
                    cx,
                )
                .key_context(CONTEXT)
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down))
                // The grid is transparent; the terminal surface is opaque.
                .bg(cx.theme().popover)
                .flex_1()
                .min_h_0()
                // Resize every pane's PTY from this element's bounds. This is
                // the innermost element whose laid-out height reliably tracks
                // the drawer; the panes themselves hug their grid content
                // (percentage heights fail to resolve below this point).
                .on_prepaint({
                    let workspace_store = self.workspace_store.clone();
                    let (cell_width, cell_height) = (self.cell_width, self.cell_height);
                    let split_state = state.clone();
                    let body_size = self.body_size.clone();
                    let session_id = self.session_id.clone();
                    move |bounds, window, cx| {
                        body_size.set(bounds.size);
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        // Each pane shown, with the room it has.
                        let panes: [Option<(u64, f32, f32)>; 2] = match active_split {
                            None => [active_id.map(|id| (id, width, height)), None],
                            Some(split) => {
                                // Panel sizes from the resize handle; before any
                                // drag the group splits the axis evenly (1px
                                // handle). Each pane also carries a 2px gutter
                                // (`pr`/`pl`/`pb`/`pt`) toward the handle.
                                let sizes = split_state.read(cx).sizes();
                                let axis = match split.direction {
                                    TerminalSplitDirection::Horizontal => width,
                                    TerminalSplitDirection::Vertical => height,
                                };
                                let (first, second) = match sizes.as_slice() {
                                    [first, second] => (f32::from(*first), f32::from(*second)),
                                    _ => ((axis - 1.) / 2., (axis - 1.) / 2.),
                                };
                                match split.direction {
                                    TerminalSplitDirection::Horizontal => [
                                        Some((split.first, first - 2., height)),
                                        Some((split.second, second - 2., height)),
                                    ],
                                    TerminalSplitDirection::Vertical => [
                                        Some((split.first, width, first - 2.)),
                                        Some((split.second, width, second - 2.)),
                                    ],
                                }
                            }
                        };
                        let scale_factor = window.scale_factor();
                        let cell_width_px = (cell_width * scale_factor).round().max(1.) as u32;
                        let cell_height_px = (cell_height * scale_factor).round().max(1.) as u32;
                        workspace_store.read(cx).with_terminal_workspace_for(
                            session_id.as_deref(),
                            |workspace| {
                                for (terminal_id, pane_width, pane_height) in
                                    panes.into_iter().flatten()
                                {
                                    let Some(entry) = workspace.terminal(terminal_id) else {
                                        continue;
                                    };
                                    let cols = ((pane_width - 2. * PANE_PADDING) / cell_width)
                                        .floor()
                                        .max(2.)
                                        as usize;
                                    let rows = ((pane_height - 2. * PANE_PADDING) / cell_height)
                                        .floor()
                                        .max(2.)
                                        as usize;
                                    entry.terminal.resize_with_cell_size(
                                        cols,
                                        rows,
                                        cell_width_px,
                                        cell_height_px,
                                    );
                                }
                            },
                            cx,
                        );
                    }
                })
                .child(body),
            )
            // The key bar rises with the keyboard as the terminal takes focus
            // and folds away with it.
            .children(
                crate::motion::shown(
                    "terminal-key-bar-shown",
                    should_show_terminal_key_bar(self.terminal_focused, cx).then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| shown.fold(self.key_bar.clone())),
            );
        self.sync_blink(cx);
        drawer
    }
}

/// The glide of the drawer's split in `scene`.
/// How long each half of a caret blink lasts, from the policy gpui-base's
/// theme sets for every caret; `None` when carets stay shown.
fn blink_interval(cx: &App) -> Option<Duration> {
    cx.try_global::<gpui_base::Theme>()
        .map(|theme| theme.cursor_blink)
        .unwrap_or_default()
        .interval()
}

fn split_glide(scene: &crate::motion::Epoch) -> crate::motion::resize_glide::ResizeGlide {
    crate::motion::resize_glide::ResizeGlide::new("terminal-split-glide", gpui::Axis::Horizontal)
        .in_scene(scene)
}

fn grid_point_and_side(
    x: f32,
    y: f32,
    cols: usize,
    rows: usize,
    cell_width: f32,
    cell_height: f32,
) -> ((usize, usize), SelectionSide) {
    let last_column = cols.saturating_sub(1);
    let mut column = (x / cell_width) as usize;
    let cell_x = x.max(0.) % cell_width;
    let mut side = if cell_x > cell_width / 2. {
        SelectionSide::Right
    } else {
        SelectionSide::Left
    };
    if column > last_column {
        column = last_column;
        side = SelectionSide::Right;
    }

    let bottommost_row = rows.saturating_sub(1) as i32;
    let mut row = (y / cell_height) as i32;
    if row > bottommost_row {
        row = bottommost_row;
        side = SelectionSide::Right;
    } else if y < 0. {
        side = SelectionSide::Left;
    }

    ((row.max(0) as usize, column.min(last_column)), side)
}

fn selection_drag_started(dx: f32, dy: f32) -> bool {
    dx.hypot(dy) > SELECTION_DRAG_THRESHOLD
}

pub(crate) fn terminal_font() -> gpui::Font {
    let mut terminal_font = font(TERMINAL_FONT_FAMILY);
    terminal_font.features = FontFeatures::disable_ligatures();
    let symbols: &[&str] = if cfg!(target_family = "wasm") {
        &["Tcode Terminal Symbols"]
    } else {
        &[]
    };
    let fallbacks: Vec<String> = symbols
        .iter()
        .chain(crate::i18n::cjk_fallback_families())
        .map(|&family| family.into())
        .collect();
    terminal_font.fallbacks =
        (!fallbacks.is_empty()).then(|| gpui::FontFallbacks::from_fonts(fallbacks));
    terminal_font
}

pub(crate) fn terminal_color(color: TerminalColor, palette: TerminalPalette) -> Hsla {
    match color {
        TerminalColor::Foreground | TerminalColor::Cursor => palette.foreground,
        TerminalColor::Background => palette.background,
        TerminalColor::Rgb { r, g, b } => {
            rgb((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)).into()
        }
        TerminalColor::Indexed(index) => {
            const ANSI: [u32; 16] = [
                0x1f2329, 0xe45649, 0x50a14f, 0xc18401, 0x4078f2, 0xa626a4, 0x0184bc, 0xabb2bf,
                0x5c6370, 0xff616e, 0x7bc275, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ];
            if index < 16 {
                return rgb(ANSI[index as usize]).into();
            }
            if index < 232 {
                let n = index - 16;
                let component = |v: u8| if v == 0 { 0 } else { 55 + 40 * u32::from(v) };
                let r = component(n / 36);
                let g = component((n % 36) / 6);
                let b = component(n % 6);
                return rgb((r << 16) | (g << 8) | b).into();
            }
            let gray = 8 + 10 * u32::from(index - 232);
            rgb((gray << 16) | (gray << 8) | gray).into()
        }
    }
}

mod grid;
mod input;
mod mouse;
pub(crate) use grid::*;
use input::*;

#[cfg(test)]
mod tests;
