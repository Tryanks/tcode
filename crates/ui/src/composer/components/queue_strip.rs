//! The queued messages above the field. Rows drag to change the order the
//! queue sends in: by the row with a mouse, by its handle on touch, where a
//! press on the row scrolls the strip.
//!
//! gpui-base has no sortable-list primitive, so the drag composes GPUI's
//! `on_drag`, `on_drag_move` and `on_drop`, and GPUI does not cancel a drag on
//! Escape by itself. The row it carries is a [`drag_ghost`].

use super::super::*;
use crate::motion::drag_ghost;
use crate::scroll::ScrollableElement as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use gpui::{DragMoveEvent, KeystrokeEvent, MouseButton, Pixels, SharedString, WeakEntity};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// The drag payload; the composer keeps the drag's state in [`QueueDrag`].
pub(in super::super) struct DraggedQueued {
    id: u64,
    label: SharedString,
    compact: bool,
}

pub(in super::super) struct QueueDrag {
    id: u64,
    /// The row the dragged one lands before; `None` is the end.
    before: Option<u64>,
    /// `before` as the drag began, where a drop moves nothing.
    origin: Option<u64>,
    auto_scroll: gpui_base::AutoScroll,
    _escape: Subscription,
}

/// The slot a queued row's ghost lifts from and settles into.
fn queued_slot(id: u64) -> SharedString {
    format!("queued-{id}").into()
}

impl Composer {
    /// The queued messages the strip lists, in queue order: a message whose
    /// send is still on its way to the host shows in the timeline instead.
    fn queued_rows(
        &self,
        cx: &App,
    ) -> Option<(
        crate::store::ComposerQueue,
        Vec<tcode_protocol::QueuedMessageStatus>,
    )> {
        let mut queue = self.composer_state(cx).queue.clone()?;
        let deliveries = self
            .workspace_store
            .read(cx)
            .delivery_messages_for(self.session_id.as_deref(), cx);
        let rows = std::mem::take(&mut queue.messages)
            .into_iter()
            .filter(|message| {
                !deliveries.iter().any(|(key, _, failure, acknowledged)| {
                    !acknowledged
                        && failure.is_none()
                        && message.delivery_key.as_deref() == Some(key.as_str())
                })
            })
            .collect();
        Some((queue, rows))
    }

    fn begin_queue_drag(
        composer: WeakEntity<Self>,
        dragged: &DraggedQueued,
        grab: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<drag_ghost::Carried> {
        let escape = {
            let composer = composer.clone();
            cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && composer
                        .update(cx, |this, cx| {
                            let cancelled = this.queue_drag.take().is_some();
                            if cancelled {
                                cx.stop_active_drag(window);
                                cx.notify();
                            }
                            cancelled
                        })
                        .unwrap_or(false)
                {
                    cx.stop_propagation();
                }
            })
        };
        let _ = composer.update(cx, |this, cx| {
            let origin = this.queued_rows(cx).and_then(|(_, rows)| {
                let index = rows.iter().position(|row| row.id == dragged.id)?;
                rows.get(index + 1).map(|row| row.id)
            });
            this.queue_drag = Some(QueueDrag {
                id: dragged.id,
                before: origin,
                origin,
                auto_scroll: Default::default(),
                _escape: escape,
            });
            cx.notify();
        });
        crate::haptics::threshold();
        let (label, compact) = (dragged.label.clone(), dragged.compact);
        drag_ghost::lift(
            queued_slot(dragged.id),
            grab,
            move |lift, _, cx| {
                drag_ghost::lifted(div(), lift)
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .rounded(crate::material::radius_overlay(cx))
                    .bg(cx.theme().list_active)
                    .text_body(compact, cx)
                    .text_color(cx.theme().foreground)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(label.clone()))
                    .into_any_element()
            },
            window,
            cx,
        )
    }

    /// The pointer is over row `over`, in its lower half when `lower`. The
    /// gap counts the rows other than the dragged one, so a row sliding
    /// through the pointer never moves the gap back.
    fn queue_drag_over(&mut self, over: u64, lower: bool, cx: &mut Context<Self>) {
        let Some(drag) = self.queue_drag.as_ref() else {
            return;
        };
        let mut before = drag.before;
        if over != drag.id {
            let others: Vec<u64> = self
                .queued_rows(cx)
                .map(|(_, rows)| rows)
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.id)
                .filter(|id| *id != drag.id)
                .collect();
            if let Some(index) = others.iter().position(|id| *id == over) {
                before = if lower {
                    others.get(index + 1).copied()
                } else {
                    Some(over)
                };
            }
        }
        let drag = self.queue_drag.as_mut().expect("checked above");
        if drag.before != before {
            crate::haptics::selection();
        }
        if drag.before != before {
            drag.before = before;
            cx.notify();
        }
    }

    /// Scroll the strip under a drag held near its top or bottom edge.
    fn queue_drag_auto_scroll(
        &mut self,
        event: &DragMoveEvent<DraggedQueued>,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.queue_drag.as_mut() else {
            return;
        };
        let position = event.event.position;
        let delta = (event.bounds.left() <= position.x && position.x <= event.bounds.right())
            .then(|| gpui_base::AutoScroll::compute_delta(position.y, event.bounds))
            .flatten();
        drag.auto_scroll.set(delta, cx, |delta, this, cx| {
            let handle = &this.queue_scroll;
            let max = handle.max_offset().y;
            let mut offset = handle.offset();
            offset.y = (offset.y - delta).clamp(-max, px(0.));
            handle.set_offset(offset);
            cx.notify();
        });
    }

    fn drop_queued_row(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.queue_drag.take() else {
            return;
        };
        cx.notify();
        if drag.before != drag.origin {
            let session_id = self.session_id.clone().unwrap_or_default();
            self.workspace_store.update(cx, |store, _cx| {
                store.move_queued_for(session_id, drag.id, drag.before)
            });
        }
    }

    /// Queued messages with edit, send-now and removal actions. Scheduled
    /// messages also show a live countdown. A row sent, dropped or steered
    /// folds away and one queued folds in; rows a drag moves past glide to
    /// their new places; the strip folds in with its first row and away with
    /// its last.
    pub(in super::super) fn render_queue_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // A drag released outside the strip ends without a drop.
        if self.queue_drag.is_some() && !cx.has_active_drag() {
            self.queue_drag = None;
        }
        let (can_steer, agent, mut queued) = match self.queued_rows(cx) {
            Some((queue, queued)) => (queue.can_steer, queue.agent, queued),
            None => (false, "", Vec::new()),
        };
        let shown = self.motion.shown(
            self.motion_key("queue-strip"),
            (!queued.is_empty()).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let reorderable = queued.len() > 1 && self.interactive(cx);
        if let Some(drag) = &self.queue_drag
            && let Some(from) = queued.iter().position(|message| message.id == drag.id)
        {
            let message = queued.remove(from);
            let to = drag
                .before
                .and_then(|before| queued.iter().position(|message| message.id == before))
                .unwrap_or(queued.len());
            queued.insert(to, message);
        }

        let this = &*self;
        let rows = queued
            .into_iter()
            .enumerate()
            .map(|(index, message)| (queued_slot(message.id), (index, message)));
        let strip = crate::motion::keyed::keyed_owned(
            &this.motion,
            this.motion_key("queue-rows"),
            crate::motion::keyed::Fold::Down,
            rows.collect::<Vec<_>>(),
            ROWS,
            window,
            cx,
            |_, (index, message), window, cx| {
                this.render_queued_row(*index, message, reorderable, can_steer, agent, window, cx)
            },
        )
        .render(v_flex().w_full());
        Some(super::super::column_fold(
            &shown,
            div()
                .id("queued-messages-scroll")
                .w_full()
                // A few queued rows before the strip scrolls, so the field
                // under it stays in reach; no owner sizes a composer strip.
                .max_h(px(180.))
                .on_drop(cx.listener(|this, _: &DraggedQueued, _, cx| this.drop_queued_row(cx)))
                .on_drag_move(
                    cx.listener(|this, event: &DragMoveEvent<DraggedQueued>, _, cx| {
                        this.queue_drag_auto_scroll(event, cx)
                    }),
                )
                .overflow_y_scroll_area()
                .track_scroll(&self.queue_scroll)
                .child(strip),
        ))
    }

    /// One queued message's row, the `index`th in the strip; the rows after
    /// the first carry the gap and rule above them.
    #[allow(clippy::too_many_arguments)]
    fn render_queued_row(
        &self,
        index: usize,
        message: &tcode_protocol::QueuedMessageStatus,
        reorderable: bool,
        can_steer: bool,
        agent: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        {
            let message = message.clone();
            let id = message.id;
            let editable = message.editable;
            let text = message.text.clone();
            let label: SharedString = crate::chat::model::one_line(
                &tcode_core::notice::host_notice(&message.text).map_or_else(
                    || message.text.clone(),
                    |notice| crate::chat::components::disclosure::notice_label(&notice),
                ),
            )
            .into();
            let dragged = (reorderable && editable).then(|| DraggedQueued {
                id,
                label: label.clone(),
                compact: self.compact,
            });
            let scheduled = message.fire_at_unix_secs.is_some();
            let steer_tooltip = if scheduled {
                crate::tr!("composer.send_now").into_owned()
            } else if can_steer {
                crate::tr!("composer.steer_queued").into_owned()
            } else {
                crate::tr!("composer.steer_unsupported_tooltip", agent = agent).into_owned()
            };
            let countdown = message.fire_at_unix_secs.map(|fire_at| {
                crate::widgets::clock::Clock::until(
                    ("queued-countdown", id as usize),
                    fire_at,
                    |left| crate::time::format_countdown(left / 1000).into(),
                )
            });
            let row = h_flex()
                .id(("queued-row", id as usize))
                .debug_selector(move || format!("queued-row-{id}"))
                .flex_none()
                .w_full()
                .gap_1()
                .items_center()
                .px_1()
                .when(index > 0, |row| {
                    row.pt_1().border_t_1().border_color(cx.theme().border)
                })
                .on_drag_move::<DraggedQueued>(cx.listener(
                    move |this, event: &DragMoveEvent<DraggedQueued>, _, cx| {
                        if event.bounds.contains(&event.event.position) {
                            let lower = event.event.position.y > event.bounds.center().y;
                            this.queue_drag_over(id, lower, cx);
                        }
                    },
                ));
            let composer = cx.entity().downgrade();
            let begin = move |dragged: &DraggedQueued, grab, window: &mut Window, cx: &mut App| {
                Self::begin_queue_drag(composer.clone(), dragged, grab, window, cx)
            };
            let (row, grip) = match dragged {
                Some(dragged) if self.compact => (
                    row,
                    Some(
                        div()
                            .id(("queue-grip", id as usize))
                            .debug_selector(move || format!("queue-grip-{id}"))
                            .role(Role::Button)
                            .aria_label(crate::tr!("composer.reorder_queued"))
                            .flex_none()
                            .size(px(crate::material::TOUCH_TARGET))
                            .flex()
                            .items_center()
                            .justify_center()
                            // The handle alone starts a drag, so a press must
                            // not reach the row.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Icon::new(IconName::GripVertical)
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_drag(dragged, begin),
                    ),
                ),
                Some(dragged) => (row.cursor_grab().on_drag(dragged, begin), None),
                None => (row, None),
            };
            let row = row
                .children(grip)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(self.compact, cx)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(label)),
                )
                .when_some(countdown, |row, countdown| {
                    row.child(
                        div()
                            .flex_none()
                            .text_secondary(self.compact, cx)
                            .child(countdown),
                    )
                })
                .child(
                    Button::new(("queue-steer", id as usize))
                        .motion(&self.button_motion())
                        .debug_selector(move || format!("queue-steer-{id}"))
                        .ghost()
                        .xsmall()
                        .touch(self.compact)
                        .icon(IconName::ArrowUp)
                        // The row's grab cursor covers its children; a button
                        // keeps Button's own, the default.
                        .cursor_default()
                        // Scheduled rows always support send-now: the
                        // runtime removes the deadline and uses the normal
                        // send/queue path even when native steering is absent.
                        .disabled(!editable || !self.interactive(cx) || (!scheduled && !can_steer))
                        .tooltip(steer_tooltip)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.steer_queued_for(
                                    this.session_id.clone().unwrap_or_default(),
                                    id,
                                )
                            });
                        })),
                )
                .child(
                    Button::new(("queue-drop", id as usize))
                        .motion(&self.button_motion())
                        .debug_selector(move || format!("queue-drop-{id}"))
                        .ghost()
                        .xsmall()
                        .touch(self.compact)
                        .icon(IconName::Close)
                        .cursor_default()
                        .disabled(!editable || !self.interactive(cx))
                        .tooltip(crate::tr!("composer.drop_queued"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.drop_queued_and_refill(id, text.clone(), window, cx);
                        })),
                )
                .context_menu({
                    let text = message.text.clone();
                    let interactive = editable && self.interactive(cx);
                    move |menu, _, _| {
                        menu.menu(
                            crate::tr!("chat.copy_text").into_owned(),
                            Box::new(CopyText(text.clone())),
                        )
                        .separator()
                        .menu_with_enable(
                            if scheduled {
                                crate::tr!("composer.send_now").into_owned()
                            } else {
                                crate::tr!("composer.steer_queued").into_owned()
                            },
                            Box::new(ComposerMenu::QueueSteer(id)),
                            interactive && (scheduled || can_steer),
                        )
                        .menu_with_enable(
                            crate::tr!("composer.edit_queued").into_owned(),
                            Box::new(ComposerMenu::QueueEdit {
                                id,
                                text: text.clone(),
                            }),
                            interactive,
                        )
                        .menu_with_enable(
                            crate::tr!("composer.drop_queued").into_owned(),
                            Box::new(ComposerMenu::QueueDrop(id)),
                            interactive,
                        )
                    }
                });
            let row = drag_ghost::slot(queued_slot(id), row, window, cx);
            div()
                .w_full()
                .when(index > 0, |gap| gap.pt_1())
                .child(row)
                .into_any_element()
        }
    }
}
