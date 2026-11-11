use std::path::Path;
use std::rc::Rc;

use gpui::{
    Action, Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent,
    ElementId, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, RenderOnce,
    Role, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, deferred, div, point, prelude::FluentBuilder, px,
};
use gpui_base::actions::{Cancel, Confirm, SelectDown, SelectUp};
use serde::Deserialize;
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use crate::overlay::{Notification, OverlayExt as _};
use crate::typography::TextRole as _;
use crate::{
    icon::{Icon, IconName},
    scroll::ScrollableElement as _,
    theme::ActiveTheme as _,
};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a surface drawn here takes to appear, and to leave.
const APPEAR: std::time::Duration = std::time::Duration::from_millis(200);

const CONTEXT: &str = "TcodePopupMenu";

/// The actions every context menu can offer without its surface wiring a
/// handler: [`PopupMenu`] handles them for both context and dropdown menus.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_menu, no_json)]
pub struct CopyText(pub String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_menu, no_json)]
pub struct OpenUrl(pub String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_menu, no_json)]
pub struct OpenPath(pub String);
/// Open a path of `host` in Zed: on this client for the host in this
/// process, on the host's machine over Tcode's connection otherwise.
/// [`PopupMenu::path_items`] leaves `host` to the menu, which takes the
/// current pane's.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_menu, no_json)]
pub struct OpenPathInZed {
    pub path: String,
    pub host: Option<crate::store::HostKey>,
}
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_menu, no_json)]
pub struct RevealPath(pub String);

/// Launching an editor is the client's own process work, so it is injected
/// through the client host rather than linked here. A client without one (a
/// phone, a browser) reports that plainly.
pub fn open_in_zed(path: &Path, window: &mut Window, cx: &mut App) {
    if !matches!(crate::remote::open_in_editor(path, cx), Some(Ok(()))) {
        window.push_notification(
            Notification::error(crate::tr!("errors.zed_cli_missing")),
            cx,
        );
    }
}

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("enter", Confirm { secondary: false }, Some(CONTEXT)),
        KeyBinding::new("up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(CONTEXT)),
    ]);
}

enum MenuItem {
    Separator,
    Label(SharedString),
    Item {
        label: Option<SharedString>,
        icon: Option<IconName>,
        render: Option<ItemRenderer>,
        action: Box<dyn Action>,
        disabled: bool,
        checked: bool,
    },
}

pub struct PopupMenu {
    touch: bool,
    focus: FocusHandle,
    items: Vec<MenuItem>,
    selected: Option<usize>,
    scroll: ScrollHandle,
    /// The keyboard's item scrolling into view: from and to which offset,
    /// since when. gpui's `scroll_to_item` jumps, and `ListScroll` glides a
    /// list, not a scrolled div, so the menu eases its own offset.
    reveal: Option<(Point<Pixels>, Point<Pixels>, Instant)>,
    after_choose: Option<AfterChoose>,
}
impl FluentBuilder for PopupMenu {}

impl PopupMenu {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            touch: false,
            focus: cx.focus_handle(),
            items: Vec::new(),
            selected: None,
            scroll: ScrollHandle::new(),
            reveal: None,
            after_choose: None,
        }
    }
    fn build(
        window: &mut Window,
        cx: &mut App,
        builder: impl Fn(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let menu = Self::new(cx);
            let mut menu = builder(menu, window, cx);
            menu.place_path_items(cx);
            menu
        })
    }
    pub fn menu(self, label: impl Into<SharedString>, action: Box<dyn Action>) -> Self {
        self.menu_with_enable(label, action, true)
    }
    pub fn menu_with_enable(
        mut self,
        label: impl Into<SharedString>,
        action: Box<dyn Action>,
        enable: bool,
    ) -> Self {
        self.items.push(MenuItem::Item {
            label: Some(label.into()),
            icon: None,
            render: None,
            action,
            disabled: !enable,
            checked: false,
        });
        self
    }
    pub fn menu_with_icon(
        self,
        label: impl Into<SharedString>,
        icon: IconName,
        action: Box<dyn Action>,
    ) -> Self {
        self.menu_with_icon_and_enable(label, icon, action, true)
    }
    pub fn menu_with_icon_and_enable(
        self,
        label: impl Into<SharedString>,
        icon: IconName,
        action: Box<dyn Action>,
        enable: bool,
    ) -> Self {
        let mut menu = self.menu_with_enable(label, action, enable);
        if let Some(MenuItem::Item { icon: slot, .. }) = menu.items.last_mut() {
            *slot = Some(icon);
        }
        menu
    }
    pub fn menu_with_check(
        mut self,
        label: impl Into<SharedString>,
        checked: bool,
        action: Box<dyn Action>,
    ) -> Self {
        self.items.push(MenuItem::Item {
            label: Some(label.into()),
            icon: None,
            render: None,
            action,
            disabled: false,
            checked,
        });
        self
    }
    pub fn menu_element<F, E>(mut self, action: Box<dyn Action>, builder: F) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        self.items.push(MenuItem::Item {
            label: None,
            icon: None,
            render: Some(Rc::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
            action,
            disabled: false,
            checked: false,
        });
        self
    }
    /// Run `then` once a chosen row's action has run and the menu has
    /// handed the focus back to where it opened from.
    pub fn after_choose(mut self, then: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.after_choose = Some(Rc::new(then));
        self
    }
    pub fn label(mut self, text: impl Into<SharedString>) -> Self {
        self.items.push(MenuItem::Label(text.into()));
        self
    }
    pub fn separator(mut self) -> Self {
        if !self.items.is_empty() && !matches!(self.items.last(), Some(MenuItem::Separator)) {
            self.items.push(MenuItem::Separator);
        }
        self
    }
    /// Fit the path items to the current pane's host: a press anywhere in a
    /// pane makes it current before the menu it opens is built. Each Open in
    /// Zed item that names no host takes that host's wording, and goes where
    /// the host offers no editor. On another machine a path is not on this
    /// client, so its Open and Reveal go, and the separators they leave
    /// unpaired with them.
    fn place_path_items(&mut self, cx: &App) {
        let entry = crate::remote::ZedEntry::current(cx);
        let elsewhere =
            crate::shell::current_store(cx).is_some_and(|store| store.read(cx).is_remote());
        self.items.retain_mut(|item| {
            let MenuItem::Item { label, action, .. } = item else {
                return true;
            };
            if elsewhere && (action.as_any().is::<OpenPath>() || action.as_any().is::<RevealPath>())
            {
                return false;
            }
            let Some(open) = action.as_any().downcast_ref::<OpenPathInZed>() else {
                return true;
            };
            if open.host.is_some() {
                return true;
            }
            let Some(entry) = &entry else {
                return false;
            };
            *label = Some(crate::tr!(entry.label).into_owned().into());
            *action = Box::new(OpenPathInZed {
                path: open.path.clone(),
                host: Some(entry.host.clone()),
            });
            true
        });
        self.items.dedup_by(|next, kept| {
            matches!((kept, next), (MenuItem::Separator, MenuItem::Separator))
        });
        if matches!(self.items.first(), Some(MenuItem::Separator)) {
            self.items.remove(0);
        }
        if matches!(self.items.last(), Some(MenuItem::Separator)) {
            self.items.pop();
        }
    }
    /// What a file path offers wherever one appears: open it, open it in
    /// Zed, reveal it, and copy it absolute or relative to `base_dir`.
    pub fn path_items(self, path: &str, base_dir: Option<&Path>) -> Self {
        let relative = base_dir
            .map(|base_dir| crate::workspace_walk::relativize_to_workspace(path, base_dir))
            .filter(|relative| relative != path);
        self.menu(
            crate::tr!("chat.open").into_owned(),
            Box::new(OpenPath(path.to_string())),
        )
        .menu(
            crate::tr!("chat.open_zed").into_owned(),
            Box::new(OpenPathInZed {
                path: path.to_string(),
                host: None,
            }),
        )
        .menu(
            crate::tr!("chat.reveal_in_file_manager").into_owned(),
            Box::new(RevealPath(path.to_string())),
        )
        .separator()
        .menu(
            crate::tr!("chat.copy_path").into_owned(),
            Box::new(CopyText(path.to_string())),
        )
        .when_some(relative, |menu, relative| {
            menu.menu(
                crate::tr!("markdown.path_copy_relative").into_owned(),
                Box::new(CopyText(relative)),
            )
        })
    }
    /// Copy and Select All for read-only text: Copy writes `selected`, and is
    /// enabled only when something is; Select All dispatches to the surface
    /// that owns the text and is offered when `select_all`.
    pub fn selection_items(self, selected: String, select_all: bool) -> Self {
        let has_selection = !selected.is_empty();
        self.menu_with_enable(
            crate::tr!("edit_menu.copy").into_owned(),
            Box::new(CopyText(selected)),
            has_selection,
        )
        .when(select_all, |menu| {
            menu.menu(
                crate::tr!("edit_menu.select_all").into_owned(),
                Box::new(gpui_base::input::SelectAll),
            )
        })
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    fn clickable(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                matches!(
                    item,
                    MenuItem::Item {
                        disabled: false,
                        ..
                    }
                )
                .then_some(i)
            })
            .collect()
    }
    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(MenuItem::Item {
            action,
            disabled: false,
            ..
        }) = self.items.get(index)
        else {
            return;
        };
        window.dispatch_action(action.boxed_clone(), cx);
        cx.emit(DismissEvent);
        if let Some(then) = self.after_choose.clone() {
            window.defer(cx, move |window, cx| then(window, cx));
        }
    }
    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.choose(index, window, cx);
        }
    }
    fn up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let items = self.clickable();
        if !items.is_empty() {
            let position = self
                .selected
                .and_then(|selected| items.iter().position(|i| *i == selected))
                .unwrap_or(0);
            self.select(items[(position + items.len() - 1) % items.len()], cx);
        }
    }
    fn down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let items = self.clickable();
        if !items.is_empty() {
            let position = self
                .selected
                .and_then(|selected| items.iter().position(|i| *i == selected))
                .map(|i| i + 1)
                .unwrap_or(0);
            self.select(items[position % items.len()], cx);
        }
    }
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        // The item glides into view: as far as brings it in, from wherever
        // the menu is now, mid-glide too.
        let viewport = self.scroll.bounds();
        match self.scroll.bounds_for_item(index) {
            Some(item) if !cx.reduce_motion() => {
                let from = self.scroll.offset();
                let shift = if item.top() < viewport.top() {
                    viewport.top() - item.top()
                } else if item.bottom() > viewport.bottom() {
                    viewport.bottom() - item.bottom()
                } else {
                    px(0.)
                };
                if shift != px(0.) {
                    let to = point(from.x, from.y + shift);
                    self.reveal = Some((from, to, cx.background_executor().now()));
                }
            }
            _ => self.scroll.scroll_to_item(index),
        }
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for PopupMenu {}
impl Focusable for PopupMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PopupMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((from, to, since)) = self.reveal {
            let elapsed = cx
                .background_executor()
                .now()
                .saturating_duration_since(since);
            let progress = crate::motion::Curve::new(GLIDE).progress(elapsed);
            self.scroll
                .set_offset(point(from.x, from.y + (to.y - from.y) * progress));
            if progress < 1. {
                window.request_animation_frame();
            } else {
                self.reveal = None;
            }
        }
        if self.is_empty() {
            // An empty menu is being dismissed by menu_popover; render nothing
            // rather than a bare strip for its final frame.
            return crate::material::raised(div()).id("tcode-popup-menu");
        }
        let root = crate::material::overlay_contour(div(), cx)
            .rounded(crate::material::radius_overlay(cx))
            .id("tcode-popup-menu")
            .debug_selector(|| "tcode-popup-menu".into())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(|action: &CopyText, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(action.0.clone()))
            })
            .on_action(|action: &OpenUrl, _, cx| cx.open_url(&action.0))
            .on_action(|action: &OpenPath, _, cx| cx.open_with_system(Path::new(&action.0)))
            .on_action(|action: &OpenPathInZed, window, cx| {
                crate::remote::open_path_in_zed(
                    action.host.as_ref(),
                    Path::new(&action.path),
                    window,
                    cx,
                )
            })
            .on_action(|action: &RevealPath, _, cx| cx.reveal_path(Path::new(&action.0)))
            .flex()
            .flex_col()
            // The menu's own width bounds: wide enough for a label and its
            // shortcut, narrow enough to stay a menu beside its anchor.
            .min_w(px(160.))
            .max_w(px(420.))
            .occlude();
        let insets = crate::window_seam::content_insets(window);
        // Both anchors keep up to 8px from each window edge; the border is
        // outside the scrolled items.
        let max_height =
            (window.viewport_size().height - insets.top - insets.bottom - px(18.)).max(px(0.));
        let mut items = div()
            .id("tcode-popup-menu-items")
            .flex()
            .flex_col()
            .max_h(max_height)
            .p_1();
        for (index, item) in self.items.iter().enumerate() {
            match item {
                MenuItem::Separator => {
                    items = items.child(div().h(px(1.)).mx_1().my_1().bg(cx.theme().border))
                }
                MenuItem::Label(label) => {
                    items = items.child(crate::material::list_caption(label.clone(), false, cx));
                }
                MenuItem::Item {
                    label,
                    icon,
                    render,
                    action,
                    disabled,
                    checked,
                } => {
                    let selected = self.selected == Some(index);
                    let disabled = *disabled;
                    let content = render.as_ref().map(|render| render(window, cx));
                    // A touch menu has no keyboard to hint at.
                    let key = (!self.touch)
                        .then(|| super::Kbd::binding_for_action(action.as_ref(), None, window))
                        .flatten();
                    items = items.child(
                        crate::material::menu_item_frame(
                            self.touch,
                            if disabled {
                                crate::material::ItemFill::Inert
                            } else {
                                crate::material::ItemFill::highlight(selected, cx.theme().muted)
                            },
                            cx,
                        )
                        .text_body(self.touch, cx)
                        .id(("menu-item", index))
                        .debug_selector(move || format!("menu-item-{index}"))
                        .role(Role::MenuItem)
                        .when_some(label.clone(), |el, label| el.aria_label(label))
                        .when(selected && disabled, |el| el.bg(cx.theme().muted))
                        .when(disabled, |el| el.text_color(cx.theme().muted_foreground))
                        .when(!disabled, |el| {
                            el.on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(index, window, cx)
                            }))
                        })
                        .child(div().flex_none().w_4().flex().justify_center().children(
                            match (*checked, icon.clone()) {
                                (true, _) => Some(Icon::new(IconName::Check)),
                                (false, Some(icon)) => Some(Icon::new(icon)),
                                (false, None) => None,
                            },
                        ))
                        .when_some(label.clone(), |el, label| el.child(label))
                        .when_some(content, |el, content| el.child(content))
                        .when_some(key, |el, key| {
                            el.child(
                                div()
                                    .flex_none()
                                    .ml_auto()
                                    .pl(cx.theme().tokens.spacing.md)
                                    .text_color(cx.theme().muted_foreground)
                                    .child(key.appearance(false)),
                            )
                        }),
                    );
                }
            }
        }
        root.child(items.overflow_y_scroll_area().track_scroll(&self.scroll))
    }
}

type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
type ItemRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;
type AfterChoose = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Default)]
struct MenuState {
    menu: Option<Entity<PopupMenu>>,
}

fn menu_popover<T>(
    id: ElementId,
    trigger: T,
    button: MouseButton,
    builder: MenuBuilder,
    touch: bool,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement
where
    T: gpui::StatefulInteractiveElement + IntoElement + 'static,
{
    let menu_state =
        window.use_keyed_state((id.clone(), "menu-state"), cx, |_, _| MenuState::default());
    super::Popover::new(id)
        .appearance(false)
        .mouse_button(button)
        .overlay_closable(true)
        // Each opening builds a fresh menu; a closing one keeps its menu, so
        // the leaving popover draws it again without building or focusing.
        .on_open_change({
            let menu_state = menu_state.clone();
            move |open, _, cx| {
                if *open {
                    menu_state.update(cx, |state, _| state.menu = None);
                }
            }
        })
        .trigger_with(move |_, toggle, _, _| {
            trigger
                .when_some(toggle, |trigger, toggle| {
                    trigger.on_a11y_action(gpui::accesskit::Action::Click, move |_, window, cx| {
                        toggle(window, cx);
                    })
                })
                .into_any_element()
        })
        .content(move |popover, window, cx| {
            if let Some(menu) = menu_state.read(cx).menu.clone() {
                return menu;
            }
            let menu = PopupMenu::build(window, cx, |mut menu, window, cx| {
                menu.touch = touch;
                builder(menu, window, cx)
            });
            // A builder can decide there is nothing to offer (e.g. a
            // right-click on non-link Markdown text): close the popover
            // instead of presenting an empty strip.
            if menu.read(cx).is_empty() {
                popover.dismiss(window, cx);
                return menu;
            }
            menu_state.update(cx, |state, _| state.menu = Some(menu.clone()));
            menu.focus_handle(cx).focus(window, cx);
            // Weak: `menu_state` owns the menu this subscription lives as long
            // as, so a strong capture would keep the popover state, and its
            // deferred registration, alive after the trigger unmounts.
            let popover = cx.entity().downgrade();
            window
                .subscribe(&menu, cx, move |_, _: &DismissEvent, window, cx| {
                    let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                })
                .detach();
            menu
        })
}

pub trait ContextMenuExt:
    InteractiveElement + ParentElement + Styled + IntoElement + 'static
{
    #[track_caller]
    fn context_menu(
        mut self,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> ContextMenu<Self>
    where
        Self: Sized,
    {
        let id = self
            .interactivity()
            .element_id
            .clone()
            .unwrap_or_else(|| ElementId::CodeLocation(*std::panic::Location::caller()));
        ContextMenu {
            id,
            trigger: self,
            builder: Rc::new(builder),
            touch: false,
            reveal: None,
        }
    }
}
impl<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> ContextMenuExt for T {}

#[derive(IntoElement)]
pub struct ContextMenu<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> {
    id: ElementId,
    trigger: T,
    builder: MenuBuilder,
    touch: bool,
    reveal: Option<(crate::motion::ListHover, SharedString)>,
}

impl<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> ContextMenu<T> {
    pub fn touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }

    /// A trigger that is `item` of the list `hover`: what it reveals stays
    /// for as long as the menu holds its hover fill.
    pub(crate) fn reveals(
        mut self,
        hover: &crate::motion::ListHover,
        item: impl Into<SharedString>,
    ) -> Self {
        self.reveal = Some((hover.clone(), item.into()));
        self
    }
}

#[derive(Default)]
struct ContextMenuState {
    menu: Option<Entity<PopupMenu>>,
    /// Dismissed, and kept only to be drawn while it leaves.
    closing: bool,
    reveal: Option<crate::motion::HoverHold>,
    position: Point<Pixels>,
    _subscription: Option<Subscription>,
    _deferred: Option<gpui_base::DeferredPopover>,
}

impl<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> RenderOnce
    for ContextMenu<T>
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let state = window.use_keyed_state((self.id.clone(), "context-menu"), cx, |_, _| {
            ContextMenuState::default()
        });
        let builder = self.builder;
        let touch = self.touch;
        let reveal = self.reveal;
        let open = Rc::new(
            move |position: Point<Pixels>,
                  long_press: bool,
                  state: Entity<ContextMenuState>,
                  window: &mut Window,
                  cx: &mut App| {
                let builder = builder.clone();
                // Deeper bubble handlers have already run, but entity updates
                // they queued may not be applied yet; build after this event
                // settles so the builder reads their state.
                window.defer(cx, move |window, cx| {
                    let menu = PopupMenu::build(window, cx, |mut menu, window, cx| {
                        menu.touch = touch;
                        builder(menu, window, cx)
                    });
                    // A builder can decide there is nothing to offer (e.g. a
                    // right-click on non-link Markdown text): open nothing.
                    if menu.read(cx).is_empty() {
                        return;
                    }
                    if long_press {
                        crate::haptics::long_press();
                    }
                    let previous_focus = window.focused(cx);
                    let menu_focus = menu.focus_handle(cx);
                    let deferred = gpui_base::GlobalState::register_deferred_popover(cx);
                    // Weak: the subscription is stored on `state`, so a strong
                    // capture would keep it, and its deferred registration,
                    // alive after the trigger unmounts.
                    let subscription = window.subscribe(&menu, cx, {
                        let state = state.downgrade();
                        move |_, _: &DismissEvent, window, cx| {
                            let _ = state.update(cx, |state, _| {
                                state.closing = true;
                                state._deferred = None;
                            });
                            if menu_focus.contains_focused(window, cx)
                                && let Some(previous) = &previous_focus
                            {
                                previous.focus(window, cx);
                            }
                            window.refresh();
                        }
                    });
                    menu.focus_handle(cx).focus(window, cx);
                    state.update(cx, |state, _| {
                        state.menu = Some(menu);
                        state.closing = false;
                        state.position = position;
                        state._subscription = Some(subscription);
                        state._deferred = Some(deferred);
                    });
                    window.refresh();
                });
            },
        );
        let mut trigger = self.trigger.on_mouse_down(MouseButton::Right, {
            let state = state.clone();
            let open = open.clone();
            move |event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                open(event.position, false, state.clone(), window, cx);
            }
        });
        if touch {
            let state = state.clone();
            trigger = trigger.child(
                gpui::canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        window.on_mouse_event(
                            move |event: &gpui::LongPressEvent, phase, window, cx| {
                                if phase == gpui::DispatchPhase::Bubble
                                    && event.phase == gpui::TouchPhase::Started
                                    && bounds.contains(&event.position)
                                {
                                    window.capture_long_press(&state);
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    open(event.position, true, state.clone(), window, cx);
                                }
                            },
                        );
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        trigger = trigger.child(dismissal.release_listener());
        let (menu, closing, position) = {
            let state = state.read(cx);
            (state.menu.clone(), state.closing, state.position)
        };
        // Sampled only while there is a menu, so a mounted trigger with
        // none asks for no frames.
        let presence = menu.is_some().then(|| {
            let presence = crate::motion::appear(
                (self.id.clone(), "context-menu"),
                !closing,
                APPEAR,
                window,
                cx,
            );
            // A finger lifts and a pointer moves on to the menu, and nothing
            // else marks which row the menu acts on.
            let held = !closing;
            crate::motion::hold_hover_layer(self.id, held, window, cx);
            if let Some((hover, item)) = reveal {
                state.update(cx, |state, cx| {
                    state.reveal =
                        held.then(|| state.reveal.take().unwrap_or_else(|| hover.hold(item, cx)));
                });
            }
            presence
        });
        if presence.is_some_and(|presence| !presence.should_render()) {
            state.update(cx, |state, _| {
                state.menu = None;
                state.closing = false;
                state._subscription = None;
            });
        }
        if let (Some(menu), Some(presence)) = (menu, presence)
            && presence.should_render()
        {
            let progress = presence.progress;
            let surface = div()
                .opacity(progress)
                .top(crate::motion::lift(progress))
                .child(crate::motion::faded::faded(progress, menu.clone()));
            let surface = if closing {
                // Above the menu: a leaving menu takes no hover or press.
                surface.child(div().absolute().inset_0().occlude())
            } else {
                surface.on_mouse_down_out(move |_, window, cx| {
                    dismissal.consume(window, cx);
                    menu.update(cx, |_, cx| cx.emit(DismissEvent));
                })
            };
            // The menu stays a child of the trigger so its actions dispatch
            // through the trigger's ancestor chain, where on_action handlers
            // (on the trigger itself or above it) live.
            trigger = trigger.child(
                deferred(
                    gpui_base::Positioner::corner(Anchor::TopLeft, position)
                        // The window margin base's own Popup keeps, which it
                        // does not export.
                        .margin(px(8.))
                        .occlude()
                        .child(surface),
                )
                .with_priority(gpui_base::POPUP_PRIORITY),
            );
        }
        trigger
    }
}

pub trait DropdownMenu:
    gpui::StatefulInteractiveElement + gpui_base::Selectable + IntoElement + 'static
{
    fn dropdown_menu(
        mut self,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> DropdownMenuPopover<Self>
    where
        Self: Sized,
    {
        let id = self.interactivity().element_id.clone().unwrap_or(0.into());
        DropdownMenuPopover {
            id,
            trigger: self,
            builder: Rc::new(builder),
            touch: false,
        }
    }
}
impl<T: gpui::StatefulInteractiveElement + gpui_base::Selectable + IntoElement + 'static>
    DropdownMenu for T
{
}

#[derive(IntoElement)]
pub struct DropdownMenuPopover<T: gpui::StatefulInteractiveElement + IntoElement + 'static> {
    id: ElementId,
    trigger: T,
    builder: MenuBuilder,
    touch: bool,
}
impl<T: gpui::StatefulInteractiveElement + IntoElement + 'static> DropdownMenuPopover<T> {
    pub fn touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }
}
impl<T: gpui::StatefulInteractiveElement + IntoElement + 'static> RenderOnce
    for DropdownMenuPopover<T>
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        menu_popover(
            self.id,
            self.trigger,
            MouseButton::Left,
            self.builder,
            self.touch,
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};

    /// Text a view has selected, offered by its context menu.
    struct SelectionHarness;
    impl Render for SelectionHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("selected-text")
                .debug_selector(|| "selected-text".into())
                .size_full()
                .context_menu(|menu, _, _| menu.selection_items("    indented\n".into(), false))
        }
    }

    /// The menu's Copy puts the selection on the clipboard as it was
    /// selected, its leading indentation and trailing newline included.
    #[gpui::test]
    fn the_selection_menu_copies_the_selection_as_selected(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let (_, cx) = cx.add_window_view(|_, _| SelectionHarness);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let at = cx.debug_bounds("selected-text").unwrap().center();
        cx.simulate_mouse_down(at, MouseButton::Right, Default::default());
        cx.simulate_mouse_up(at, MouseButton::Right, Default::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let copy = cx.debug_bounds("menu-item-0").expect("menu opens").center();
        cx.simulate_click(copy, Default::default());
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("    indented\n".into())
        );
    }

    fn touch(cx: &mut VisualTestContext, phase: gpui::TouchPhase, position: Point<Pixels>) {
        cx.simulate_event(gpui::TouchEvent {
            id: gpui::TouchId(1),
            phase,
            position,
            predicted_position: None,
            force: None,
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    type Press = Rc<std::cell::RefCell<Option<Box<dyn FnMut(&mut Window, &mut App)>>>>;

    // Capture the registered action at the trigger boundary because GPUI's
    // TestWindow does not implement the platform accessibility adapter.
    #[derive(IntoElement)]
    struct AccessibleTrigger {
        button: gpui::Stateful<gpui::Div>,
        press: Press,
        node: Rc<std::cell::RefCell<Option<gpui::accesskit::Node>>>,
    }
    impl InteractiveElement for AccessibleTrigger {
        fn interactivity(&mut self) -> &mut gpui::Interactivity {
            self.button.interactivity()
        }
    }
    impl gpui::StatefulInteractiveElement for AccessibleTrigger {
        fn on_a11y_action(
            mut self,
            action: gpui::accesskit::Action,
            mut listener: impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App)
            + 'static,
        ) -> Self {
            assert_eq!(action, gpui::accesskit::Action::Click);
            *self.press.borrow_mut() = Some(Box::new(move |window, cx| listener(None, window, cx)));
            let press = self.press.clone();
            self.button = self.button.on_a11y_action(action, move |_, window, cx| {
                press.borrow_mut().as_mut().unwrap()(window, cx);
            });
            self
        }
    }
    impl gpui_base::Selectable for AccessibleTrigger {
        fn selected(mut self, selected: bool) -> Self {
            self.button = self.button.aria_selected(selected);
            self
        }
        fn is_selected(&self) -> bool {
            false
        }
    }
    impl RenderOnce for AccessibleTrigger {
        fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
            use gpui::Element as _;
            let mut node = gpui::accesskit::Node::new(self.button.a11y_role().unwrap());
            self.button.write_a11y_info(&mut node);
            *self.node.borrow_mut() = Some(node);
            self.button
        }
    }
    struct AccessibleDropdown(Press, Rc<std::cell::RefCell<Option<gpui::accesskit::Node>>>);
    impl Render for AccessibleDropdown {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            AccessibleTrigger {
                button: div()
                    .id("actions")
                    .role(gpui::Role::Button)
                    .aria_label("Actions for #716")
                    .child("Actions"),
                press: self.0.clone(),
                node: self.1.clone(),
            }
            .dropdown_menu(|menu, _, _| menu.menu("Choose", Box::new(Cancel)))
        }
    }

    #[gpui::test]
    fn named_desktop_dropdown_press_opens_menu(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let press = Press::default();
        let node = Rc::new(std::cell::RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let press = press.clone();
            let node = node.clone();
            move |_, _| AccessibleDropdown(press, node)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        {
            let node = node.borrow();
            let trigger = node.as_ref().expect("named dropdown trigger");
            assert_eq!(trigger.role(), gpui::accesskit::Role::Button);
            assert!(trigger.supports_action(gpui::accesskit::Action::Click));
        }
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        cx.update(|window, cx| {
            press.borrow_mut().as_mut().expect("press action")(window, cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tcode-popup-menu").is_some());
    }

    /// A phone row's "more" button: a dropdown opened by touch.
    struct MoreMenu;
    impl Render for MoreMenu {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().pt(px(200.)).child(
                crate::widgets::Button::new("more")
                    .label("More")
                    .debug_selector(|| "more-trigger".into())
                    .dropdown_menu(|menu, _, _| {
                        menu.menu_with_icon(
                            "Copy link",
                            IconName::Copy,
                            Box::new(CopyText("https://example.com/pr/716".into())),
                        )
                        .menu(
                            "Open in browser",
                            Box::new(OpenUrl("https://example.com/pr/716".into())),
                        )
                    })
                    .touch(true),
            )
        }
    }

    #[gpui::test]
    fn a_touch_dropdown_delivers_its_action(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let (_, cx) = cx.add_window_view(|_, _| MoreMenu);
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let trigger = cx.debug_bounds("more-trigger").unwrap().center();
        touch(cx, gpui::TouchPhase::Started, trigger);
        touch(cx, gpui::TouchPhase::Ended, trigger);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("menu-item-0").expect("menu opens");
        touch(cx, gpui::TouchPhase::Started, row.center());
        touch(cx, gpui::TouchPhase::Ended, row.center());
        cx.run_until_parked();
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        touch(cx, gpui::TouchPhase::Started, trigger);
        touch(cx, gpui::TouchPhase::Ended, trigger);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("menu-item-1").expect("menu reopens");
        touch(cx, gpui::TouchPhase::Started, row.center());
        touch(cx, gpui::TouchPhase::Ended, row.center());
        cx.run_until_parked();
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            (copied, cx.opened_url()),
            (
                Some("https://example.com/pr/716".into()),
                Some("https://example.com/pr/716".into())
            )
        );
    }
}
