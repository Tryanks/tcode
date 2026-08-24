use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::menu::ContextMenuExt as _;
use crate::{icon::IconName, sizing::Sizable as _};
use gpui::{
    Action, AnyElement, App, Bounds, ClipboardItem, ContentMask, Context, Entity, ExternalPaths,
    FocusHandle, Focusable, FontFeatures, FontStyle, FontWeight, Hsla, InputHandler,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, RenderImage, Role,
    ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _, Task, TextAlign, TextRun,
    UTF16Selection, UnderlineStyle, Window, canvas, div, fill, font, point,
    prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_base::{ElementExt as _, h_flex, h_resizable, resizable_panel, v_flex, v_resizable};
use term::{
    HyperlinkMatch, SelectionKind, SelectionSide, TermEvent, TermSnapshot,
    graphics::{
        AtlasPlacement, ColorType, GraphicData, GraphicOverlay, IncompletePlacement,
        KittyPlacement, OverlayViewport, PLACEHOLDER, PlaceholderRun, UpdateQueues,
        VirtualPlacement, atlas_image_key, atlas_overlay_geometry, clip_overlay_to_rect,
        compute_run_geometry, kitty_image_key, kitty_overlay_geometry,
    },
    mappings::{self, GridPoint, Modifiers as TermModifiers, MouseButton as TermMouseButton},
    rio_vt::{
        ansi::CursorShape,
        clipboard::ClipboardType,
        config::colors::{AnsiColor, NamedColor},
        crosswords::{Mode, square::Wide, style::StyleFlags},
    },
};

use crate::{
    material,
    store::{StoreChange, TopicKind, WorkspaceStore, observe_store_topics},
};
use tcode_core::ui::{MAX_TERMINALS_PER_SESSION, TerminalSplitDirection};

pub(crate) const TERMINAL_FONT_SIZE: f32 = 13.;
pub(crate) const TERMINAL_CELL_WIDTH: f32 = 7.83;
pub(crate) const TERMINAL_CELL_HEIGHT: f32 = 17.;
#[cfg(target_os = "macos")]
const TERMINAL_FONT_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
const TERMINAL_FONT_FAMILY: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const TERMINAL_FONT_FAMILY: &str = "Lilex";
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
        left_pressed: bool,
    ) -> SelectionDragAction {
        if !left_pressed
            || !self
                .mouse_down
                .is_some_and(|(mouse_id, _)| mouse_id == terminal_id)
        {
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

    fn should_report_mouse_move(&mut self, terminal_id: u64, point: (usize, usize)) -> bool {
        if self.last_reported_point.get(&terminal_id) == Some(&point) {
            false
        } else {
            self.last_reported_point.insert(terminal_id, point);
            true
        }
    }
}

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

struct TerminalEventSubscription {
    receiver: smol::channel::Receiver<TermEvent>,
    _task: Task<()>,
}

#[derive(Clone)]
struct MarkedText {
    terminal_id: u64,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridTextStyle {
    pub(crate) fg: AnsiColor,
    pub(crate) bg: AnsiColor,
    bold: bool,
    italic: bool,
    underline: bool,
    underline_wavy: bool,
    selected: bool,
    cursor: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchedTextRun {
    pub(crate) row: usize,
    pub(crate) start_col: usize,
    pub(crate) text: String,
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
    row: usize,
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
    pub(crate) text_runs: Vec<BatchedTextRun>,
    backgrounds: Vec<BackgroundRect>,
    selections: Vec<BackgroundRect>,
    cursor: Option<CursorPaint>,
}

#[derive(Clone, Default)]
struct RowPaintData {
    text_runs: Vec<BatchedTextRun>,
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
    selection: Option<term::rio_vt::selection::SelectionRange>,
    hovered_link: Option<((usize, usize), (usize, usize))>,
    cursor: Option<CursorRowKey>,
}

#[derive(Clone)]
struct CachedRowLayout {
    key: RowLayoutKey,
    paint: RowPaintData,
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

#[derive(Clone)]
struct TerminalImage {
    image: Arc<RenderImage>,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
struct VirtualPlaceholderPaint {
    run: PlaceholderRun,
    screen_line: usize,
    start_screen_col: usize,
}

struct OrderedGraphicOverlay {
    overlay: GraphicOverlay,
    protocol_order: u8,
    placement_order: u32,
}

pub struct TerminalDrawer {
    workspace_store: Entity<WorkspaceStore>,
    focus_handle: FocusHandle,
    grid_bounds: Rc<RefCell<HashMap<u64, GridGeometry>>>,
    /// Last-known panel sizes of the active split (from its resize handle),
    /// used to apportion the drawer body between the two panes when resizing
    /// their PTYs. Empty means "no drag yet": assume an even split.
    split_sizes: Rc<RefCell<Vec<f32>>>,
    row_layout_cache: RefCell<HashMap<u64, TerminalGridCache>>,
    image_registry: RefCell<HashMap<u64, HashMap<u64, TerminalImage>>>,
    cell_width: f32,
    cell_height: f32,
    scroll_remainder: HashMap<u64, f32>,
    selection_drag: SelectionDrag,
    _focus_subscriptions: Vec<gpui::Subscription>,
    event_subscriptions: HashMap<u64, TerminalEventSubscription>,
    marked_text: Option<MarkedText>,
    bell_tabs: HashSet<u64>,
    hovered_link: Option<(u64, HyperlinkMatch)>,
    pressed_link: Option<(u64, String)>,
    last_link_hover: Option<Instant>,
    cursor_phase: bool,
    last_input: Instant,
    terminal_focused: bool,
    current_size: Option<(f32, f32)>,
    _blink_task: Option<Task<()>>,
    _store_subscriptions: Vec<gpui::Subscription>,
}

impl TerminalDrawer {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_observer = observe_store_topics(
            &workspace_store,
            &[TopicKind::ActiveSession, TopicKind::SessionStatus],
            cx,
        );
        let topology_observer = cx.subscribe_in(
            &workspace_store,
            window,
            |this, _, change: &StoreChange, window, cx| {
                if matches!(
                    change.topic,
                    TopicKind::ActiveSession | TopicKind::SessionStatus
                ) {
                    this.sync_event_subscriptions(window, cx);
                }
            },
        );
        let focus_handle = cx.focus_handle().tab_index(0).tab_stop(true);
        let focus_store = workspace_store.clone();
        let focus_in = window.on_focus_in(&focus_handle, cx, move |_, cx| {
            focus_store.read(cx).with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active()
                    && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                {
                    entry.terminal.write_raw(b"\x1b[I".to_vec());
                }
            });
        });
        let focus_store = workspace_store.clone();
        let focus_out = window.on_focus_out(&focus_handle, cx, move |_, _, cx| {
            focus_store.read(cx).with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active()
                    && entry.terminal.mode().contains(Mode::FOCUS_IN_OUT)
                {
                    entry.terminal.write_raw(b"\x1b[O".to_vec());
                }
            });
        });
        #[cfg(not(test))]
        let blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(Duration::from_millis(500)).await;
                if this
                    .update(cx, |this, cx| {
                        this.cursor_phase = !this.cursor_phase;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        // GPUI's deterministic test scheduler rejects the async-io timer's
        // background-thread wakeup during window teardown.
        #[cfg(test)]
        let blink_task = None;
        let mut drawer = Self {
            workspace_store,
            focus_handle,
            grid_bounds: Rc::new(RefCell::new(HashMap::new())),
            split_sizes: Rc::new(RefCell::new(Vec::new())),
            row_layout_cache: RefCell::new(HashMap::new()),
            image_registry: RefCell::new(HashMap::new()),
            cell_width: TERMINAL_CELL_WIDTH,
            cell_height: TERMINAL_CELL_HEIGHT,
            scroll_remainder: HashMap::new(),
            selection_drag: SelectionDrag::default(),
            _focus_subscriptions: vec![focus_in, focus_out],
            event_subscriptions: HashMap::new(),
            marked_text: None,
            bell_tabs: HashSet::new(),
            hovered_link: None,
            pressed_link: None,
            last_link_hover: None,
            cursor_phase: true,
            last_input: Instant::now(),
            terminal_focused: false,
            current_size: None,
            _blink_task: blink_task,
            _store_subscriptions: vec![store_observer, topology_observer],
        };
        drawer.sync_event_subscriptions(window, cx);
        drawer
    }

    pub fn is_size(&self, width: f32, height: f32) -> bool {
        self.current_size == Some((width, height))
    }

    pub fn resize(&mut self, width: f32, height: f32, cx: &mut Context<Self>) {
        if self.is_size(width, height) {
            return;
        }
        self.current_size = Some((width, height));
        self.workspace_store
            .update(cx, |store, cx| store.set_terminal_height(height, cx));
    }

    fn with_terminal(&self, cx: &mut Context<Self>, f: impl FnOnce(&term::Terminal)) {
        self.workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.active() {
                    f(&entry.terminal);
                }
            });
    }

    fn with_terminal_id(
        &self,
        terminal_id: u64,
        cx: &mut Context<Self>,
        f: impl FnOnce(&term::Terminal),
    ) {
        self.workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    f(&entry.terminal);
                }
            });
    }

    fn paste_to_terminal(&self, terminal_id: u64, text: &str, cx: &mut Context<Self>) {
        self.with_terminal_id(terminal_id, cx, |terminal| {
            let mode = terminal.mode();
            let text = prepare_terminal_paste(text, mode.contains(Mode::BRACKETED_PASTE));
            terminal.write_input(text.into_bytes());
        });
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
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminal(action.0)
                    .and_then(|entry| entry.terminal.selected_text())
                    .map(|selection| selection.text)
            })
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
        self.with_terminal_id(action.0, cx, term::Terminal::select_all);
    }

    fn on_terminal_clear(
        &mut self,
        action: &TerminalClear,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_terminal_id(action.0, cx, term::Terminal::clear);
    }

    fn on_terminal_add_context(
        &mut self,
        action: &TerminalAddContext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store
            .update(cx, |store, _cx| store.capture_terminal_selection(action.0));
    }

    /// Keep one gpui-side drain task per live PTY. Terminal restarts retain the
    /// tab id, so channel identity (rather than just the id) determines whether
    /// an existing subscription is still valid.
    fn sync_event_subscriptions(&mut self, window: &Window, cx: &mut Context<Self>) {
        let streams = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace
                    .terminals
                    .iter()
                    .map(|entry| (entry.id, entry.terminal.events()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        self.event_subscriptions
            .retain(|id, _| streams.iter().any(|(stream_id, _)| stream_id == id));
        self.image_registry
            .borrow_mut()
            .retain(|id, _| streams.iter().any(|(stream_id, _)| stream_id == id));

        for (terminal_id, receiver) in streams {
            let already_subscribed = self
                .event_subscriptions
                .get(&terminal_id)
                .is_some_and(|subscription| subscription.receiver.same_channel(&receiver));
            if already_subscribed {
                continue;
            }
            if self.event_subscriptions.contains_key(&terminal_id) {
                // A restart keeps the tab id but creates a new terminal. Its
                // image namespace starts empty just like a newly opened tab.
                self.image_registry.borrow_mut().remove(&terminal_id);
            }

            let task_receiver = receiver.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                while let Ok(first_event) = task_receiver.recv().await {
                    // The first event is visible immediately. A short trailing
                    // window then collapses Wakeup floods from large PTY writes.
                    if this
                        .update_in(cx, |this, window, cx| {
                            match &first_event {
                                TermEvent::Bell => {
                                    this.bell_tabs.insert(terminal_id);
                                    window.play_system_bell();
                                }
                                TermEvent::ClipboardStore {
                                    kind: ClipboardType::Clipboard,
                                    text,
                                } => {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                }
                                TermEvent::ClipboardStore {
                                    kind: ClipboardType::Selection,
                                    ..
                                } => {
                                    // GPUI exposes the system clipboard but no primary-selection
                                    // clipboard. macOS has no primary selection, and on other
                                    // platforms substituting the system clipboard would be wrong.
                                }
                                _ => {}
                            }
                            window.invalidate_character_coordinates();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }

                    let deadline = Instant::now() + Duration::from_millis(4);
                    let mut saw_batched_event = false;
                    let mut non_wakeup_events = 0;
                    loop {
                        let next = smol::future::or(
                            async {
                                smol::Timer::at(deadline).await;
                                None
                            },
                            async { Some(task_receiver.recv().await) },
                        )
                        .await;
                        let Some(next) = next else {
                            break;
                        };
                        let Ok(event) = next else {
                            return;
                        };
                        saw_batched_event = true;
                        match &event {
                            TermEvent::Bell => {
                                let _ = this.update_in(cx, |this, window, _| {
                                    this.bell_tabs.insert(terminal_id);
                                    window.play_system_bell();
                                });
                            }
                            TermEvent::ClipboardStore {
                                kind: ClipboardType::Clipboard,
                                text,
                            } => {
                                let text = text.clone();
                                let _ = this.update_in(cx, |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                });
                            }
                            TermEvent::ClipboardStore {
                                kind: ClipboardType::Selection,
                                ..
                            }
                            | TermEvent::Wakeup
                            | TermEvent::Exited => {}
                        }
                        if !matches!(&event, TermEvent::Wakeup) {
                            non_wakeup_events += 1;
                            if non_wakeup_events >= 100 {
                                break;
                            }
                        }
                    }
                    if saw_batched_event
                        && this
                            .update_in(cx, |_, window, cx| {
                                window.invalidate_character_coordinates();
                                cx.notify();
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
            self.event_subscriptions.insert(
                terminal_id,
                TerminalEventSubscription {
                    receiver,
                    _task: task,
                },
            );
        }
    }

    fn apply_graphics_updates(&self, terminal_id: u64, updates: Option<UpdateQueues>) {
        let Some(updates) = updates else {
            return;
        };
        let mut registries = self.image_registry.borrow_mut();
        let images = registries.entry(terminal_id).or_default();

        for graphic in updates.pending {
            let key = atlas_image_key(graphic.id.get());
            if let Some(image) = terminal_image(graphic) {
                images.insert(key, image);
            }
        }
        for (image_id, graphic) in updates.pending_images {
            let key = kitty_image_key(image_id);
            if let Some(image) = terminal_image(graphic) {
                images.insert(key, image);
            }
        }
        for key in updates.remove_queue {
            images.remove(&key);
        }
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
                        .with_terminal_workspace(|workspace| {
                            workspace
                                .active()
                                .and_then(|entry| entry.terminal.selected_text())
                                .map(|selection| selection.text)
                        })
                        .flatten()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                ClipboardShortcut::Paste => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                        && let Some(terminal_id) = self
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace(|workspace| workspace.active_id)
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
            if let Some(bytes) = mappings::key_bytes(
                &keystroke.key,
                term_modifiers(keystroke.modifiers),
                terminal.mode(),
                terminal.keyboard_mode(),
                terminal.modify_other_keys(),
                true,
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
        self.last_input = Instant::now();
        self.cursor_phase = true;
        if let Some(id) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| workspace.active_id)
            .flatten()
        {
            self.bell_tabs.remove(&id);
        }
        cx.notify();
    }

    fn on_scroll(
        &mut self,
        terminal_id: u64,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = f32::from(event.delta.pixel_delta(px(self.cell_height)).y);
        let remainder = self.scroll_remainder.entry(terminal_id).or_default();
        let total = *remainder + delta;
        let lines = (total / self.cell_height).trunc() as i32;
        *remainder = total - lines as f32 * self.cell_height;
        if lines != 0 {
            self.workspace_store
                .read(cx)
                .with_terminal_workspace(|workspace| {
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
                });
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_grid(
        &self,
        terminal_id: u64,
        state: &TermSnapshot,
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
        let blink_phase =
            self.cursor_phase || self.last_input.elapsed() < Duration::from_millis(500);
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
        let cols = state.cols;
        let rows = state.screen_lines;
        let atlas_placements = state.atlas_placements.clone();
        let kitty_placements = state.kitty_placements.clone();
        let kitty_virtual_placements = state.kitty_virtual_placements.clone();
        let virtual_placeholder_paints = virtual_placeholder_paints(state);
        let history_size = (state.lines_evicted.min(i64::MAX as u64) as i64)
            .saturating_add(state.history_size.min(i64::MAX as usize) as i64);
        let display_offset = state.display_offset.min(i64::MAX as usize) as i64;
        let graphic_images = self
            .image_registry
            .borrow()
            .get(&terminal_id)
            .cloned()
            .unwrap_or_default();
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

                    let (cursor_bounds, graphic_overlays) = paint_terminal_grid(
                        bounds,
                        window,
                        cx,
                        &paint_data,
                        palette,
                        cell_width,
                        cell_height,
                        marked_text.is_none(),
                        |origin, scale_factor, window| {
                            let physical_cell_width = cell_width * scale_factor;
                            let physical_cell_height = cell_height * scale_factor;
                            let physical_origin_x = f32::from(origin.x) * scale_factor;
                            let physical_origin_y = f32::from(origin.y) * scale_factor;
                            let viewport = OverlayViewport {
                                cell_width: physical_cell_width,
                                cell_height: physical_cell_height,
                                origin_x: physical_origin_x,
                                origin_y: physical_origin_y,
                                history_size,
                                display_offset,
                                screen_lines: rows.min(i64::MAX as usize) as i64,
                            };
                            let clip = (
                                physical_origin_x,
                                physical_origin_y,
                                physical_origin_x + cols as f32 * physical_cell_width,
                                physical_origin_y + rows as f32 * physical_cell_height,
                            );
                            let overlays = layout_graphic_overlays(
                                &atlas_placements,
                                &kitty_placements,
                                &kitty_virtual_placements,
                                &virtual_placeholder_paints,
                                &graphic_images,
                                &viewport,
                                clip,
                            );
                            paint_graphic_overlays(
                                window,
                                &overlays,
                                &graphic_images,
                                scale_factor,
                                false,
                            );
                            overlays
                        },
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

                    paint_graphic_overlays(
                        window,
                        &graphic_overlays,
                        &graphic_images,
                        scale_factor,
                        true,
                    );

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

    fn grid_point_and_side(
        &self,
        terminal_id: u64,
        position: gpui::Point<Pixels>,
    ) -> Option<((usize, usize), SelectionSide)> {
        let geometry = *self.grid_bounds.borrow().get(&terminal_id)?;
        Some(grid_point_and_side(
            f32::from(position.x - geometry.bounds.left()),
            f32::from(position.y - geometry.bounds.top()),
            geometry.cols,
            geometry.rows,
            geometry.cell_width,
            geometry.cell_height,
        ))
    }

    fn terminal_mouse_down(
        &mut self,
        terminal_id: u64,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            return;
        };
        self.workspace_store
            .update(cx, |store, _cx| store.activate_terminal(terminal_id));
        let workspace_store = self.workspace_store.clone();
        let mut stop_propagation = false;
        #[cfg(target_os = "linux")]
        let primary_text = (event.button == MouseButton::Middle)
            .then(|| cx.read_from_primary().and_then(|item| item.text()))
            .flatten();
        workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                if let Some(entry) = workspace.terminal(terminal_id) {
                    let mode = entry.terminal.mode();
                    if mappings::routes_mouse(mode, event.modifiers.shift) {
                        if let Some(button) = term_mouse_button(event.button)
                            && let Some(bytes) = mappings::mouse_button_report(
                                GridPoint {
                                    row: point.0,
                                    column: point.1,
                                },
                                button,
                                term_modifiers(event.modifiers),
                                true,
                                mode,
                            )
                        {
                            entry.terminal.write_raw(bytes);
                        }
                        if event.button == MouseButton::Right {
                            stop_propagation = true;
                        }
                        return;
                    }
                    if event.button == MouseButton::Right {
                        if entry.terminal.selected_text().is_none() {
                            entry
                                .terminal
                                .start_selection(SelectionKind::Semantic, point, side);
                        }
                        return;
                    }
                    #[cfg(target_os = "linux")]
                    if event.button == MouseButton::Middle {
                        if let Some(text) = primary_text {
                            let text =
                                prepare_terminal_paste(&text, mode.contains(Mode::BRACKETED_PASTE));
                            entry.terminal.write_input(text.into_bytes());
                        }
                        stop_propagation = true;
                        return;
                    }
                    if event.button != MouseButton::Left {
                        return;
                    }
                    if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos"))
                        && let Some(link) = entry.terminal.hyperlink_at(point.0, point.1)
                    {
                        self.pressed_link = Some((terminal_id, link.url));
                        return;
                    }
                    let action = self.selection_drag.on_down(
                        terminal_id,
                        ScreenPoint {
                            x: f32::from(event.position.x),
                            y: f32::from(event.position.y),
                        },
                        point,
                        side,
                        event.click_count,
                        event.modifiers.shift,
                    );
                    match action {
                        SelectionDragAction::None => {}
                        SelectionDragAction::ClearAndWait => entry.terminal.clear_selection(),
                        SelectionDragAction::Start { kind, point, side } => {
                            entry.terminal.start_selection(kind, point, side);
                        }
                        SelectionDragAction::Update { point, side } => {
                            entry.terminal.update_selection(point, side);
                        }
                        SelectionDragAction::StartSimpleAndUpdate { .. } => {
                            unreachable!("mouse down cannot start a drag")
                        }
                    }
                }
            });
        if stop_propagation {
            cx.stop_propagation();
        }
        cx.notify();
    }

    fn terminal_mouse_move(
        &mut self,
        terminal_id: u64,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((point, side)) = self.grid_point_and_side(terminal_id, event.position) else {
            self.hovered_link = None;
            return;
        };
        let workspace_store = self.workspace_store.clone();
        workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                let Some(entry) = workspace.terminal(terminal_id) else {
                    return;
                };
                let mode = entry.terminal.mode();
                if mappings::routes_mouse(mode, event.modifiers.shift) {
                    if self
                        .selection_drag
                        .should_report_mouse_move(terminal_id, point)
                        && let Some(bytes) = mappings::mouse_move_report(
                            GridPoint {
                                row: point.0,
                                column: point.1,
                            },
                            event.pressed_button.and_then(term_mouse_button),
                            term_modifiers(event.modifiers),
                            mode,
                        )
                    {
                        entry.terminal.write_raw(bytes);
                    }
                    return;
                }
                if terminal_link_modifier(event.modifiers, cfg!(target_os = "macos")) {
                    if self
                        .last_link_hover
                        .is_none_or(|last| last.elapsed() >= Duration::from_millis(16))
                    {
                        self.last_link_hover = Some(Instant::now());
                        self.hovered_link = entry
                            .terminal
                            .hyperlink_at(point.0, point.1)
                            .map(|link| (terminal_id, link));
                    }
                } else {
                    self.hovered_link = None;
                    let action = self.selection_drag.on_move(
                        terminal_id,
                        ScreenPoint {
                            x: f32::from(event.position.x),
                            y: f32::from(event.position.y),
                        },
                        point,
                        side,
                        event.pressed_button == Some(MouseButton::Left),
                    );
                    if !matches!(action, SelectionDragAction::None) {
                        match action {
                            SelectionDragAction::StartSimpleAndUpdate {
                                anchor,
                                anchor_side,
                                point,
                                side,
                            } => {
                                entry.terminal.start_selection(
                                    SelectionKind::Simple,
                                    anchor,
                                    anchor_side,
                                );
                                entry.terminal.update_selection(point, side);
                            }
                            SelectionDragAction::Update { point, side } => {
                                entry.terminal.update_selection(point, side);
                            }
                            SelectionDragAction::None
                            | SelectionDragAction::ClearAndWait
                            | SelectionDragAction::Start { .. } => {
                                unreachable!("unexpected mouse move action")
                            }
                        }
                        if !mode.contains(Mode::ALT_SCREEN)
                            && entry.terminal.history_size() > 0
                            && let Some(lines) = drag_scroll_lines(
                                event.position.y,
                                self.grid_bounds.borrow().get(&terminal_id).copied(),
                                self.cell_height,
                            )
                        {
                            entry.terminal.scroll(lines);
                        }
                    }
                }
            });
        cx.notify();
    }

    fn terminal_mouse_up(
        &mut self,
        terminal_id: u64,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let released_url = self
            .grid_point_and_side(terminal_id, event.position)
            .and_then(|(point, _side)| {
                self.workspace_store
                    .read(cx)
                    .with_terminal_workspace(|workspace| {
                        let entry = workspace.terminal(terminal_id)?;
                        let mode = entry.terminal.mode();
                        if mappings::routes_mouse(mode, event.modifiers.shift) {
                            if let Some(button) = term_mouse_button(event.button)
                                && let Some(bytes) = mappings::mouse_button_report(
                                    GridPoint {
                                        row: point.0,
                                        column: point.1,
                                    },
                                    button,
                                    term_modifiers(event.modifiers),
                                    false,
                                    mode,
                                )
                            {
                                entry.terminal.write_raw(bytes);
                            }
                        } else if event.button == MouseButton::Left
                            && terminal_link_modifier(event.modifiers, cfg!(target_os = "macos"))
                        {
                            let released = entry
                                .terminal
                                .hyperlink_at(point.0, point.1)
                                .map(|link| link.url);
                            if let (Some((pressed_id, pressed)), Some(released)) =
                                (self.pressed_link.take(), released)
                                && pressed_id == terminal_id
                                && pressed == released
                            {
                                return Some(released);
                            }
                        }
                        None
                    })
                    .flatten()
            });
        if let Some(released_url) = released_url {
            cx.open_url(&released_url);
        }
        if self.selection_drag.on_up(terminal_id) {
            #[cfg(target_os = "linux")]
            if let Some(text) = self
                .workspace_store
                .read(cx)
                .with_terminal_workspace(|workspace| {
                    workspace
                        .terminal(terminal_id)
                        .and_then(|entry| entry.terminal.selected_text())
                        .map(|selection| selection.text)
                })
                .flatten()
            {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
        }
        self.pressed_link = None;
        cx.notify();
    }

    fn render_terminal(&self, terminal_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some((mut snapshot, label, register_input)) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                workspace.terminal(terminal_id).map(|entry| {
                    (
                        entry.terminal.snapshot(),
                        entry.terminal.label(),
                        workspace.active_id == Some(terminal_id),
                    )
                })
            })
            .flatten()
        else {
            return div().into_any_element();
        };

        self.apply_graphics_updates(terminal_id, snapshot.graphics_updates.take());

        let mut grid = v_flex().child(self.render_grid(terminal_id, &snapshot, register_input, cx));
        if snapshot.exited {
            let status = snapshot
                .exit_code
                .map(|code| crate::tr!("terminal.exited_code", code = code).into_owned())
                .unwrap_or_else(|| crate::tr!("terminal.exited").into_owned());
            grid = grid.child(
                div()
                    .h(px(self.cell_height))
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            );
        }

        // The add-to-context button is a pure overlay: it must never affect the
        // grid's geometry. Reserving space for it while a selection exists
        // resized the PTY mid-drag — rows jumped and blank lines appeared.
        let has_selection = snapshot.selection.is_some();
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
            .on_scroll_wheel(cx.listener(move |this, event, window, cx| {
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
            .when(has_selection, |this| {
                this.child(
                    Button::new(("terminal-add-context", terminal_id))
                        .absolute()
                        .right(px(PANE_PADDING))
                        .top(px(PANE_PADDING))
                        .small()
                        .label(crate::tr!("terminal.add_context"))
                        .tooltip(format!("{} · {}", label, crate::tr!("terminal.selection")))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.capture_terminal_selection(terminal_id)
                            });
                        })),
                )
            })
            .context_menu({
                let workspace_store = self.workspace_store.clone();
                move |menu, _window, cx| {
                    let has_selection = workspace_store
                        .read(cx)
                        .with_terminal_workspace(|workspace| {
                            workspace
                                .terminal(terminal_id)
                                .and_then(|entry| entry.terminal.selected_text())
                                .is_some()
                        })
                        .unwrap_or(false);
                    menu.menu_with_enable(
                        crate::tr!("terminal.copy").into_owned(),
                        Box::new(TerminalCopy(terminal_id)),
                        has_selection,
                    )
                    .menu(
                        crate::tr!("terminal.paste").into_owned(),
                        Box::new(TerminalPaste(terminal_id)),
                    )
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
        let (tabs, active_id, active_split) = self
            .workspace_store
            .read(cx)
            .with_terminal_workspace(|workspace| {
                (
                    workspace
                        .terminals
                        .iter()
                        .map(|entry| {
                            (
                                entry.id,
                                entry.terminal.label(),
                                entry.terminal.exited(),
                                self.bell_tabs.contains(&entry.id),
                            )
                        })
                        .collect::<Vec<_>>(),
                    workspace.active_id,
                    workspace.active_id.and_then(|id| workspace.split_for(id)),
                )
            })
            .unwrap_or_default();

        if self
            .marked_text
            .as_ref()
            .is_some_and(|marked| Some(marked.terminal_id) != active_id)
        {
            self.marked_text = None;
        }

        let mut tab_strip = h_flex()
            .id("terminal-tab-list")
            .role(Role::TabList)
            .aria_label(crate::tr!("terminal.tabs"))
            .min_w_0()
            .gap(px(2.))
            .overflow_hidden();
        for (id, label, exited, bell) in &tabs {
            let id = *id;
            let selected = active_id == Some(id);
            let close_id = id;
            let tab_label = crate::tr!("terminal.tab", label = label.clone()).into_owned();
            tab_strip = tab_strip.child(
                crate::material::accessible_clickable(
                    h_flex(),
                    ("terminal-tab", id),
                    Role::Tab,
                    tab_label,
                    cx,
                )
                .aria_selected(selected)
                .h(px(25.))
                .gap(px(2.))
                .px_2()
                .rounded(material::radius_button())
                .cursor_pointer()
                .bg(if selected {
                    cx.theme().list_active
                } else {
                    cx.theme().background.opacity(0.)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace_store
                        .update(cx, |store, _cx| store.activate_terminal(id));
                }))
                .child(
                    div()
                        .max_w(px(92.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(11.))
                        .text_color(if *exited || !selected {
                            cx.theme().muted_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .child(label.clone()),
                )
                .when(*bell, |this| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().warning)
                            .child("●"),
                    )
                })
                .child(
                    Button::new(("terminal-tab-close", close_id))
                        .ghost()
                        .compact()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(crate::tr!("terminal.close_tab"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, cx| store.close_terminal(close_id, cx));
                        })),
                ),
            );
        }

        let at_limit = tabs.len() >= MAX_TERMINALS_PER_SESSION;
        let can_split = !at_limit && active_id.is_some() && active_split.is_none();
        let active_exited = tabs
            .iter()
            .any(|(id, _, exited, _)| Some(*id) == active_id && *exited);
        let header = h_flex()
            .flex_none()
            .h(px(31.))
            .px_2()
            .gap_1()
            .items_center()
            .child(tab_strip)
            .child(div().flex_1())
            .when(active_exited, |this| {
                this.child(
                    Button::new("terminal-restart")
                        .ghost()
                        .small()
                        .label(crate::tr!("terminal.restart"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.restart_terminal());
                        })),
                )
            })
            .child(
                Button::new("terminal-split-horizontal")
                    .ghost()
                    .small()
                    .compact()
                    .label("↔")
                    .disabled(!can_split)
                    .tooltip(crate::tr!("terminal.split_horizontal"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace(|workspace| {
                                workspace
                                    .active()
                                    .map(|entry| entry.terminal.working_directory())
                            })
                            .flatten();
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.split_terminal(TerminalSplitDirection::Horizontal)
                                });
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-split-vertical")
                    .ghost()
                    .small()
                    .compact()
                    .label("↕")
                    .disabled(!can_split)
                    .tooltip(crate::tr!("terminal.split_vertical"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace(|workspace| {
                                workspace
                                    .active()
                                    .map(|entry| entry.terminal.working_directory())
                            })
                            .flatten();
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.split_terminal(TerminalSplitDirection::Vertical)
                                });
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-new")
                    .ghost()
                    .small()
                    .compact()
                    .label("+")
                    .disabled(at_limit)
                    .tooltip(if at_limit {
                        crate::tr!("terminal.max_reached", count = MAX_TERMINALS_PER_SESSION)
                    } else {
                        crate::tr!("terminal.new")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let cwd = this
                            .workspace_store
                            .read(cx)
                            .with_terminal_workspace(|workspace| {
                                workspace
                                    .active()
                                    .map(|entry| entry.terminal.working_directory())
                            })
                            .flatten();
                        if let Some(cwd) = cwd {
                            term::Terminal::with_spawn_cwd(cwd, || {
                                this.workspace_store
                                    .update(cx, |store, _cx| store.new_terminal());
                            });
                        }
                    })),
            )
            .child(
                Button::new("terminal-close-drawer")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(crate::tr!("terminal.close"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, cx| store.close_terminal_panel(cx));
                    })),
            );

        if active_split.is_none() {
            self.split_sizes.borrow_mut().clear();
        }
        let body: AnyElement = match (active_id, active_split) {
            (_, Some(split)) => {
                let split_sizes = self.split_sizes.clone();
                let on_resize = move |state: &gpui::Entity<gpui_base::ResizableState>,
                                      _: &mut Window,
                                      cx: &mut App| {
                    *split_sizes.borrow_mut() = state
                        .read(cx)
                        .sizes()
                        .iter()
                        .map(|size| f32::from(*size))
                        .collect();
                };
                match split.direction {
                    TerminalSplitDirection::Horizontal => {
                        let first = resizable_panel()
                            .pr(px(2.))
                            .child(self.render_terminal(split.first, cx));
                        let second = resizable_panel()
                            .pl(px(2.))
                            .child(self.render_terminal(split.second, cx));
                        h_resizable(("terminal-split-h", split.first))
                            .on_resize(on_resize)
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                    TerminalSplitDirection::Vertical => {
                        let first = resizable_panel()
                            .pb(px(2.))
                            .child(self.render_terminal(split.first, cx));
                        let second = resizable_panel()
                            .pt(px(2.))
                            .child(self.render_terminal(split.second, cx));
                        v_resizable(("terminal-split-v", split.first))
                            .on_resize(on_resize)
                            .child(first)
                            .child(second)
                            .into_any_element()
                    }
                }
            }
            (Some(id), None) => self.render_terminal(id, cx),
            _ => div()
                .p_3()
                .child(crate::tr!("terminal.starting"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .min_h_0()
            .font_family(TERMINAL_FONT_FAMILY)
            .text_size(px(TERMINAL_FONT_SIZE))
            .on_action(cx.listener(Self::on_terminal_copy))
            .on_action(cx.listener(Self::on_terminal_paste))
            .on_action(cx.listener(Self::on_terminal_select_all))
            .on_action(cx.listener(Self::on_terminal_clear))
            .on_action(cx.listener(Self::on_terminal_add_context))
            .child(header)
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "terminal-content",
                    Role::Terminal,
                    crate::tr!("terminal.content"),
                    cx,
                )
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down))
                // The accessibility focus ring is painted as a shadow behind this
                // element. Keep the terminal surface opaque so the shadow cannot
                // show through the otherwise transparent grid as a solid blue fill.
                .bg(cx.theme().popover)
                .flex_1()
                .min_h_0()
                // Resize every pane's PTY from this element's bounds. This is
                // the innermost element whose laid-out height reliably tracks
                // the drawer; the panes themselves hug their grid content
                // (percentage heights fail to resolve below this point), which
                // previously froze the row count at its initial value.
                .on_prepaint({
                    let workspace_store = self.workspace_store.clone();
                    let (cell_width, cell_height) = (self.cell_width, self.cell_height);
                    let split_sizes = self.split_sizes.clone();
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        let scale_factor = window.scale_factor();
                        let resize =
                            |cx: &App, terminal_id: u64, pane_width: f32, pane_height: f32| {
                                let cols = ((pane_width - 2. * PANE_PADDING) / cell_width)
                                    .floor()
                                    .max(2.) as usize;
                                let rows = ((pane_height - 2. * PANE_PADDING) / cell_height)
                                    .floor()
                                    .max(2.) as usize;
                                let cell_width_px =
                                    (cell_width * scale_factor).round().max(1.) as u32;
                                let cell_height_px =
                                    (cell_height * scale_factor).round().max(1.) as u32;
                                workspace_store
                                    .read(cx)
                                    .with_terminal_workspace(|workspace| {
                                        if let Some(entry) = workspace.terminal(terminal_id) {
                                            entry.terminal.resize_with_cell_size(
                                                cols,
                                                rows,
                                                cell_width_px,
                                                cell_height_px,
                                            );
                                        }
                                    });
                            };
                        match active_split {
                            None => {
                                if let Some(id) = active_id {
                                    resize(cx, id, width, height);
                                }
                            }
                            Some(split) => {
                                // Panel sizes from the resize handle; before any
                                // drag the group splits the axis evenly (1px
                                // handle). Each pane also carries a 2px gutter
                                // (`pr`/`pl`/`pb`/`pt`) toward the handle.
                                let sizes = split_sizes.borrow();
                                let axis = match split.direction {
                                    TerminalSplitDirection::Horizontal => width,
                                    TerminalSplitDirection::Vertical => height,
                                };
                                let (first, second) = match sizes.as_slice() {
                                    [first, second] => (*first, *second),
                                    _ => ((axis - 1.) / 2., (axis - 1.) / 2.),
                                };
                                match split.direction {
                                    TerminalSplitDirection::Horizontal => {
                                        resize(cx, split.first, first - 2., height);
                                        resize(cx, split.second, second - 2., height);
                                    }
                                    TerminalSplitDirection::Vertical => {
                                        resize(cx, split.first, width, first - 2.);
                                        resize(cx, split.second, width, second - 2.);
                                    }
                                }
                            }
                        }
                    }
                })
                .child(body),
            )
    }
}

fn snapped_grid_origin(bounds: Bounds<Pixels>, scale_factor: f32) -> Point<Pixels> {
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    point(snap_down(bounds.origin.x), snap_down(bounds.origin.y))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_terminal_grid<T>(
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
    paint_data: &GridPaintData,
    palette: TerminalPalette,
    cell_width: f32,
    cell_height: f32,
    show_cursor: bool,
    paint_underlay: impl FnOnce(Point<Pixels>, f32, &mut Window) -> T,
) -> (Option<Bounds<Pixels>>, T) {
    let scale_factor = window.scale_factor();
    let snap_down = |value: Pixels| px((f32::from(value) * scale_factor).floor() / scale_factor);
    let snap_up = |value: Pixels| px((f32::from(value) * scale_factor).ceil() / scale_factor);
    let origin = snapped_grid_origin(bounds, scale_factor);

    for background in paint_data.backgrounds.iter().chain(&paint_data.selections) {
        let left = origin.x + px(background.start_col as f32 * cell_width);
        let right =
            origin.x + px((background.start_col + background.cell_count) as f32 * cell_width);
        let left = snap_down(left);
        let background_bounds = Bounds::new(
            point(left, origin.y + px(background.row as f32 * cell_height)),
            size(snap_up(right) - left, px(cell_height)),
        );
        window.paint_quad(fill(background_bounds, background.color));
    }

    let underlay = paint_underlay(origin, scale_factor, window);
    let cursor_bounds = paint_data.cursor.map(|cursor| {
        Bounds::new(
            point(
                origin.x + px(cursor.start_col as f32 * cell_width),
                origin.y + px(cursor.row as f32 * cell_height),
            ),
            size(px(cursor.cell_count as f32 * cell_width), px(cell_height)),
        )
    });
    if show_cursor
        && let Some(cursor) = paint_data.cursor.filter(|cursor| cursor.visible)
        && let Some(cursor_bounds) = cursor_bounds
    {
        match (cursor.focused, cursor.shape) {
            (false, _) => {
                let t = px(1.);
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(cursor_bounds.size.width, t)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.left(), cursor_bounds.bottom() - t),
                        size(cursor_bounds.size.width, t),
                    ),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(cursor_bounds.origin, size(t, cursor_bounds.size.height)),
                    cursor.color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor_bounds.right() - t, cursor_bounds.top()),
                        size(t, cursor_bounds.size.height),
                    ),
                    cursor.color,
                ));
            }
            (_, CursorShape::Beam) => window.paint_quad(fill(
                Bounds::new(
                    cursor_bounds.origin,
                    size(px(2.), cursor_bounds.size.height),
                ),
                cursor.color,
            )),
            (_, CursorShape::Underline) => window.paint_quad(fill(
                Bounds::new(
                    point(cursor_bounds.left(), cursor_bounds.bottom() - px(2.)),
                    size(cursor_bounds.size.width, px(2.)),
                ),
                cursor.color,
            )),
            (_, CursorShape::Block) => window.paint_quad(fill(cursor_bounds, cursor.color)),
            (_, CursorShape::Hidden) => {}
        }
    }

    for run in &paint_data.text_runs {
        let mut run_font = terminal_font();
        run_font.weight = if run.style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        run_font.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let foreground = if run.style.cursor {
            terminal_color(run.style.bg, palette)
        } else {
            terminal_color(run.style.fg, palette)
        };
        let text_run = TextRun {
            len: run.text.len(),
            font: run_font,
            color: foreground,
            background_color: None,
            strikethrough: None,
            underline: run.style.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(foreground),
                wavy: run.style.underline_wavy,
            }),
        };
        let shaped = window.text_system().shape_line(
            run.text.clone().into(),
            px(TERMINAL_FONT_SIZE),
            &[text_run],
            Some(px(cell_width)),
        );
        let position = point(
            origin.x + px(run.start_col as f32 * cell_width),
            origin.y + px(run.row as f32 * cell_height),
        );
        let _ = shaped.paint(position, px(cell_height), TextAlign::Left, None, window, cx);
    }

    (cursor_bounds, underlay)
}

pub(crate) fn layout_grid(
    state: &TermSnapshot,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    layout_grid_cached(
        &mut HashMap::new(),
        0,
        state,
        palette,
        composing.then_some(""),
        hovered_link,
        focused,
        blink_phase,
    )
}

#[allow(clippy::too_many_arguments)]
fn layout_grid_cached(
    caches: &mut HashMap<u64, TerminalGridCache>,
    terminal_id: u64,
    state: &TermSnapshot,
    palette: TerminalPalette,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> GridPaintData {
    let composing = marked_text.is_some();
    let cursor = layout_cursor(state, palette);
    let cursor_cell = cursor.map(|cursor| (cursor.row, cursor.start_col));
    let grid_key = GridCacheKey {
        cols: state.cols,
        screen_lines: state.screen_lines,
        display_offset: state.display_offset,
        palette,
    };
    let cache = caches
        .entry(terminal_id)
        .or_insert_with(|| TerminalGridCache {
            key: grid_key,
            rows: vec![None; state.screen_lines],
        });
    if cache.key != grid_key || matches!(state.damage, term::rio_vt::event::TerminalDamage::Full) {
        cache.key = grid_key;
        cache.rows = vec![None; state.screen_lines];
    }

    let mut paint = RowPaintData::default();

    for row in 0..state.screen_lines {
        let row_key = row_layout_key(
            state,
            row,
            cursor_cell,
            marked_text,
            hovered_link,
            focused,
            blink_phase,
        );
        let clean = !state.row_damage.get(row).copied().unwrap_or(true);
        let cached = cache.rows[row]
            .as_ref()
            .filter(|cached| clean && cached.key == row_key)
            .map(|cached| cached.paint.clone());
        let row_paint = cached.unwrap_or_else(|| {
            let paint = layout_row(
                state,
                row,
                palette,
                composing,
                hovered_link,
                focused,
                blink_phase,
                cursor_cell,
            );
            cache.rows[row] = Some(CachedRowLayout {
                key: row_key,
                paint: paint.clone(),
            });
            paint
        });
        paint.text_runs.extend(row_paint.text_runs);
        paint.backgrounds.extend(row_paint.backgrounds);
        paint.selections.extend(row_paint.selections);
    }

    GridPaintData {
        text_runs: paint.text_runs,
        backgrounds: paint.backgrounds,
        selections: paint.selections,
        cursor: cursor.map(|mut cursor| {
            cursor.focused = focused;
            cursor.visible &= !composing && (!state.cursor_blinking || blink_phase);
            cursor
        }),
    }
}

fn row_layout_key(
    state: &TermSnapshot,
    row: usize,
    cursor_cell: Option<(usize, usize)>,
    marked_text: Option<&str>,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
) -> RowLayoutKey {
    let grid_line = row as i32 - state.display_offset as i32;
    let selection = state
        .selection
        .filter(|range| grid_line >= range.start.row.0 && grid_line <= range.end.row.0);
    let hovered_link = hovered_link
        .filter(|link| row >= link.start.0 && row <= link.end.0)
        .map(|link| (link.start, link.end));
    let cursor = cursor_cell
        .filter(|(cursor_row, _)| *cursor_row == row && state.display_offset == 0)
        .map(|position| CursorRowKey {
            position,
            shape: state.cursor_state.content,
            blinking: state.cursor_blinking,
            marked_text: marked_text.map(str::to_owned),
            focused,
            blink_phase,
        });
    RowLayoutKey {
        selection,
        hovered_link,
        cursor,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_row(
    state: &TermSnapshot,
    row: usize,
    palette: TerminalPalette,
    composing: bool,
    hovered_link: Option<&HyperlinkMatch>,
    focused: bool,
    blink_phase: bool,
    cursor_cell: Option<(usize, usize)>,
) -> RowPaintData {
    let mut paint = RowPaintData::default();
    let mut previous_cell_had_extras = false;
    for col in 0..state.cols {
        let Some(square) = state.cell(row, col).copied() else {
            break;
        };
        let Some(style) = state.style(row, col) else {
            break;
        };
        let selected = state.is_selected(row, col);
        let (fg, bg) = cell_colors(style.fg, style.bg, style.flags);

        let background = if matches!(bg, AnsiColor::Named(NamedColor::Background)) {
            None
        } else {
            Some(terminal_color(bg, palette))
        };
        if let Some(color) = background {
            push_background(&mut paint.backgrounds, row, col, color);
        }
        if selected {
            push_background(&mut paint.selections, row, col, palette.selection);
        }

        // A wide spacer still participates in backgrounds and hit-testing,
        // but never contributes a glyph to the shaped text.
        if square.wide() == Wide::Spacer {
            continue;
        }

        // Kitty Unicode placeholders are image-placement metadata, not text.
        // Their cell styling still participates in backgrounds and selection.
        if square.c() == PLACEHOLDER {
            previous_cell_had_extras = false;
            continue;
        }

        // Alacritty stores emoji variation/modifier codepoints as extras;
        // its following placeholder space is not an independently painted
        // character. This mirrors Zed's terminal layout workaround.
        let cell_text = state.cell_text(row, col).unwrap_or_else(|| " ".to_string());
        if square.c() == ' ' && previous_cell_had_extras {
            previous_cell_had_extras = false;
            continue;
        }
        previous_cell_had_extras = cell_text.chars().nth(1).is_some();

        let text = display_cell_text(&cell_text);
        let underline = style.flags.intersects(StyleFlags::ALL_UNDERLINES);
        if matches!(square.c(), '\0' | ' ') && !underline {
            continue;
        }

        let cursor_visible = !composing
            && !selected
            && state.display_offset == 0
            && cursor_cell == Some((row, col))
            && focused
            && state.cursor_state.content == CursorShape::Block
            && (!state.cursor_blinking || blink_phase);
        let hyperlink_hovered =
            hovered_link.is_some_and(|link| (row, col) >= link.start && (row, col) <= link.end);
        let style = GridTextStyle {
            fg,
            bg,
            bold: style.flags.contains(StyleFlags::BOLD),
            italic: style.flags.contains(StyleFlags::ITALIC),
            underline: underline || hyperlink_hovered,
            underline_wavy: style.flags.contains(StyleFlags::UNDERCURL),
            selected,
            cursor: cursor_visible,
        };

        if let Some(current) = paint.text_runs.last_mut()
            && current.row == row
            && current.start_col + current.cell_count == col
            && current.style == style
        {
            current.text.push_str(&text);
            current.cell_count += 1;
        } else {
            paint.text_runs.push(BatchedTextRun {
                row,
                start_col: col,
                text,
                cell_count: 1,
                style,
            });
        }
    }
    paint
}

fn push_background(backgrounds: &mut Vec<BackgroundRect>, row: usize, col: usize, color: Hsla) {
    if let Some(previous) = backgrounds.last_mut()
        && previous.row == row
        && previous.start_col + previous.cell_count == col
        && previous.color == color
    {
        previous.cell_count += 1;
    } else {
        backgrounds.push(BackgroundRect {
            row,
            start_col: col,
            cell_count: 1,
            color,
        });
    }
}

fn display_cell_text(cell_text: &str) -> String {
    let mut characters = cell_text.chars();
    let Some(first) = characters.next() else {
        return " ".to_string();
    };
    let mut text = String::new();
    text.push(if first == '\0' { ' ' } else { first });
    text.extend(characters);
    text
}

fn cell_colors(
    foreground: AnsiColor,
    background: AnsiColor,
    flags: StyleFlags,
) -> (AnsiColor, AnsiColor) {
    let (mut fg, mut bg) = (foreground, background);
    if flags.contains(StyleFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn layout_cursor(state: &TermSnapshot, palette: TerminalPalette) -> Option<CursorPaint> {
    if state.display_offset != 0 {
        return None;
    }
    let (row, col) = state.cursor?;
    let square = *state.cell(row, col)?;
    let (start_col, cell_count, color_col) = if square.wide() == Wide::Spacer && col > 0 {
        (col - 1, 2, col - 1)
    } else if square.wide() == Wide::Wide {
        (col, 2, col)
    } else {
        (col, 1, col)
    };
    let style = state.style(row, color_col)?;
    let (fg, _) = cell_colors(style.fg, style.bg, style.flags);
    Some(CursorPaint {
        row,
        start_col,
        cell_count,
        color: terminal_color(fg, palette).opacity(0.72),
        visible: !state.is_selected(row, color_col),
        shape: state.cursor_state.content,
        focused: true,
    })
}

struct TerminalInputHandler {
    drawer: Entity<TerminalDrawer>,
    terminal_id: u64,
    cursor_bounds: Option<Bounds<Pixels>>,
    cell_width: Pixels,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.drawer
            .read(cx)
            .marked_text
            .as_ref()
            .filter(|marked| marked.terminal_id == self.terminal_id)
            .map(|marked| 0..marked.text.encode_utf16().count())
    }

    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let text = text.to_string();
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = None;
            drawer.last_input = Instant::now();
            drawer.cursor_phase = true;
            drawer.bell_tabs.remove(&terminal_id);
            if !text.is_empty() {
                drawer.with_terminal_id(terminal_id, cx, |terminal| {
                    terminal.write_input(text.into_bytes());
                });
            }
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = self.terminal_id;
        let marked_text = (!new_text.is_empty()).then(|| MarkedText {
            terminal_id,
            text: new_text.to_string(),
        });
        self.drawer.update(cx, |drawer, cx| {
            drawer.marked_text = marked_text;
            cx.notify();
        });
        window.invalidate_character_coordinates();
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        let terminal_id = self.terminal_id;
        self.drawer.update(cx, |drawer, cx| {
            if drawer
                .marked_text
                .as_ref()
                .is_some_and(|marked| marked.terminal_id == terminal_id)
            {
                drawer.marked_text = None;
                cx.notify();
            }
        });
        window.invalidate_character_coordinates();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        bounds.origin.x += self.cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn terminal_clipboard_shortcut(
    key: &str,
    modifiers: gpui::Modifiers,
    use_platform_modifier: bool,
) -> Option<ClipboardShortcut> {
    let expected_modifiers = if use_platform_modifier {
        modifiers.platform
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.function
    } else {
        modifiers.control
            && modifiers.shift
            && !modifiers.platform
            && !modifiers.alt
            && !modifiers.function
    };
    if !expected_modifiers {
        return None;
    }
    if key.eq_ignore_ascii_case("c") {
        Some(ClipboardShortcut::Copy)
    } else if key.eq_ignore_ascii_case("v") {
        Some(ClipboardShortcut::Paste)
    } else {
        None
    }
}

fn terminal_link_modifier(modifiers: gpui::Modifiers, use_platform_modifier: bool) -> bool {
    if use_platform_modifier {
        modifiers.platform
    } else {
        modifiers.control
    }
}

fn prepare_terminal_paste(text: &str, bracketed_paste: bool) -> String {
    if bracketed_paste {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

fn term_modifiers(modifiers: gpui::Modifiers) -> TermModifiers {
    TermModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
        platform: modifiers.platform,
    }
}

fn term_mouse_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        _ => None,
    }
}

fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
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

fn drag_scroll_lines(y: Pixels, geometry: Option<GridGeometry>, cell_height: f32) -> Option<i32> {
    let geometry = geometry?;
    let top = geometry.bounds.top();
    let bottom = top + px(geometry.rows as f32 * geometry.cell_height);
    let pixels = if y < top {
        f32::from(top - y)
    } else if y > bottom {
        -f32::from(y - bottom)
    } else {
        return None;
    };
    let lines = (pixels.abs().powf(1.1) / cell_height).ceil() as i32;
    Some(lines.clamp(1, 3) * pixels.signum() as i32)
}

fn terminal_image(graphic: GraphicData) -> Option<TerminalImage> {
    if graphic.width == 0 || graphic.height == 0 {
        return None;
    }
    let pixel_count = graphic.width.checked_mul(graphic.height)?;
    let bgra = match graphic.color_type {
        ColorType::Rgba => {
            if graphic.pixels.len() != pixel_count.checked_mul(4)? {
                return None;
            }
            let mut pixels = graphic.pixels;
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            pixels
        }
        ColorType::Rgb => {
            if graphic.pixels.len() != pixel_count.checked_mul(3)? {
                return None;
            }
            let mut pixels = Vec::with_capacity(pixel_count.checked_mul(4)?);
            for pixel in graphic.pixels.as_chunks::<3>().0 {
                pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], u8::MAX]);
            }
            pixels
        }
    };
    let width = u32::try_from(graphic.width).ok()?;
    let height = u32::try_from(graphic.height).ok()?;
    // RenderImage's byte contract is BGRA even though image::RgbaImage is the
    // storage carrier, so protocol RGB(A) is swizzled exactly once on arrival.
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(TerminalImage {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        width: graphic.width,
        height: graphic.height,
    })
}

fn virtual_placeholder_paints(state: &TermSnapshot) -> Vec<VirtualPlaceholderPaint> {
    let mut paints = Vec::new();
    for (screen_line, row) in state.visible_rows.iter().enumerate() {
        if !row.kitty_virtual_placeholder {
            continue;
        }

        let mut current: Option<(IncompletePlacement, usize)> = None;
        for (col, square) in row.inner.iter().take(state.cols).enumerate() {
            if square.c() != PLACEHOLDER {
                flush_virtual_placeholder_paint(&mut paints, &mut current, screen_line);
                continue;
            }

            let Some(style) = state.style(screen_line, col) else {
                flush_virtual_placeholder_paint(&mut paints, &mut current, screen_line);
                continue;
            };
            let combining = square
                .extras_id()
                .and_then(|id| state.zero_width.get(&id))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut cell =
                IncompletePlacement::from_cell(style.fg, style.underline_color, combining);

            if let Some((placement, _)) = current.as_mut()
                && placement.can_append(&cell)
            {
                placement.append();
                continue;
            }

            flush_virtual_placeholder_paint(&mut paints, &mut current, screen_line);
            // Missing coordinates on the first cell default to zero before
            // continuation matching, as required by kitty's placeholder rules.
            cell.row.get_or_insert(0);
            cell.col.get_or_insert(0);
            current = Some((cell, col));
        }
        flush_virtual_placeholder_paint(&mut paints, &mut current, screen_line);
    }
    paints
}

fn flush_virtual_placeholder_paint(
    paints: &mut Vec<VirtualPlaceholderPaint>,
    current: &mut Option<(IncompletePlacement, usize)>,
    screen_line: usize,
) {
    if let Some((placement, start_screen_col)) = current.take() {
        paints.push(VirtualPlaceholderPaint {
            run: placement.complete(),
            screen_line,
            start_screen_col,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_graphic_overlays(
    atlas_placements: &[AtlasPlacement],
    kitty_placements: &[KittyPlacement],
    virtual_placements: &HashMap<(u32, u32), VirtualPlacement>,
    placeholder_paints: &[VirtualPlaceholderPaint],
    images: &HashMap<u64, TerminalImage>,
    viewport: &OverlayViewport,
    clip: (f32, f32, f32, f32),
) -> Vec<OrderedGraphicOverlay> {
    let mut overlays = Vec::new();

    for placement in atlas_placements {
        if !images.contains_key(&placement.image_key) {
            continue;
        }
        let Some(geometry) = atlas_overlay_geometry(placement, viewport) else {
            continue;
        };
        push_graphic_overlay(
            &mut overlays,
            GraphicOverlay {
                image_id: placement.image_key,
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
                z_index: -1,
                source_rect: geometry.source_rect,
            },
            0,
            0,
            clip,
        );
    }

    for placement in kitty_placements {
        let image_key = kitty_image_key(placement.image_id);
        let Some(image) = images.get(&image_key) else {
            continue;
        };
        let Some(geometry) = kitty_overlay_geometry(placement, image.width, image.height, viewport)
        else {
            continue;
        };
        push_graphic_overlay(
            &mut overlays,
            GraphicOverlay {
                image_id: image_key,
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
                z_index: placement.z_index,
                source_rect: geometry.source_rect,
            },
            1,
            placement.placement_id,
            clip,
        );
    }

    // Virtual placements have no z-index field in rio's metadata; rio's own
    // renderer assigns -1, which puts them below glyphs like atlas graphics.
    for placeholder in placeholder_paints {
        let placement = virtual_placements
            .get(&(placeholder.run.image_id, placeholder.run.placement_id))
            .or_else(|| virtual_placements.get(&(placeholder.run.image_id, 0)));
        let Some(placement) = placement else {
            continue;
        };
        let image_key = kitty_image_key(placeholder.run.image_id);
        let Some(image) = images.get(&image_key) else {
            continue;
        };
        let (Ok(image_width), Ok(image_height)) =
            (u32::try_from(image.width), u32::try_from(image.height))
        else {
            continue;
        };
        let Some(geometry) = compute_run_geometry(
            &placeholder.run,
            placement.columns,
            placement.rows,
            image_width,
            image_height,
            (placement.x, placement.y, placement.width, placement.height),
            viewport.cell_width,
            viewport.cell_height,
            viewport.origin_x,
            viewport.origin_y,
            placeholder.screen_line,
            placeholder.start_screen_col,
        ) else {
            continue;
        };
        push_graphic_overlay(
            &mut overlays,
            GraphicOverlay {
                image_id: image_key,
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
                z_index: -1,
                source_rect: geometry.source_rect,
            },
            2,
            placement.placement_id,
            clip,
        );
    }

    overlays.sort_by_key(|ordered| {
        (
            ordered.overlay.z_index,
            ordered.protocol_order,
            ordered.overlay.image_id,
            ordered.placement_order,
        )
    });
    overlays
}

fn push_graphic_overlay(
    overlays: &mut Vec<OrderedGraphicOverlay>,
    mut overlay: GraphicOverlay,
    protocol_order: u8,
    placement_order: u32,
    clip: (f32, f32, f32, f32),
) {
    if clip_overlay_to_rect(&mut overlay, clip.0, clip.1, clip.2, clip.3) {
        overlays.push(OrderedGraphicOverlay {
            overlay,
            protocol_order,
            placement_order,
        });
    }
}

fn paint_graphic_overlays(
    window: &mut Window,
    overlays: &[OrderedGraphicOverlay],
    images: &HashMap<u64, TerminalImage>,
    scale_factor: f32,
    above_text: bool,
) {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return;
    }
    for ordered in overlays {
        let overlay = &ordered.overlay;
        if (overlay.z_index >= 0) != above_text {
            continue;
        }
        let Some(image) = images.get(&overlay.image_id) else {
            continue;
        };
        paint_graphic_overlay(window, overlay, image, scale_factor);
    }
}

fn paint_graphic_overlay(
    window: &mut Window,
    overlay: &GraphicOverlay,
    image: &TerminalImage,
    scale_factor: f32,
) {
    let [u0, v0, u1, v1] = overlay.source_rect;
    let source_width = u1 - u0;
    let source_height = v1 - v0;
    if !overlay.x.is_finite()
        || !overlay.y.is_finite()
        || !overlay.width.is_finite()
        || !overlay.height.is_finite()
        || !u0.is_finite()
        || !v0.is_finite()
        || !source_width.is_finite()
        || !source_height.is_finite()
        || overlay.width <= 0.0
        || overlay.height <= 0.0
        || source_width <= 0.0
        || source_height <= 0.0
    {
        return;
    }

    let target_x = overlay.x / scale_factor;
    let target_y = overlay.y / scale_factor;
    let target_width = overlay.width / scale_factor;
    let target_height = overlay.height / scale_factor;
    let full_width = target_width / source_width;
    let full_height = target_height / source_height;
    let image_x = target_x - u0 * full_width;
    let image_y = target_y - v0 * full_height;
    let target_bounds = Bounds::new(
        point(px(target_x), px(target_y)),
        size(px(target_width), px(target_height)),
    );
    let image_bounds = Bounds::new(
        point(px(image_x), px(image_y)),
        size(px(full_width), px(full_height)),
    );
    let _ = window.paint_image(
        target_bounds,
        image_bounds,
        Default::default(),
        image.image.clone(),
        0,
        false,
    );
}

pub(crate) fn terminal_font() -> gpui::Font {
    let mut terminal_font = font(TERMINAL_FONT_FAMILY);
    terminal_font.features = FontFeatures::disable_ligatures();
    terminal_font
}

pub(crate) fn terminal_color(color: AnsiColor, palette: TerminalPalette) -> Hsla {
    match color {
        AnsiColor::Named(
            NamedColor::Foreground | NamedColor::LightForeground | NamedColor::DimForeground,
        ) => palette.foreground,
        AnsiColor::Named(NamedColor::Background) => palette.background,
        AnsiColor::Named(NamedColor::Cursor) => palette.foreground,
        AnsiColor::Named(NamedColor::DimBlack) => terminal_color(AnsiColor::Indexed(0), palette),
        AnsiColor::Named(NamedColor::DimRed) => terminal_color(AnsiColor::Indexed(1), palette),
        AnsiColor::Named(NamedColor::DimGreen) => terminal_color(AnsiColor::Indexed(2), palette),
        AnsiColor::Named(NamedColor::DimYellow) => terminal_color(AnsiColor::Indexed(3), palette),
        AnsiColor::Named(NamedColor::DimBlue) => terminal_color(AnsiColor::Indexed(4), palette),
        AnsiColor::Named(NamedColor::DimMagenta) => terminal_color(AnsiColor::Indexed(5), palette),
        AnsiColor::Named(NamedColor::DimCyan) => terminal_color(AnsiColor::Indexed(6), palette),
        AnsiColor::Named(NamedColor::DimWhite) => terminal_color(AnsiColor::Indexed(7), palette),
        AnsiColor::Named(named) => terminal_color(AnsiColor::Indexed(named as u8), palette),
        AnsiColor::Spec(color) => {
            rgb((u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)).into()
        }
        AnsiColor::Indexed(index) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use term::rio_vt::{
        ansi::KeyboardModes,
        crosswords::{grid::row::Row, pos::CursorState, square::Square, style::Style},
        event::TerminalDamage,
    };

    fn cell(ch: char, wide: Wide) -> Square {
        let mut square = Square::from_char(ch);
        square.set_wide(wide);
        square
    }

    fn snapshot(cells: Vec<Square>) -> TermSnapshot {
        let cols = cells.len();
        let mut row = Row::new(cols);
        row.inner = cells;
        TermSnapshot {
            cols,
            screen_lines: 1,
            visible_rows: vec![row],
            row_damage: vec![true],
            damage: TerminalDamage::Full,
            cursor_state: CursorState::default(),
            cursor: None,
            cursor_blinking: false,
            title: String::new(),
            exited: false,
            exit_code: None,
            display_offset: 0,
            history_size: 0,
            lines_evicted: 0,
            graphics_updates: None,
            atlas_placements: Vec::new(),
            kitty_placements: Vec::new(),
            kitty_virtual_placements: HashMap::new(),
            mode: Mode::empty(),
            keyboard_mode: KeyboardModes::NO_MODE,
            selection: None,
            styles: vec![Style::default()],
            zero_width: HashMap::new(),
        }
    }

    #[test]
    fn grid_point_uses_left_side_for_left_half_of_cell() {
        assert_eq!(
            grid_point_and_side(2., 5., 3, 2, 10., 20.),
            ((0, 0), SelectionSide::Left)
        );
    }

    #[test]
    fn grid_point_uses_right_side_for_right_half_of_cell() {
        assert_eq!(
            grid_point_and_side(8., 5., 3, 2, 10., 20.),
            ((0, 0), SelectionSide::Right)
        );
    }

    #[test]
    fn grid_point_clamps_past_last_column_to_right_side() {
        assert_eq!(
            grid_point_and_side(35., 5., 3, 2, 10., 20.),
            ((0, 2), SelectionSide::Right)
        );
    }

    #[test]
    fn simple_selection_waits_until_drag_crosses_threshold() {
        assert!(!selection_drag_started(0., 0.));
        assert!(!selection_drag_started(SELECTION_DRAG_THRESHOLD, 0.));
        assert!(selection_drag_started(SELECTION_DRAG_THRESHOLD + 0.01, 0.));
        assert!(selection_drag_started(2., 2.));
    }

    #[test]
    fn selection_drag_distinguishes_click_from_drag_threshold() {
        let mut drag = SelectionDrag::default();
        assert!(matches!(
            drag.on_down(
                7,
                ScreenPoint { x: 10., y: 10. },
                (0, 1),
                SelectionSide::Left,
                1,
                false,
            ),
            SelectionDragAction::ClearAndWait
        ));
        assert!(matches!(
            drag.on_move(
                7,
                ScreenPoint { x: 12., y: 10. },
                (0, 1),
                SelectionSide::Right,
                true,
            ),
            SelectionDragAction::None
        ));
        assert!(matches!(
            drag.on_move(
                7,
                ScreenPoint { x: 12.1, y: 10. },
                (0, 2),
                SelectionSide::Left,
                true,
            ),
            SelectionDragAction::StartSimpleAndUpdate {
                anchor: (0, 1),
                point: (0, 2),
                ..
            }
        ));
    }

    #[test]
    fn selection_drag_preserves_word_and_line_click_kinds() {
        let mut drag = SelectionDrag::default();
        assert!(matches!(
            drag.on_down(
                1,
                ScreenPoint { x: 0., y: 0. },
                (2, 3),
                SelectionSide::Left,
                2,
                false,
            ),
            SelectionDragAction::Start {
                kind: SelectionKind::Semantic,
                point: (2, 3),
                ..
            }
        ));
        assert!(matches!(
            drag.on_down(
                1,
                ScreenPoint { x: 0., y: 0. },
                (4, 0),
                SelectionSide::Left,
                3,
                false,
            ),
            SelectionDragAction::Start {
                kind: SelectionKind::Lines,
                point: (4, 0),
                ..
            }
        ));
    }

    #[test]
    fn selection_drag_updates_across_rows_after_starting() {
        let mut drag = SelectionDrag::default();
        drag.on_down(
            3,
            ScreenPoint { x: 4., y: 4. },
            (1, 4),
            SelectionSide::Left,
            1,
            false,
        );
        let action = drag.on_move(
            3,
            ScreenPoint { x: 20., y: 40. },
            (3, 2),
            SelectionSide::Right,
            true,
        );
        assert!(matches!(
            action,
            SelectionDragAction::StartSimpleAndUpdate {
                anchor: (1, 4),
                point: (3, 2),
                ..
            }
        ));
        assert!(drag.on_up(3));
    }

    #[test]
    fn unselected_default_grid_has_no_selection_or_ansi_background_paint() {
        let state = snapshot(vec![cell('x', Wide::Narrow)]);
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };

        let paint = layout_grid(&state, palette, false, None, true, true);
        assert!(paint.selections.is_empty());
        assert!(paint.backgrounds.is_empty());
    }

    #[test]
    fn shell_quotes_paths() {
        assert_eq!(shell_quote("/tmp/a"), "'/tmp/a'");
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn clipboard_shortcuts_are_platform_specific() {
        let command = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        let control_shift = gpui::Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };

        assert_eq!(
            terminal_clipboard_shortcut("c", command, true),
            Some(ClipboardShortcut::Copy)
        );
        assert_eq!(
            terminal_clipboard_shortcut("v", command, true),
            Some(ClipboardShortcut::Paste)
        );
        assert_eq!(terminal_clipboard_shortcut("c", control_shift, true), None);
        assert_eq!(terminal_clipboard_shortcut("c", command, false), None);
        assert_eq!(
            terminal_clipboard_shortcut("C", control_shift, false),
            Some(ClipboardShortcut::Copy)
        );
        assert_eq!(
            terminal_clipboard_shortcut("V", control_shift, false),
            Some(ClipboardShortcut::Paste)
        );
        assert_eq!(
            terminal_clipboard_shortcut(
                "c",
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
            ),
            None
        );
    }

    #[test]
    fn hyperlink_modifier_matches_the_platform_shortcut_convention() {
        let command = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        let control = gpui::Modifiers {
            control: true,
            ..Default::default()
        };

        assert!(terminal_link_modifier(command, true));
        assert!(!terminal_link_modifier(control, true));
        assert!(terminal_link_modifier(control, false));
        assert!(!terminal_link_modifier(command, false));
    }

    #[test]
    fn terminal_paste_preparation_is_shared_by_clipboard_paths() {
        assert_eq!(prepare_terminal_paste("a\r\nb\nc", false), "a\rb\rc");
        assert_eq!(
            prepare_terminal_paste("a\x1bb", true),
            "\x1b[200~ab\x1b[201~"
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn unix_terminal_font_uses_the_bundled_lilex_family() {
        assert_eq!(TERMINAL_FONT_FAMILY, "Lilex");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_font_remains_menlo() {
        assert_eq!(TERMINAL_FONT_FAMILY, "Menlo");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_terminal_font_remains_consolas() {
        assert_eq!(TERMINAL_FONT_FAMILY, "Consolas");
    }

    #[test]
    fn batches_mixed_cjk_at_physical_column_boundaries() {
        let cells = vec![
            cell('a', Wide::Narrow),
            cell('中', Wide::Wide),
            cell(' ', Wide::Spacer),
            cell('b', Wide::Narrow),
            cell('文', Wide::Wide),
            cell(' ', Wide::Spacer),
            cell('c', Wide::Narrow),
        ];
        let state = snapshot(cells);
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };

        let runs = layout_grid(&state, palette, false, None, true, true).text_runs;
        let boundaries = runs
            .iter()
            .map(|run| (run.start_col, run.text.as_str(), run.cell_count))
            .collect::<Vec<_>>();
        assert_eq!(boundaries, vec![(0, "a中", 2), (3, "b文", 2), (6, "c", 1)]);
        assert_eq!(
            runs.iter()
                .map(|run| run.start_col as f32 * 8.)
                .collect::<Vec<_>>(),
            vec![0., 24., 48.]
        );
    }

    #[test]
    fn row_cache_reuses_clean_rows_and_rebuilds_damaged_rows() {
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };
        let mut state = snapshot(vec![cell('a', Wide::Narrow)]);
        let mut caches = HashMap::new();
        let first = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(first.text_runs[0].text, "a");

        state.visible_rows[0].inner[0] = cell('b', Wide::Narrow);
        state.damage = TerminalDamage::CursorOnly;
        state.row_damage[0] = false;
        let cached = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(cached.text_runs[0].text, "a");

        state.damage = TerminalDamage::Partial;
        state.row_damage[0] = true;
        let rebuilt = layout_grid_cached(&mut caches, 7, &state, palette, None, None, true, true);
        assert_eq!(rebuilt.text_runs[0].text, "b");
    }

    #[test]
    fn undercurl_maps_to_wavy_underline() {
        let mut square = cell('x', Wide::Narrow);
        square.set_style_id(1);
        let mut state = snapshot(vec![square]);
        state.styles.push(Style {
            flags: StyleFlags::UNDERCURL,
            ..Style::default()
        });
        let palette = TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x336699).into(),
        };

        let run = layout_grid(&state, palette, false, None, true, true)
            .text_runs
            .remove(0);
        assert!(run.style.underline);
        assert!(run.style.underline_wavy);
    }

    #[test]
    fn printable_keys_defer_to_input_handler_but_control_keys_stay_raw() {
        let mode = Mode::empty();
        let encode = |key: &str| {
            let key = gpui::Keystroke::parse(key).unwrap();
            mappings::key_bytes(
                &key.key,
                term_modifiers(key.modifiers),
                mode,
                term::rio_vt::ansi::KeyboardModes::NO_MODE,
                None,
                true,
            )
        };
        assert_eq!(encode("a"), None);
        assert_eq!(encode("ctrl-space"), Some(vec![0]));
        assert_eq!(encode("ctrl-c"), Some(vec![3]));
    }
}
