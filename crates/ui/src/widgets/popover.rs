use crate::theme::ActiveTheme as _;
use gpui::{
    Anchor, AnyElement, App, Context, ElementId, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement, RenderOnce, StyleRefinement, Styled, Window,
    prelude::FluentBuilder as _,
};
use gpui_base::StyledExt as _;
use std::rc::Rc;

pub use gpui_base::PopoverState;

/// Styled tcode facade over the headless gpui-base popover.
#[derive(IntoElement)]
pub struct Popover {
    id: ElementId,
    anchor: Anchor,
    default_open: bool,
    open: Option<bool>,
    tracked_focus: Option<FocusHandle>,
    trigger: Option<Box<dyn FnOnce(bool, &Window, &App) -> AnyElement>>,
    content: Option<
        Box<dyn FnOnce(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> AnyElement>,
    >,
    children: Vec<AnyElement>,
    style: StyleRefinement,
    mouse_button: MouseButton,
    overlay_closable: bool,
    appearance: bool,
    on_open_change: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
}

impl Popover {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
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
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let appearance = self.appearance;
        let content = self.content;
        let children = self.children;
        let style = self.style;
        gpui_base::Popover::new(self.id)
            .anchor(self.anchor)
            .mouse_button(self.mouse_button)
            .default_open(self.default_open)
            .overlay_closable(self.overlay_closable)
            .when_some(self.open, |base, open| base.open(open))
            .when_some(self.tracked_focus, |base, focus| base.track_focus(&focus))
            .when_some(self.trigger, |base, trigger| base.trigger_with(trigger))
            .when_some(self.on_open_change, |base, callback| {
                base.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .content(move |state, window, cx| {
                gpui_base::v_flex()
                    .id("tcode-popover-content")
                    .occlude()
                    .when(appearance, |el| {
                        el.p_3()
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
    }
}
