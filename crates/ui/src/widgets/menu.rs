use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, AppContext as _, Context, DismissEvent, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Render, RenderOnce, Role, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Window, anchored, deferred, div,
    prelude::FluentBuilder, px,
};
use gpui_base::actions::{Cancel, Confirm, SelectDown, SelectUp};

use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
};

const CONTEXT: &str = "TcodePopupMenu";

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
    Item {
        label: Option<SharedString>,
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
}
impl FluentBuilder for PopupMenu {}

impl PopupMenu {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            touch: false,
            focus: cx.focus_handle(),
            items: Vec::new(),
            selected: None,
        }
    }
    fn build(
        window: &mut Window,
        cx: &mut App,
        builder: impl Fn(Self, &mut Window, &mut Context<Self>) -> Self,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let menu = Self::new(cx);
            builder(menu, window, cx)
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
            render: None,
            action,
            disabled: !enable,
            checked: false,
        });
        self
    }
    pub fn menu_with_check(
        mut self,
        label: impl Into<SharedString>,
        checked: bool,
        action: Box<dyn Action>,
    ) -> Self {
        self.items.push(MenuItem::Item {
            label: Some(label.into()),
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
            render: Some(Rc::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
            action,
            disabled: false,
            checked: false,
        });
        self
    }
    pub fn separator(mut self) -> Self {
        if !self.items.is_empty() && !matches!(self.items.last(), Some(MenuItem::Separator)) {
            self.items.push(MenuItem::Separator);
        }
        self
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
            self.selected = Some(items[(position + items.len() - 1) % items.len()]);
            cx.notify();
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
            self.selected = Some(items[position % items.len()]);
            cx.notify();
        }
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
        if self.is_empty() {
            // An empty menu is being dismissed by menu_popover; render nothing
            // rather than a bare strip for its final frame.
            return div().id("tcode-popup-menu");
        }
        let mut root = div()
            .id("tcode-popup-menu")
            .debug_selector(|| "tcode-popup-menu".into())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .flex()
            .flex_col()
            .min_w(px(160.))
            .max_w(px(420.))
            .p_1()
            .rounded(crate::material::radius_overlay())
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .occlude();
        for (index, item) in self.items.iter().enumerate() {
            match item {
                MenuItem::Separator => {
                    root = root.child(div().h(px(1.)).mx_1().my_1().bg(cx.theme().border))
                }
                MenuItem::Item {
                    label,
                    render,
                    action: _,
                    disabled,
                    checked,
                } => {
                    let selected = self.selected == Some(index);
                    let disabled = *disabled;
                    let content = render.as_ref().map(|render| render(window, cx));
                    root = root.child(
                        div()
                            .id(("menu-item", index))
                            .role(Role::MenuItem)
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .when(self.touch, |el| el.min_h(px(44.)))
                            .rounded(crate::material::radius_button())
                            .text_sm()
                            .when(selected, |el| el.bg(cx.theme().muted))
                            .when(disabled, |el| el.text_color(cx.theme().muted_foreground))
                            .when(!disabled, |el| {
                                el.hover(|style| style.bg(cx.theme().muted))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.choose(index, window, cx)
                                    }))
                            })
                            .when(*checked, |el| el.child(Icon::new(IconName::Check).xsmall()))
                            .when(!*checked, |el| el.child(div().w_4()))
                            .when_some(label.clone(), |el, label| el.child(label))
                            .when_some(content, |el, content| el.child(content)),
                    );
                }
            }
        }
        root
    }
}

type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;
type ItemRenderer = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

#[derive(Default)]
struct MenuState {
    menu: Option<Entity<PopupMenu>>,
}

fn menu_popover<T>(
    id: ElementId,
    trigger: T,
    button: MouseButton,
    builder: MenuBuilder,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement
where
    T: IntoElement + 'static,
{
    let menu_state =
        window.use_keyed_state((id.clone(), "menu-state"), cx, |_, _| MenuState::default());
    super::Popover::new(id)
        .appearance(false)
        .mouse_button(button)
        .overlay_closable(true)
        .on_open_change({
            let menu_state = menu_state.clone();
            move |open, _, cx| {
                if !open {
                    menu_state.update(cx, |state, _| state.menu = None);
                }
            }
        })
        .trigger_with(move |_, _, _| trigger.into_any_element())
        .content(move |popover, window, cx| {
            if let Some(menu) = menu_state.read(cx).menu.clone() {
                return menu;
            }
            let menu = PopupMenu::build(window, cx, |menu, window, cx| builder(menu, window, cx));
            // A builder can decide there is nothing to offer (e.g. a
            // right-click on non-link Markdown text): close the popover
            // instead of presenting an empty strip.
            if menu.read(cx).is_empty() {
                popover.dismiss(window, cx);
                return menu;
            }
            menu_state.update(cx, |state, _| state.menu = Some(menu.clone()));
            menu.focus_handle(cx).focus(window, cx);
            let popover = cx.entity();
            window
                .subscribe(&menu, cx, {
                    let menu_state = menu_state.clone();
                    move |_, _: &DismissEvent, window, cx| {
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                        menu_state.update(cx, |state, _| state.menu = None);
                    }
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
}

impl<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> ContextMenu<T> {
    pub fn touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }
}

#[derive(Default)]
struct ContextMenuState {
    menu: Option<Entity<PopupMenu>>,
    position: Point<Pixels>,
    _subscription: Option<Subscription>,
    _deferred: Option<gpui_base::DeferredPopover>,
}

impl<T: InteractiveElement + ParentElement + Styled + IntoElement + 'static> RenderOnce
    for ContextMenu<T>
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let state = window.use_keyed_state((self.id, "context-menu"), cx, |_, _| {
            ContextMenuState::default()
        });
        let builder = self.builder;
        let touch = self.touch;
        let open = Rc::new(
            move |position: Point<Pixels>,
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
                    let previous_focus = window.focused(cx);
                    let menu_focus = menu.focus_handle(cx);
                    let deferred = gpui_base::GlobalState::register_deferred_popover(cx);
                    let subscription = window.subscribe(&menu, cx, {
                        let state = state.clone();
                        move |_, _: &DismissEvent, window, cx| {
                            state.update(cx, |state, _| {
                                state.menu = None;
                                state._subscription = None;
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
                open(event.position, state.clone(), window, cx);
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
                                    open(event.position, state.clone(), window, cx);
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
        let (menu, position) = {
            let state = state.read(cx);
            (state.menu.clone(), state.position)
        };
        if let Some(menu) = menu {
            // The menu stays a child of the trigger so its actions dispatch
            // through the trigger's ancestor chain, where on_action handlers
            // (on the trigger itself or above it) live.
            trigger = trigger.child(
                deferred(
                    anchored()
                        .position(position)
                        .snap_to_window_with_margin(px(8.))
                        .child(div().child(menu.clone()).on_mouse_down_out(
                            move |event, window, cx| {
                                dismissal.consume(event, window, cx);
                                menu.update(cx, |_, cx| cx.emit(DismissEvent));
                            },
                        )),
                )
                .with_priority(gpui_base::POPUP_PRIORITY),
            );
        }
        trigger
    }
}

pub trait DropdownMenu: InteractiveElement + gpui_base::Selectable + IntoElement + 'static {
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
        }
    }
}
impl<T: InteractiveElement + gpui_base::Selectable + IntoElement + 'static> DropdownMenu for T {}

#[derive(IntoElement)]
pub struct DropdownMenuPopover<T: IntoElement + 'static> {
    id: ElementId,
    trigger: T,
    builder: MenuBuilder,
}
impl<T: IntoElement + 'static> RenderOnce for DropdownMenuPopover<T> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        menu_popover(
            self.id,
            self.trigger,
            MouseButton::Left,
            self.builder,
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        PlatformInput, TestAppContext, TouchEvent, TouchId, TouchPhase, VisualTestContext, point,
        size,
    };
    use std::cell::Cell;

    struct MenuHarness(Rc<Cell<usize>>);
    impl Render for MenuHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.0.clone();
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("row-a")
                        .relative()
                        .h(px(56.))
                        .w_full()
                        .child("A")
                        .context_menu(|menu, _, _| menu.menu("Action", Box::new(Cancel)))
                        .touch(true),
                )
                .child(
                    div()
                        .id("row-b")
                        .mt(px(400.))
                        .h(px(56.))
                        .w_full()
                        .child("B")
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
        }
    }

    fn touch(cx: &mut VisualTestContext, id: u64, phase: TouchPhase, position: Point<Pixels>) {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::Touch(TouchEvent {
                    id: TouchId(id),
                    phase,
                    position,
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        });
    }

    #[gpui::test]
    fn long_press_dismissal_consumes_outside_tap(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let clicks = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| MenuHarness(clicks)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let a = point(px(20.), px(40.));
        touch(cx, 1, TouchPhase::Started, a);
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(801));
        cx.run_until_parked();
        touch(cx, 1, TouchPhase::Ended, a);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tcode-popup-menu").is_some());
        let b = point(px(20.), px(480.));
        touch(cx, 2, TouchPhase::Started, b);
        touch(cx, 2, TouchPhase::Ended, b);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tcode-popup-menu").is_none());
        assert_eq!(clicks.get(), 0, "dismissal must not activate B");
        touch(cx, 3, TouchPhase::Started, b);
        touch(cx, 3, TouchPhase::Ended, b);
        assert_eq!(clicks.get(), 1, "the second tap activates B");
    }
}
