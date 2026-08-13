use super::super::*;

/// Queue bubbles collapse whitespace and clip long messages (the full text is
/// still what gets sent).
fn truncate_queued(text: &str) -> String {
    const MAX: usize = 80;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX {
        return normalized;
    }
    let clipped: String = normalized.chars().take(MAX).collect();
    format!("{clipped}…")
}

impl Composer {
    /// The 64px thumbnail strip above the control row (T3), when images are
    /// attached; each thumbnail opens an expanded preview and has a remove `x`.
    /// The queue strip shown ABOVE the card whenever messages are waiting for
    /// the running turn to finish: one row per queued message (truncated text),
    /// each with a send-now/steer button and an ✕ (drop it). Scheduled rows add
    /// a live countdown; rows are reorderable-by-removal only.
    pub(in super::super) fn render_queue_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some((queued, can_steer, agent)) = self.workspace_store.read(cx).composer_queue()
        else {
            self.scheduled_countdown_tick = None;
            return None;
        };
        let has_scheduled = queued
            .iter()
            .any(|message| message.fire_at_unix_secs.is_some());
        if has_scheduled && self.scheduled_countdown_tick.is_none() {
            self.scheduled_countdown_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !has_scheduled {
            // Dropping the task cancels its timer, so at most one repaint loop
            // exists and it stops as soon as the last scheduled row disappears.
            self.scheduled_countdown_tick = None;
        }
        if queued.is_empty() {
            return None;
        }

        let muted = cx.theme().muted_foreground;
        let mut strip = v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary.opacity(0.5))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("composer.queued_count", count = queued.len())),
            );

        for message in queued {
            let id = message.id;
            let scheduled = message.fire_at_unix_secs.is_some();
            let steer_tooltip = if scheduled {
                crate::tr!("composer.send_now").into_owned()
            } else if can_steer {
                crate::tr!("composer.steer_queued").into_owned()
            } else {
                crate::tr!("composer.steer_unsupported_tooltip", agent = agent).into_owned()
            };
            let countdown = message.fire_at_unix_secs.map(|fire_at| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                format_countdown(fire_at.saturating_sub(now))
            });
            strip = strip.child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(cx.theme().foreground)
                            .child(truncate_queued(&message.text)),
                    )
                    .when_some(countdown, |row, countdown| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(12.))
                                .text_color(muted)
                                .child(countdown),
                        )
                    })
                    .child(
                        Button::new(("queue-steer", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowUp)
                            // Scheduled rows always support send-now: the
                            // runtime removes the deadline and uses the normal
                            // send/queue path even when native steering is absent.
                            .disabled(!scheduled && !can_steer)
                            .tooltip(steer_tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.dispatch(Command::SteerQueued { id })
                                });
                            })),
                    )
                    .child(
                        Button::new(("queue-drop", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(crate::tr!("composer.drop_queued"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.dispatch(Command::DropQueued { id })
                                });
                            })),
                    ),
            );
        }
        Some(
            div()
                .id("queued-messages-scroll")
                .w_full()
                .max_h(px(180.))
                .overflow_y_scroll()
                .child(strip)
                .into_any_element(),
        )
    }
}
