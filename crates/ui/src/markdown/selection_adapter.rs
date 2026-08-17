use std::{cell::RefCell, ops::RangeInclusive, rc::Rc, time::Duration};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, EntityId, Hitbox, Modifiers, Pixels,
    PlatformInput, Point, ScrollDelta, ScrollWheelEvent, Task, TextLayout, WeakEntity, Window,
    point, px,
};
use gpui_base::{
    TextSelectionContentKey, TextSelectionCoverage, TextSelectionEndpoint, TextSelectionEvent,
    TextSelectionHandle, TextSelectionRegistration, TextSelectionRun, TextSelectionSnapshot,
};

use super::MarkdownState;

#[derive(Clone, Copy, Debug, PartialEq)]
struct CachedBlockEndpoint {
    endpoint: TextSelectionEndpoint,
    block_ix: Option<usize>,
}

#[derive(Default)]
struct VirtualBlockSelection {
    anchor: Option<CachedBlockEndpoint>,
    cursor: Option<CachedBlockEndpoint>,
    coverage: TextSelectionCoverage,
}

impl VirtualBlockSelection {
    fn update(&mut self, snapshot: Option<TextSelectionSnapshot>, entity_id: EntityId) {
        let Some(snapshot) = snapshot else {
            *self = Self::default();
            return;
        };
        self.coverage = snapshot.coverage();
        Self::update_endpoint(&mut self.anchor, snapshot.anchor(), entity_id);
        Self::update_endpoint(&mut self.cursor, snapshot.cursor(), entity_id);
    }

    fn update_endpoint(
        cached: &mut Option<CachedBlockEndpoint>,
        endpoint: TextSelectionEndpoint,
        entity_id: EntityId,
    ) {
        if cached.is_some_and(|cached| cached.endpoint == endpoint) {
            return;
        }
        let block_ix = (endpoint.entity_id() == Some(entity_id))
            .then(|| endpoint.content_key().map(|key| key.value() as usize))
            .flatten();
        *cached = Some(CachedBlockEndpoint { endpoint, block_ix });
    }

    fn block_range(&self, entity_id: EntityId, last: usize) -> Option<RangeInclusive<usize>> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        match self.coverage {
            TextSelectionCoverage::Full => Some(0..=last),
            TextSelectionCoverage::FromStart => Some(0..=anchor.block_ix.or(cursor.block_ix)?),
            TextSelectionCoverage::ToEnd => Some(anchor.block_ix.or(cursor.block_ix)?..=last),
            TextSelectionCoverage::Bounded => {
                if anchor.endpoint.entity_id() != Some(entity_id)
                    || cursor.endpoint.entity_id() != Some(entity_id)
                {
                    return None;
                }
                let (anchor, cursor) = (anchor.block_ix?, cursor.block_ix?);
                Some(anchor.min(cursor)..=anchor.max(cursor))
            }
        }
    }
}

/// Markdown's renderer-specific bridge to base-owned window selection.
#[derive(Clone)]
pub(super) struct MarkdownSelectionAdapter {
    pub(super) selection: TextSelectionHandle,
    frame: Rc<RefCell<FrameSelectionGeometry>>,
    auto_scroll: Rc<RefCell<AutoScrollState>>,
    layout_revision: Option<usize>,
}

#[derive(Default)]
struct FrameSelectionGeometry {
    text_bounds: Vec<Bounds<Pixels>>,
    runs: Vec<TextSelectionRun>,
}

#[derive(Default)]
struct AutoScrollState {
    window: Option<AnyWindowHandle>,
    /// The clip region this participant paints under — the scroll viewport it
    /// shares with sibling messages, not its own bounds.
    viewport: Option<Bounds<Pixels>>,
    task: Option<Task<()>>,
}

impl MarkdownSelectionAdapter {
    pub(super) fn new(view: WeakEntity<MarkdownState>, cx: &mut App) -> Self {
        let selection = TextSelectionHandle::new("", cx);
        let selection_id = selection.entity_id();
        let virtual_blocks = Rc::new(RefCell::new(VirtualBlockSelection::default()));
        let auto_scroll = Rc::new(RefCell::new(AutoScrollState::default()));

        let view_for_events = view.clone();
        let blocks_for_events = virtual_blocks.clone();
        let auto_scroll_for_events = auto_scroll.clone();
        selection
            .subscribe(
                move |event, cx| match event {
                    TextSelectionEvent::SelectionChanged(snapshot) => {
                        let snapshot = *snapshot;
                        blocks_for_events
                            .borrow_mut()
                            .update(snapshot, selection_id);
                        // The engine's AutoScroll deltas are computed against
                        // this participant's registered bounds — one chat
                        // message, not the scroll viewport it shares with its
                        // siblings — so a drag crossing a message edge
                        // mid-screen would start scrolling. Drive the loop
                        // from drag state instead and compute deltas against
                        // the clip viewport each tick.
                        let anchor_dragging = snapshot.is_some_and(|snapshot| {
                            snapshot.is_selecting()
                                && snapshot.anchor().entity_id() == Some(selection_id)
                        });
                        if anchor_dragging {
                            start_auto_scroll(&auto_scroll_for_events, cx);
                        } else {
                            stop_auto_scroll(&auto_scroll_for_events);
                        }
                        let _ = view_for_events.update(cx, |state, cx| {
                            state.is_selecting =
                                snapshot.is_some_and(|snapshot| snapshot.is_selecting());
                            cx.notify();
                        });
                    }
                    TextSelectionEvent::AutoScroll(_) | TextSelectionEvent::Cleared => {}
                },
                cx,
            )
            .detach();

        let view_for_clear = view.clone();
        let blocks_for_clear = virtual_blocks.clone();
        let auto_scroll_for_clear = auto_scroll.clone();
        selection.clear_with(
            move |cx| {
                blocks_for_clear.replace(VirtualBlockSelection::default());
                stop_auto_scroll(&auto_scroll_for_clear);
                let _ = view_for_clear.update(cx, |state, cx| {
                    state.reset_selection_projection();
                    cx.notify();
                });
            },
            cx,
        );

        let view_for_copy = view.clone();
        let blocks_for_copy = virtual_blocks.clone();
        let selection_for_copy = selection.clone();
        selection.copy_with(
            move |cx| {
                let Some(view) = view_for_copy.upgrade() else {
                    return String::new();
                };
                let state = view.read(cx);
                if selection_for_copy.has_local_selection(cx) {
                    return state.rendered_text();
                }
                let last = state.block_count().saturating_sub(1);
                state.selected_text_in(blocks_for_copy.borrow().block_range(selection_id, last))
            },
            cx,
        );

        let view_for_content_key = view.clone();
        selection.resolve_content_key_with(
            move |point, cx| {
                let view = view_for_content_key.upgrade()?;
                view.read(cx)
                    .block_ix_at(point.y)
                    .map(|block| TextSelectionContentKey::new(block as u64))
            },
            cx,
        );

        selection.focus_with(
            move |window, cx| {
                let Some(view) = view.upgrade() else {
                    return;
                };
                let focus_handle = view.read(cx).focus_handle.clone();
                focus_handle.focus(window, cx);
            },
            cx,
        );

        Self {
            selection,
            frame: Rc::default(),
            auto_scroll,
            layout_revision: None,
        }
    }

    pub(super) fn update_layout_revision(&mut self, revision: usize, is_selecting: bool) -> bool {
        let changed = self
            .layout_revision
            .is_some_and(|previous| previous != revision);
        if !changed || !is_selecting {
            self.layout_revision = Some(revision);
        }
        changed && !is_selecting
    }

    pub(super) fn begin_frame(&self) {
        *self.frame.borrow_mut() = FrameSelectionGeometry::default();
    }

    pub(super) fn update_run(
        &self,
        text: impl Into<gpui::SharedString>,
        layout: TextLayout,
        bounds: Bounds<Pixels>,
        text_bounds: Vec<Bounds<Pixels>>,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        let mut frame = self.frame.borrow_mut();
        let document_order = frame.runs.len() as u64;
        frame.text_bounds.extend(text_bounds);
        frame
            .runs
            .push(TextSelectionRun::new(text, layout, bounds).with_document_order(document_order));
        self.selection
            .update_runs(&frame.runs, cx)
            .ranges()
            .last()
            .cloned()
            .flatten()
    }

    pub(super) fn register(
        &self,
        hitbox: Hitbox,
        bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        document_order: u64,
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let mut auto_scroll = self.auto_scroll.borrow_mut();
            auto_scroll.window = Some(window.window_handle());
            auto_scroll.viewport = Some(hitbox.content_mask.bounds);
        }
        self.selection.register(
            TextSelectionRegistration::new(hitbox, bounds)
                .with_scroll_offset(scroll_offset)
                .with_document_order(document_order)
                .with_text_bounds(self.frame.borrow().text_bounds.clone()),
            window,
            cx,
        );
    }

    pub(super) fn participates_in_selection(&self, cx: &App) -> bool {
        let id = self.selection.entity_id();
        self.selection.snapshot(cx).is_some_and(|snapshot| {
            snapshot.anchor().entity_id() == Some(id) || snapshot.cursor().entity_id() == Some(id)
        })
    }

    pub(super) fn has_selection_snapshot(&self, cx: &App) -> bool {
        self.selection.snapshot(cx).is_some()
    }
}

fn start_auto_scroll(state: &Rc<RefCell<AutoScrollState>>, cx: &mut App) {
    if state.borrow().task.is_some() {
        return;
    }

    let state_for_task = state.clone();
    let task = cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let sample = cx.update(|_| {
                let state = state_for_task.borrow();
                Some((state.window?, state.viewport?))
            });
            let Some((handle, viewport)) = sample else {
                break;
            };
            if cx
                .update_window(handle, |_, window, cx| {
                    let pointer = window.mouse_position();
                    if pointer.x < viewport.left() || pointer.x > viewport.right() {
                        return;
                    }
                    let Some(delta) = gpui_base::AutoScroll::compute_delta(pointer.y, viewport)
                    else {
                        return;
                    };
                    let position = clamp_auto_scroll_position(pointer, viewport);
                    window.dispatch_event(
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(point(px(0.), -delta)),
                            modifiers: Modifiers::default(),
                            ..Default::default()
                        }),
                        cx,
                    );
                })
                .is_err()
            {
                break;
            }
        }
    });

    state.borrow_mut().task = Some(task);
}

fn stop_auto_scroll(state: &Rc<RefCell<AutoScrollState>>) {
    state.borrow_mut().task = None;
}

fn clamp_auto_scroll_position(pointer: Point<Pixels>, bounds: Bounds<Pixels>) -> Point<Pixels> {
    point(
        pointer
            .x
            .max(bounds.left() + px(1.))
            .min(bounds.right() - px(1.)),
        pointer
            .y
            .max(bounds.top() + px(1.))
            .min(bounds.bottom() - px(1.)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        Styled as _, TestAppContext, VisualTestContext, div,
    };
    use gpui_base::TextSelectionLayer;

    use crate::markdown::{MarkdownState, MarkdownView};

    struct AutoScrollRoot {
        markdown: Entity<MarkdownState>,
        wheel_deltas: Rc<RefCell<Vec<Pixels>>>,
    }

    impl AutoScrollRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx.new(|cx| MarkdownState::new("selectable text", cx)),
                wheel_deltas: Rc::default(),
            }
        }
    }

    impl Render for AutoScrollRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let wheel_deltas = self.wheel_deltas.clone();
            div()
                .size_full()
                .child(TextSelectionLayer)
                .child(MarkdownView::new(&self.markdown).selectable(true))
                .on_scroll_wheel(move |event, window, _| {
                    wheel_deltas
                        .borrow_mut()
                        .push(event.delta.pixel_delta(window.line_height()).y);
                })
        }
    }

    #[gpui::test]
    fn drag_auto_scrolls_at_viewport_edge_not_participant_edge(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| AutoScrollRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let viewport_bottom = cx.update(|window, _| window.viewport_size().height);

        use gpui::{Modifiers, MouseButton};
        cx.simulate_mouse_down(
            point(px(5.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Mid-window, well past the one-line participant's bottom edge: the
        // old per-participant trigger would scroll here; the viewport model
        // must not.
        cx.simulate_mouse_move(
            point(px(5.), viewport_bottom / 2.),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.executor().advance_clock(Duration::from_millis(48));
        cx.run_until_parked();
        let wheel_deltas = view.read_with(cx, |root, _| root.wheel_deltas.borrow().clone());
        assert_eq!(wheel_deltas, Vec::<Pixels>::new());

        // Inside the viewport's bottom trigger zone: the loop must scroll.
        cx.simulate_mouse_move(
            point(px(5.), viewport_bottom - px(8.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.executor().advance_clock(Duration::from_millis(48));
        cx.run_until_parked();
        let wheel_deltas = view.read_with(cx, |root, _| root.wheel_deltas.borrow().clone());
        assert!(
            !wheel_deltas.is_empty() && wheel_deltas.iter().all(|delta| *delta < px(0.)),
            "expected downward auto-scroll, got {wheel_deltas:?}"
        );

        // Releasing the drag stops the loop.
        cx.simulate_mouse_up(
            point(px(5.), viewport_bottom - px(8.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let settled = view.read_with(cx, |root, _| root.wheel_deltas.borrow().len());
        cx.executor().advance_clock(Duration::from_millis(64));
        cx.run_until_parked();
        let wheel_deltas = view.read_with(cx, |root, _| root.wheel_deltas.borrow().clone());
        assert_eq!(wheel_deltas.len(), settled);
    }
}
