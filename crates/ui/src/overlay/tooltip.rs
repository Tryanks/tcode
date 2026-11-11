//! The window's tooltip layer.
//!
//! gpui-base's `TooltipOverlay` drops its content the frame it hides and
//! exposes neither its state nor a presence hook, and GPUI's own `.tooltip()`
//! drops the tooltip view the moment it hides, so neither lets a tooltip
//! leave. This host repeats `TooltipOverlay`'s lifecycle — the show delay and
//! grace period of [`TooltipDefaults`], the immediate switch while one is
//! shown or was just shown, no hover tooltips on a phone — and keeps what it
//! hid drawn, inert, until its exit has played. Placement is gpui-base's
//! [`TooltipPositioner`].

use std::rc::Rc;

use gpui::{
    App, Bounds, Context, Entity, EntityId, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, Styled as _, Subscription, Task, Window, deferred, div,
};
use gpui_base::{TooltipDefaults, TooltipPositioner};

use crate::widgets::Tooltip;

/// How long a surface drawn here takes to appear, and to leave.
const APPEAR: std::time::Duration = std::time::Duration::from_millis(200);

/// Above gpui-base's popups, the layer its `TooltipOverlay` draws on.
const TOOLTIP_PRIORITY: usize = gpui_base::POPUP_PRIORITY + 100;

pub(crate) type TooltipBuilder = Rc<dyn Fn(&mut Window, &mut App) -> Tooltip>;

/// The identity of one tooltip trigger, kept in the trigger's element state:
/// it is released when the trigger stops being drawn, which hides its tooltip.
#[derive(Default)]
pub(crate) struct TooltipOwner {
    /// Which part of the trigger the pointer is over, for a trigger with
    /// several (the links of a paragraph); `0` for a whole element.
    pub(crate) hovered: std::cell::Cell<Option<usize>>,
}

struct Request {
    owner: (EntityId, usize),
    trigger_bounds: Bounds<Pixels>,
    build: TooltipBuilder,
}

struct Shown {
    id: usize,
    owner: (EntityId, usize),
    trigger_bounds: Bounds<Pixels>,
    tooltip: Tooltip,
}

pub(crate) struct TooltipHost {
    enabled: bool,
    shown: Option<Shown>,
    leaving: Vec<Shown>,
    owner: Option<(EntityId, Subscription)>,
    had_recent_tooltip: bool,
    epoch: usize,
    next_id: usize,
    show_task: Option<Task<()>>,
    hide_task: Option<Task<()>>,
}

impl TooltipHost {
    pub(crate) fn new(_: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            enabled: !gpui_base::is_mobile(),
            shown: None,
            leaving: Vec::new(),
            owner: None,
            had_recent_tooltip: false,
            epoch: 0,
            next_id: 0,
            show_task: None,
            hide_task: None,
        }
    }

    /// The host of `window`, if it has a Base root.
    pub(crate) fn of(window: &Window, cx: &App) -> Option<Entity<Self>> {
        window
            .root::<gpui_base::Root>()
            .flatten()?
            .read(cx)
            .plugin::<Self>()
    }

    /// The pointer entered `part` of `owner`: show its tooltip after the show
    /// delay, or at once while another is shown or was just hidden.
    pub(crate) fn request_show(
        &mut self,
        owner: &Entity<TooltipOwner>,
        part: usize,
        trigger_bounds: Bounds<Pixels>,
        build: TooltipBuilder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        self.hide_task = None;
        self.adopt(owner, cx);
        let request = Request {
            owner: (owner.entity_id(), part),
            trigger_bounds,
            build,
        };
        let show_delay = TooltipDefaults::global(cx).show_delay();
        if self.shown.is_some() || self.had_recent_tooltip || show_delay.is_zero() {
            self.show_task = None;
            self.show(request, window, cx);
            return;
        }
        let epoch = self.next_epoch();
        self.show_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(show_delay).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.epoch == epoch {
                    this.show(request, window, cx);
                }
            });
        }));
    }

    /// The pointer left `owner`: its tooltip stays for the grace period, and
    /// a trigger entered within it shows its own at once.
    pub(crate) fn request_hide(
        &mut self,
        owner: &Entity<TooltipOwner>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Another trigger may have taken over in the same pointer move,
        // before this one heard that the pointer left it.
        if !self.owns(owner.entity_id()) {
            return;
        }
        self.show_task = None;
        if self.shown.is_none() {
            return;
        }
        let epoch = self.next_epoch();
        let grace_period = TooltipDefaults::global(cx).grace_period();
        self.had_recent_tooltip = true;
        self.hide_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(grace_period).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.epoch == epoch {
                    this.leave();
                    this.had_recent_tooltip = false;
                    cx.notify();
                }
            });
        }));
    }

    /// Hide at once, without a grace period: a press, a scroll, or the
    /// trigger gone.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        let changed = self.shown.is_some()
            || self.had_recent_tooltip
            || self.show_task.is_some()
            || self.hide_task.is_some();
        self.leave();
        self.owner = None;
        self.had_recent_tooltip = false;
        self.show_task = None;
        self.hide_task = None;
        if changed {
            cx.notify();
        }
    }

    fn owns(&self, owner: EntityId) -> bool {
        self.owner.as_ref().is_some_and(|(id, _)| *id == owner)
    }

    fn adopt(&mut self, owner: &Entity<TooltipOwner>, cx: &mut Context<Self>) {
        let id = owner.entity_id();
        if self.owns(id) {
            return;
        }
        let released = cx.observe_release(owner, move |this, _, cx| {
            if this.owns(id) {
                this.hide(cx);
            }
        });
        self.owner = Some((id, released));
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn show(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        // Back on the trigger whose tooltip is still up: it stays as it is.
        if self
            .shown
            .as_ref()
            .is_some_and(|shown| shown.owner == request.owner)
        {
            return;
        }
        self.leave();
        let tooltip = (request.build)(window, cx);
        self.next_id += 1;
        self.shown = Some(Shown {
            id: self.next_id,
            owner: request.owner,
            trigger_bounds: request.trigger_bounds,
            tooltip,
        });
        cx.notify();
    }

    fn leave(&mut self) {
        self.leaving.extend(self.shown.take());
    }

    fn layer(
        shown: &Shown,
        progress: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        deferred(
            TooltipPositioner::new(shown.trigger_bounds).child(
                div()
                    .id(("tcode-tooltip", shown.id))
                    .child(shown.tooltip.surface(progress, window, cx)),
            ),
        )
        .with_priority(TOOLTIP_PRIORITY)
    }
}

impl gpui_base::RootPlugin for TooltipHost {}

impl Render for TooltipHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let presence = |shown: &Shown, present: bool, window: &mut Window, cx: &mut App| {
            crate::motion::appear(
                gpui::ElementId::from(("tcode-tooltip-presence", shown.id)),
                present,
                APPEAR,
                window,
                cx,
            )
        };
        let mut leaving = Vec::with_capacity(self.leaving.len());
        for shown in std::mem::take(&mut self.leaving) {
            let sample = presence(&shown, false, window, cx);
            if sample.should_render() {
                leaving.push((shown, sample.progress));
            }
        }
        let mut layers = Vec::with_capacity(leaving.len() + 1);
        for (shown, progress) in &leaving {
            layers.push(Self::layer(shown, *progress, window, cx).into_any_element());
        }
        self.leaving = leaving.into_iter().map(|(shown, _)| shown).collect();
        if let Some(shown) = self.shown.as_ref() {
            let progress = presence(shown, true, window, cx).progress;
            layers.push(Self::layer(shown, progress, window, cx).into_any_element());
        }
        let host = cx.entity();
        div()
            .children(layers)
            // A press or a scroll anywhere in the window hides a tooltip at
            // once, as GPUI's own tooltips do.
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let pressed = host.clone();
                        window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, cx| {
                            if phase.capture() {
                                pressed.update(cx, |host, cx| host.hide(cx));
                            }
                        });
                        let scrolled = host.clone();
                        window.on_mouse_event(move |_: &gpui::ScrollEvent, phase, _, cx| {
                            if phase.capture() {
                                scrolled.update(cx, |host, cx| host.hide(cx));
                            }
                        });
                    },
                )
                .absolute()
                .size(gpui::px(0.)),
            )
    }
}
