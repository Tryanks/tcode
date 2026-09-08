//! Element capture for native touch scrolling. GPUI still owns recognition and inertia.
use std::{collections::HashMap, panic::Location};

use gpui::{
    App, Bounds, Element, ElementId, Entity, Global, GlobalElementId, HitboxBehavior, HitboxId,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, ListState, ParentElement,
    Pixels, Point, ScrollHandle, ScrollWheelEvent, StatefulInteractiveElement, StyleRefinement,
    Styled, TouchDragEvent, TouchPhase, Window, WindowId, px,
};
use gpui_base::input::TextareaState;

#[derive(Clone)]
pub(crate) enum Handle {
    Scroll(ScrollHandle),
    List(ListState),
    Textarea(Entity<TextareaState>),
}

impl Handle {
    fn apply(&self, delta: Point<Pixels>, cx: &mut App) -> Point<Pixels> {
        match self {
            Self::Scroll(handle) => {
                let offset = handle.offset() + delta;
                let max = handle.max_offset();
                let next = bounded_offset(offset, max);
                let consumed = next - handle.offset();
                handle.set_offset(next);
                delta - consumed
            }
            Self::List(list) => {
                if delta.y == px(0.) {
                    return delta;
                }
                let old = list.scroll_px_offset_for_scrollbar();
                let next = bounded_offset(old + delta, list.max_offset_for_scrollbar());
                // A following bottom-aligned list uses an end-of-list logical
                // anchor. scroll_by would subtract from that anchor, swallowing
                // pans shorter than the viewport when layout clamps to the end.
                // Let ListState clamp its own padded extent; the scrollbar's
                // maximum excludes list padding.
                list.set_offset_from_scrollbar(old + delta);
                delta - (next - old)
            }
            Self::Textarea(input) => input.update(cx, |input, cx| {
                input.set_scroll_offset(input.scroll_offset() + delta, cx);
                // Textarea clamps after layout, so its limit is not synchronously
                // observable. It keeps exclusive capture rather than guessing.
                Point::default()
            }),
        }
    }
}

fn bounded_offset(offset: Point<Pixels>, max: Point<Pixels>) -> Point<Pixels> {
    Point::new(
        offset.x.clamp(-max.x.max(px(0.)), px(0.)),
        offset.y.clamp(-max.y.max(px(0.)), px(0.)),
    )
}

#[derive(Clone)]
struct Entry {
    id: GlobalElementId,
    parent: Option<GlobalElementId>,
    bounds: Bounds<Pixels>,
    handle: Handle,
    hitbox: Option<HitboxId>,
}

#[derive(Clone)]
struct Capture {
    target: Entry,
    ancestor: Option<Entry>,
    momentum: bool,
    textarea_pending: Option<(u64, Point<Pixels>)>,
}
impl Capture {
    fn apply(&mut self, delta: Point<Pixels>, frame: u64, cx: &mut App) {
        if let Handle::Textarea(input) = &self.target.handle {
            let offset = self
                .textarea_pending
                .filter(|(epoch, _)| *epoch == frame)
                .map(|(_, offset)| offset)
                .unwrap_or_else(|| input.read(cx).scroll_offset())
                + delta;
            input.update(cx, |input, cx| input.set_scroll_offset(offset, cx));
            self.textarea_pending = Some((frame, offset));
            return;
        }
        let remaining = self.target.handle.apply(delta, cx);
        if remaining != Point::default()
            && let Some(ancestor) = self.ancestor.take()
        {
            self.target = ancestor;
            self.apply(remaining, frame, cx);
        }
    }
}

#[derive(Default)]
struct Registry {
    entries: Vec<Entry>,
    parents: Vec<GlobalElementId>,
    frame: u64,
    pending: Option<Capture>,
    active: Option<Capture>,
    momentum: Option<Capture>,
}

impl Registry {
    fn hit_test(&self, position: Point<Pixels>) -> Option<Capture> {
        let target = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.bounds.contains(&position))?
            .clone();
        let ancestor = target
            .parent
            .as_ref()
            .and_then(|id| self.entries.iter().find(|entry| &entry.id == id))
            .cloned();
        Some(Capture {
            target,
            ancestor,
            momentum: false,
            textarea_pending: None,
        })
    }

    fn start(&mut self, position: Point<Pixels>) {
        self.active = None;
        self.momentum = None;
        self.pending = self.hit_test(position);
    }

    fn take_scroll(&mut self, event: &ScrollWheelEvent) -> Option<Capture> {
        if event.touch_phase == TouchPhase::Cancelled {
            self.pending = None;
        }
        if event.touch_phase == TouchPhase::Started {
            self.active = self
                .pending
                .take()
                .or_else(|| self.hit_test(event.position));
            self.momentum = None;
        }
        self.active.take().or_else(|| self.momentum.take())
    }

    fn finish_scroll(&mut self, mut capture: Capture, phase: TouchPhase) {
        match phase {
            TouchPhase::Cancelled => {
                self.pending = None;
                self.active = None;
                self.momentum = None;
            }
            // The same captured handle follows GPUI's frame-driven inertia;
            // the next touch replaces it before any new pan can use it.
            TouchPhase::Ended => {
                self.pending = None;
                if !capture.momentum {
                    capture.momentum = true;
                    self.momentum = Some(capture);
                }
            }
            _ if capture.momentum => self.momentum = Some(capture),
            _ => self.active = Some(capture),
        }
    }
}

#[derive(Default)]
struct TouchScroll(HashMap<WindowId, Registry>);
impl Global for TouchScroll {}

fn registry<'a>(window: &Window, cx: &'a mut App) -> &'a mut Registry {
    cx.default_global::<TouchScroll>()
        .0
        .entry(window.window_handle().window_id())
        .or_default()
}

fn discard_occluded(window: &Window, cx: &mut App) {
    // Menus and dialogs can occlude an otherwise matching viewport. Consult
    // GPUI's hit test only at gesture start, never to retarget an active pan.
    registry(window, cx).entries.retain(|entry| {
        entry
            .hitbox
            .is_some_and(|hitbox| hitbox.should_handle_scroll(window))
    });
}

fn install(window: &mut Window) {
    if !cfg!(any(target_os = "android", target_os = "ios", test)) {
        return;
    }
    window.on_mouse_event(|event: &TouchDragEvent, phase, window, cx| {
        if phase.capture() && event.phase == TouchPhase::Started {
            discard_occluded(window, cx);
            registry(window, cx).start(event.start_position);
        }
    });
    window.on_mouse_event(|event: &ScrollWheelEvent, phase, window, cx| {
        if phase.capture() && event.touch_phase == TouchPhase::Started {
            discard_occluded(window, cx);
        }
        if phase.capture()
            && let Some(mut capture) = registry(window, cx).take_scroll(event)
        {
            let frame = registry(window, cx).frame;
            capture.apply(event.delta.pixel_delta(window.line_height()), frame, cx);
            registry(window, cx).finish_scroll(capture, event.touch_phase);
            window.refresh();
            cx.stop_propagation();
        }
    });
}

/// A transparent element wrapper: registration precedes child prepaint, so the
/// last matching entry is the innermost viewport. Captures retain handles, not bounds.
pub(crate) struct Registered<E: Element> {
    element: Option<E>,
    id: ElementId,
    handle: Option<Handle>,
    prepare: Option<fn(E, &ScrollHandle) -> E>,
    root: bool,
}

#[track_caller]
pub(crate) fn register<E: IntoElement>(element: E, handle: Handle) -> Registered<E::Element> {
    let element = element.into_element();
    let id = element
        .id()
        .unwrap_or(ElementId::CodeLocation(*Location::caller()));
    Registered {
        element: Some(element),
        id,
        handle: Some(handle),
        prepare: None,
        root: false,
    }
}

pub(crate) fn root<E: IntoElement>(element: E) -> Registered<E::Element> {
    Registered {
        element: Some(element.into_element()),
        id: "touch-scroll-root".into(),
        handle: None,
        prepare: None,
        root: true,
    }
}

pub(crate) trait TouchScrollExt: StatefulInteractiveElement + Element + Sized {
    #[track_caller]
    fn touch_overflow_x_scroll(self) -> Registered<Self> {
        let id = Element::id(&self).unwrap_or(ElementId::CodeLocation(*Location::caller()));
        Registered {
            element: Some(self.overflow_x_scroll()),
            id,
            handle: None,
            prepare: Some(|element, handle| element.track_scroll(handle)),
            root: false,
        }
    }
    #[track_caller]
    fn touch_overflow_y_scroll(self) -> Registered<Self> {
        let id = Element::id(&self).unwrap_or(ElementId::CodeLocation(*Location::caller()));
        Registered {
            element: Some(self.overflow_y_scroll()),
            id,
            handle: None,
            prepare: Some(|element, handle| element.track_scroll(handle)),
            root: false,
        }
    }
}
impl<E: StatefulInteractiveElement + Element> TouchScrollExt for E {}

impl<E: Element> Registered<E> {
    pub(crate) fn with_id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }
}

impl<E: Element> IntoElement for Registered<E> {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl<E: Element + Styled> Styled for Registered<E> {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.as_mut().unwrap().style()
    }
}
impl<E: Element + ParentElement> ParentElement for Registered<E> {
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.as_mut().unwrap().extend(elements);
    }
}
impl<E: Element + InteractiveElement> InteractiveElement for Registered<E> {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.as_mut().unwrap().interactivity()
    }
}
impl<E: Element + StatefulInteractiveElement> StatefulInteractiveElement for Registered<E> {
    fn track_scroll(mut self, handle: &ScrollHandle) -> Self {
        self.element = Some(self.element.take().unwrap().track_scroll(handle));
        self.handle = Some(Handle::Scroll(handle.clone()));
        self.prepare = None;
        self
    }
}

impl<E: Element> Element for Registered<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if let Some(prepare) = self.prepare.take() {
            let handle = window
                .use_keyed_state("touch-scroll-handle", cx, |_, _| ScrollHandle::new())
                .read(cx)
                .clone();
            self.element = Some(prepare(self.element.take().unwrap(), &handle));
            self.handle = Some(Handle::Scroll(handle));
        }
        self.element
            .as_mut()
            .unwrap()
            .request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.root {
            let registry = registry(window, cx);
            registry.entries.clear();
            registry.frame = registry.frame.wrapping_add(1);
            registry.parents.clear();
        }
        let entry_index = registry(window, cx).entries.len();
        if let Some(handle) = &self.handle {
            let bounds = bounds.intersect(&window.content_mask().bounds);
            let registry = registry(window, cx);
            let id = id.expect("registered scroll element has an id").clone();
            registry.entries.push(Entry {
                id: id.clone(),
                parent: registry.parents.last().cloned(),
                bounds,
                handle: handle.clone(),
                hitbox: None,
            });
            registry.parents.push(id);
        }
        let result = self
            .element
            .as_mut()
            .unwrap()
            .prepaint(id, inspector, bounds, layout, window, cx);
        if self.handle.is_some() {
            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
            let registry = registry(window, cx);
            registry.entries[entry_index].hitbox = Some(hitbox.id);
            registry.parents.pop();
        }
        result
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.root {
            install(window);
        }
        self.element
            .as_mut()
            .unwrap()
            .paint(id, inspector, bounds, layout, prepaint, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, ScrollDelta, point, size};

    fn id(name: &'static str) -> GlobalElementId {
        let mut id = GlobalElementId::default();
        *id = vec![name.into()].into();
        id
    }
    fn entry(name: &'static str, parent: Option<&'static str>, y: f32, height: f32) -> Entry {
        Entry {
            id: id(name),
            parent: parent.map(id),
            bounds: Bounds::new(point(px(0.), px(y)), size(px(100.), px(height))),
            handle: Handle::Scroll(ScrollHandle::new()),
            hitbox: None,
        }
    }
    fn scroll(phase: TouchPhase, y: f32) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(50.), px(y)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-20.))),
            modifiers: Modifiers::default(),
            touch_phase: phase,
        }
    }

    #[test]
    fn captures_innermost_at_down_even_when_child_moves_under_anchor() {
        let mut registry = Registry {
            entries: vec![
                entry("page", None, 0., 500.),
                entry("textarea", Some("page"), 200., 80.),
            ],
            ..Default::default()
        };
        assert_eq!(
            registry
                .hit_test(point(px(50.), px(220.)))
                .unwrap()
                .target
                .id,
            id("textarea")
        );
        registry.start(point(px(50.), px(50.)));
        // A new frame moves the textarea beneath the initial touch point.
        registry.entries[1].bounds.origin.y = px(30.);
        let capture = registry
            .take_scroll(&scroll(TouchPhase::Started, 50.))
            .unwrap();
        assert_eq!(capture.target.id, id("page"));
        registry.finish_scroll(capture, TouchPhase::Started);
        let capture = registry
            .take_scroll(&scroll(TouchPhase::Moved, 230.))
            .unwrap();
        assert_eq!(capture.target.id, id("page"));
        registry.finish_scroll(capture, TouchPhase::Moved);
        registry.start(point(px(50.), px(50.)));
        assert!(registry.active.is_none());
        assert_eq!(
            registry
                .take_scroll(&scroll(TouchPhase::Started, 50.))
                .unwrap()
                .target
                .id,
            id("textarea")
        );
    }

    #[test]
    fn release_retains_only_momentum_and_cancel_or_next_gesture_clears_it() {
        for end in [TouchPhase::Ended, TouchPhase::Cancelled] {
            let mut registry = Registry::default();
            registry.entries.push(entry("page", None, 0., 500.));
            registry.start(point(px(50.), px(50.)));
            let capture = registry
                .take_scroll(&scroll(TouchPhase::Started, 50.))
                .unwrap();
            registry.finish_scroll(capture, end);
            assert!(registry.active.is_none());
            assert!(registry.pending.is_none());
            if end == TouchPhase::Ended {
                let capture = registry
                    .take_scroll(&scroll(TouchPhase::Moved, 400.))
                    .unwrap();
                assert_eq!(capture.target.id, id("page"));
                registry.finish_scroll(capture, TouchPhase::Moved);
                let capture = registry
                    .take_scroll(&scroll(TouchPhase::Ended, 400.))
                    .unwrap();
                registry.finish_scroll(capture, TouchPhase::Ended);
            }
            assert!(registry.momentum.is_none());
            registry.start(point(px(500.), px(500.)));
            assert!(
                registry
                    .take_scroll(&scroll(TouchPhase::Moved, 50.))
                    .is_none()
            );
        }
    }

    #[gpui::test]
    fn at_limit_chains_once_to_registered_parent(cx: &mut gpui::TestAppContext) {
        let registry = Registry {
            entries: vec![
                entry("grandparent", None, 0., 500.),
                entry("parent", Some("grandparent"), 0., 300.),
                entry("child", Some("parent"), 10., 50.),
            ],
            ..Default::default()
        };
        let mut capture = registry.hit_test(point(px(20.), px(20.))).unwrap();
        // Default handles have zero overflow: the child's requested delta is
        // wholly unconsumed and must go to its actual registered parent once.
        cx.update(|cx| capture.apply(point(px(0.), px(-20.)), 1, cx));
        assert_eq!(capture.target.id, id("parent"));
        assert!(capture.ancestor.is_none());
        cx.update(|cx| capture.apply(point(px(0.), px(-20.)), 1, cx));
        assert_eq!(capture.target.id, id("parent"));
    }
    struct PanProbe {
        page: ScrollHandle,
        child: ScrollHandle,
    }
    impl gpui::Render for PanProbe {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            use gpui::{ParentElement as _, Styled as _, div};
            root(
                div()
                    .id("page")
                    .w(px(200.))
                    .h(px(300.))
                    .touch_overflow_y_scroll()
                    .track_scroll(&self.page)
                    .child(div().h(px(240.)).flex_none())
                    .child(
                        div()
                            .id("child")
                            .h(px(80.))
                            .flex_none()
                            .touch_overflow_y_scroll()
                            .track_scroll(&self.child)
                            .child(div().h(px(600.)).flex_none()),
                    )
                    .child(div().h(px(600.)).flex_none()),
            )
        }
    }

    #[gpui::test]
    fn native_pan_keeps_page_when_child_moves_under_anchor(cx: &mut gpui::TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId};
        let (view, cx) = cx.add_window_view(|_, _| PanProbe {
            page: ScrollHandle::new(),
            child: ScrollHandle::new(),
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let (page, child) = view.read_with(cx, |view, _| (view.page.clone(), view.child.clone()));
        assert!(page.max_offset().y > px(300.));
        assert!(child.max_offset().y > px(300.));
        let send = |phase, y, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(1),
                        phase,
                        position: point(px(50.), px(y)),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
                let _ = window.draw(cx);
            });
        };
        send(TouchPhase::Started, 50., cx);
        send(TouchPhase::Moved, -150., cx);
        assert_eq!(page.offset().y, px(-200.));
        assert!(child.bounds().contains(&point(px(50.), px(50.))));
        send(TouchPhase::Moved, -170., cx);
        assert_eq!(page.offset().y, px(-220.));
        assert_eq!(child.offset().y, px(0.));
        send(TouchPhase::Cancelled, -170., cx);
        page.set_offset(Point::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        send(TouchPhase::Started, 260., cx);
        send(TouchPhase::Moved, 210., cx);
        assert_eq!(child.offset().y, px(-50.));
        assert_eq!(page.offset().y, px(0.));
        send(TouchPhase::Moved, -310., cx);
        assert_eq!(child.offset().y, -child.max_offset().y);
        assert_eq!(page.offset().y, px(-50.));
        send(TouchPhase::Cancelled, -310., cx);
    }
    struct OccludedProbe(ScrollHandle);
    impl gpui::Render for OccludedProbe {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            use gpui::div;
            root(
                div()
                    .relative()
                    .w(px(200.))
                    .h(px(300.))
                    .child(
                        div()
                            .id("page")
                            .size_full()
                            .touch_overflow_y_scroll()
                            .track_scroll(&self.0)
                            .child(div().h(px(600.))),
                    )
                    .child(div().absolute().inset_0().occlude()),
            )
        }
    }

    #[gpui::test]
    fn an_occluding_overlay_does_not_capture_the_page_behind_it(cx: &mut gpui::TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId};
        let (view, cx) = cx.add_window_view(|_, _| OccludedProbe(ScrollHandle::new()));
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.update(|window, cx| {
            for (phase, y) in [(TouchPhase::Started, 50.), (TouchPhase::Moved, -150.)] {
                window.dispatch_event(
                    PlatformInput::Touch(TouchEvent {
                        id: TouchId(1),
                        phase,
                        position: point(px(50.), px(y)),
                        predicted_position: None,
                        force: None,
                    }),
                    cx,
                );
            }
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.0.offset()),
            Point::default()
        );
    }
}
