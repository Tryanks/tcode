use crate::theme::ActiveTheme as _;
use crate::touch_scroll::TouchScrollExt as _;
use gpui::{
    Anchor, AnyElement, App, Context, ElementId, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement, RenderOnce, StyleRefinement, Styled, Window,
    prelude::FluentBuilder as _,
};
use gpui::{
    Focusable as _, Role, SharedString, StatefulInteractiveElement as _, anchored, deferred, div,
    point, px,
};
use gpui_base::StyledExt as _;
use std::rc::Rc;

pub use gpui_base::PopoverState;

/// Styled tcode facade over the headless gpui-base popover.
#[derive(IntoElement)]
pub struct Popover {
    id: ElementId,
    sheet_title: Option<SharedString>,
    anchor: Anchor,
    default_open: bool,
    open: Option<bool>,
    tracked_focus: Option<FocusHandle>,
    trigger: Option<TriggerBuilder>,
    content: Option<ContentBuilder>,
    children: Vec<AnyElement>,
    style: StyleRefinement,
    mouse_button: MouseButton,
    overlay_closable: bool,
    appearance: bool,
    on_open_change: Option<super::ToggleHandler>,
}

type TriggerBuilder = Box<dyn FnOnce(bool, &Window, &App) -> AnyElement>;
type ContentBuilder =
    Box<dyn FnOnce(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> AnyElement>;

impl Popover {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            sheet_title: None,
            anchor: Anchor::TopLeft,
            default_open: false,
            open: None,
            tracked_focus: None,
            trigger: None,
            content: None,
            children: Vec::new(),
            style: StyleRefinement::default(),
            mouse_button: MouseButton::Left,
            overlay_closable: true,
            appearance: true,
            on_open_change: None,
        }
    }
    /// Present the same picker state and content in a touch-sized bottom sheet.
    pub fn bottom_sheet(mut self, title: impl Into<SharedString>) -> Self {
        self.sheet_title = Some(title.into());
        self
    }
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }
    pub fn mouse_button(mut self, button: MouseButton) -> Self {
        self.mouse_button = button;
        self
    }
    pub fn default_open(mut self, open: bool) -> Self {
        self.default_open = open;
        self
    }
    pub fn open(mut self, open: bool) -> Self {
        self.open = Some(open);
        self
    }
    pub fn overlay_closable(mut self, value: bool) -> Self {
        self.overlay_closable = value;
        self
    }
    pub fn appearance(mut self, value: bool) -> Self {
        self.appearance = value;
        self
    }
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.tracked_focus = Some(focus.clone());
        self
    }
    pub fn on_open_change(
        mut self,
        callback: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(callback));
        self
    }
    pub fn trigger<T>(mut self, trigger: T) -> Self
    where
        T: gpui_base::Selectable + IntoElement + 'static,
    {
        self.trigger = Some(Box::new(move |open, _, _| {
            let selected = trigger.is_selected();
            trigger.selected(selected || open).into_any_element()
        }));
        self
    }
    pub(crate) fn trigger_with(
        mut self,
        trigger: impl FnOnce(bool, &Window, &App) -> AnyElement + 'static,
    ) -> Self {
        self.trigger = Some(Box::new(trigger));
        self
    }
    pub fn content<F, E>(mut self, builder: F) -> Self
    where
        F: FnOnce(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> E + 'static,
        E: IntoElement,
    {
        self.content = Some(Box::new(move |state, window, cx| {
            builder(state, window, cx).into_any_element()
        }));
        self
    }
}

impl ParentElement for Popover {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Popover {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.sheet_title.is_some() {
            return self.render_sheet(window, cx);
        }

        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let release = dismissal.clone();
        let closable = self.overlay_closable;
        let appearance = self.appearance;
        let content = self.content;
        let children = self.children;
        let style = self.style;
        gpui_base::Popover::new(self.id)
            .anchor(self.anchor)
            .mouse_button(self.mouse_button)
            .default_open(self.default_open)
            .overlay_closable(false)
            .when_some(self.open, |base, open| base.open(open))
            .when_some(self.tracked_focus, |base, focus| base.track_focus(&focus))
            .when_some(self.trigger, |base, trigger| {
                base.trigger_with(move |open, window, cx| {
                    div()
                        .child(trigger(open, window, cx))
                        .child(release.release_listener())
                        .into_any_element()
                })
            })
            .when_some(self.on_open_change, |base, callback| {
                base.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .content(move |state, window, cx| {
                let popover = cx.entity();
                let parent = window.current_view();
                gpui_base::v_flex()
                    .id("tcode-popover-content")
                    .when(closable, |el| {
                        el.on_mouse_down_out(move |event, window, cx| {
                            dismissal.consume(event, window, cx);
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                            cx.notify(parent);
                        })
                    })
                    .occlude()
                    .when(appearance, |el| {
                        el.p_1()
                            .rounded(crate::material::radius_overlay())
                            .bg(cx.theme().popover)
                            .border_1()
                            .border_color(cx.theme().border)
                            .shadow_xl()
                    })
                    .when_some(content, |el, builder| el.child(builder(state, window, cx)))
                    .children(children)
                    .refine_style(&style)
            })
            .into_any_element()
    }
}

impl Popover {
    fn render_sheet(self, window: &mut Window, cx: &mut App) -> AnyElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, cx| {
            PopoverState::new(self.default_open, cx)
        });
        state.update(cx, |state, cx| {
            state.track_focus(self.tracked_focus);
            state.set_on_open_change(self.on_open_change);
            if let Some(open) = self.open {
                state.sync_open(open, window, cx);
            }
        });
        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let outside = dismissal.clone();
        let closable = self.overlay_closable;
        let open = state.read(cx).is_open();
        // Keep the sheet mounted through its fade-out before dismissal.
        let presence = gpui_base::motion::Presence::new(
            gpui::SharedString::from(format!("{:?}-sheet", self.id)),
            open,
        )
        .transition(gpui_base::motion::Transition::new(
            std::time::Duration::from_millis(180),
        ))
        .sample(window, cx);
        let progress = presence.progress;
        let parent = window.current_view();
        let toggle = state.clone();
        let mut root = div()
            .id(self.id)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                toggle.update(cx, |state, cx| state.toggle_open(window, cx));
                cx.notify(parent);
            })
            .when_some(self.trigger, |el, trigger| {
                el.child(trigger(open, window, cx))
            })
            .child(dismissal.release_listener());
        if presence.should_render() {
            let close = state.clone();
            let backdrop = state.clone();
            let focus = state.read(cx).focus_handle(cx);
            let content = self
                .content
                .map(|content| state.update(cx, |state, cx| content(state, window, cx)));
            let surface = gpui_base::v_flex()
                .id("touch-picker-sheet")
                .role(Role::Group)
                .aria_label(self.sheet_title.clone().unwrap_or_default())
                .occlude()
                .track_focus(&focus)
                .key_context("Popover")
                .on_action(window.listener_for(&state, PopoverState::on_action_cancel))
                .when(closable, |surface| {
                    surface.on_mouse_down_out(move |event, window, cx| {
                        outside.consume(event, window, cx);
                        backdrop.update(cx, |state, cx| state.dismiss(window, cx));
                        cx.notify(parent);
                    })
                })
                // Offset animation would move the hit targets, so fade with the
                // backdrop while keeping the sheet in its final position.
                .opacity(progress)
                .w_full()
                .max_h(window.viewport_size().height - px(64.))
                .rounded_t(crate::material::radius_overlay_sheet())
                .bg(cx.theme().popover)
                .border_t_1()
                .border_color(cx.theme().border)
                .shadow_xl()
                .text_color(cx.theme().foreground)
                .child(crate::material::sheet_grabber(cx))
                .child(
                    gpui_base::h_flex()
                        .h(px(48.))
                        .flex_none()
                        .px(px(16.))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(17.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .children(self.sheet_title),
                        )
                        .child(
                            div()
                                .id("touch-picker-close")
                                .role(Role::Button)
                                .aria_label(crate::tr!("mobile.cancel"))
                                .min_w(px(44.))
                                .h(px(44.))
                                .flex()
                                .items_center()
                                .justify_end()
                                .cursor_pointer()
                                .text_size(px(15.))
                                .text_color(cx.theme().foreground)
                                .child(crate::tr!("mobile.cancel"))
                                .on_click(move |_, window, cx| {
                                    close.update(cx, |state, cx| state.dismiss(window, cx));
                                    cx.notify(parent);
                                }),
                        ),
                )
                .child(crate::material::faded_hairline(cx))
                .child(
                    div()
                        .id("touch-picker-content")
                        .min_h_0()
                        .touch_overflow_y_scroll()
                        .p(px(16.))
                        .pb(px(32.))
                        .children(content)
                        .children(self.children),
                );
            root = root.child(
                deferred(
                    anchored().position(point(px(0.), px(0.))).child(
                        div()
                            .id("touch-picker-backdrop")
                            .occlude()
                            .w(window.viewport_size().width)
                            .h(window.viewport_size().height)
                            .bg(crate::material::scrim(progress, cx))
                            .flex()
                            .flex_col()
                            .justify_end()
                            .child(surface),
                    ),
                )
                .with_priority(gpui_base::POPUP_PRIORITY),
            );
        }
        root.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext};
    use std::cell::Cell;

    struct SheetHarness(Rc<Cell<usize>>);
    impl Render for SheetHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let selected = self.0.clone();
            div().size_full().flex().flex_col().justify_end().child(
                div().h(px(44.)).w_full().overflow_hidden().child(
                    Popover::new("sheet-test")
                        .bottom_sheet("Model")
                        .default_open(false)
                        .trigger(
                            crate::widgets::button::Button::new("trigger")
                                .label("Model")
                                .debug_selector(|| "sheet-trigger".into()),
                        )
                        .content(move |_, _, _| {
                            div()
                                .id("sheet-option")
                                .debug_selector(|| "sheet-option".into())
                                .h(px(48.))
                                .w_full()
                                .child("Choose")
                                .on_click(move |_, _, _| selected.set(selected.get() + 1))
                        }),
                ),
            )
        }
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Popover,
        Sheet,
        Dropdown,
    }
    struct OutsideHarness {
        kind: Kind,
        clicks: Rc<Cell<usize>>,
        releases: Rc<Cell<usize>>,
    }
    impl Render for OutsideHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            use crate::widgets::menu::DropdownMenu as _;
            let button = crate::widgets::Button::new("open")
                .label("Open")
                .debug_selector(|| "outside-trigger".into());
            let trigger = match self.kind {
                Kind::Dropdown => button
                    .dropdown_menu(|menu, _, _| {
                        menu.menu("Choose", Box::new(gpui_base::actions::Cancel))
                    })
                    .into_any_element(),
                kind => Popover::new("outside-popover")
                    .when(matches!(kind, Kind::Sheet), |popover| {
                        popover.bottom_sheet("Choose")
                    })
                    .trigger(button)
                    .content(|_, _, _| div().size(px(60.)).child("Option"))
                    .into_any_element(),
            };
            let clicks = self.clicks.clone();
            let releases = self.releases.clone();
            div().size_full().child(trigger).child(
                div()
                    .id("outside-target")
                    .absolute()
                    .top(px(300.))
                    .left(px(280.))
                    .size(px(60.))
                    .debug_selector(|| "outside-target".into())
                    .on_mouse_up(MouseButton::Left, move |_, _, _| {
                        releases.set(releases.get() + 1)
                    })
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
            )
        }
    }

    #[gpui::test]
    fn all_popovers_consume_dismissal_release_even_after_redraw(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for kind in [Kind::Popover, Kind::Sheet, Kind::Dropdown] {
            for redraw in [false, true] {
                let clicks = Rc::new(Cell::new(0));
                let releases = Rc::new(Cell::new(0));
                let (_, cx) = cx.add_window_view({
                    let clicks = clicks.clone();
                    let releases = releases.clone();
                    move |_, _| OutsideHarness {
                        kind,
                        clicks,
                        releases,
                    }
                });
                cx.simulate_resize(gpui::size(px(393.), px(852.)));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let trigger = cx.debug_bounds("outside-trigger").unwrap().center();
                cx.simulate_click(trigger, Default::default());
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let position = cx.debug_bounds("outside-target").unwrap().center();
                cx.simulate_event(gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                });
                if redraw {
                    cx.update(|window, cx| window.draw(cx).clear(cx));
                }
                cx.simulate_event(gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                });
                assert_eq!(clicks.get(), 0);
                assert_eq!(
                    releases.get(),
                    0,
                    "dismissed popup must retain its release guard"
                );
                cx.run_until_parked();
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.simulate_click(position, Default::default());
                assert_eq!(clicks.get(), 1);
                assert_eq!(releases.get(), 1);
            }
        }
    }

    #[gpui::test]
    fn bottom_sheet_options_receive_clicks_above_the_backdrop(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let selected = Rc::new(Cell::new(0));
        let (_, window) = cx.add_window_view({
            let selected = selected.clone();
            move |_, _| SheetHarness(selected)
        });
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));
        let trigger = window.debug_bounds("sheet-trigger").expect("trigger");
        window.simulate_click(trigger.center(), gpui::Modifiers::default());
        // Let the 180ms present transition finish so the sheet is at its
        // resting position before the option is hit-tested.
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = window
            .debug_bounds("sheet-option")
            .expect("sheet option is laid out");
        window.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(selected.get(), 1);
    }
}
