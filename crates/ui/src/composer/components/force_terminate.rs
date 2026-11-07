//! The way out when a harness stops answering: a stop that has gone
//! unanswered for a while offers to kill its process, after saying what that
//! loses.

use super::super::*;
use crate::widgets::button::ButtonVariant;

/// How long a stop goes unanswered before the force terminate is offered. A
/// harness that answers at all answers within seconds.
const UNANSWERED: std::time::Duration = std::time::Duration::from_secs(15);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

impl Composer {
    pub(in super::super) fn render_force_terminate(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.composer_state(cx);
        let since = state.stopping_since.filter(|_| state.stopping);
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let waited = since.map(|since| now.saturating_sub(since));
        let due = waited.filter(|waited| *waited >= UNANSWERED.as_millis() as u64);
        if let (Some(since), Some(waited), None) = (since, waited, due) {
            if self.stop_wait.as_ref().is_none_or(|(at, _)| *at != since) {
                let left = UNANSWERED.saturating_sub(std::time::Duration::from_millis(waited));
                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(left).await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                });
                self.stop_wait = Some((since, task));
            }
        } else {
            self.stop_wait = None;
        }
        let harness = due.map(|_| {
            self.workspace_store
                .read(cx)
                .harness_name_for(self.session_id.as_deref(), cx)
        });
        let shown = self.motion.shown(
            self.motion_key("force-terminate"),
            harness,
            SHOW,
            window,
            cx,
        )?;
        let harness = shown.value.clone();
        let row = h_flex()
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_secondary(self.compact, cx)
                    .child(
                        crate::tr!("composer.stop_unanswered", harness = harness.as_str())
                            .into_owned(),
                    ),
            )
            .child(
                Button::new("force-terminate")
                    .motion(&self.button_motion())
                    .debug_selector(|| "force-terminate".into())
                    .danger()
                    .xsmall()
                    .touch(self.compact)
                    .label(crate::tr!("composer.force_terminate").into_owned())
                    .disabled(!self.interactive(cx))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_force_terminate(harness.clone(), window, cx)
                    })),
            );
        Some(super::super::column_fold(&shown, row))
    }

    fn confirm_force_terminate(
        &mut self,
        harness: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.clone();
        let session_id = self.session_id.clone().unwrap_or_default();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let session_id = session_id.clone();
            alert
                .title(crate::tr!(
                    "composer.force_terminate_title",
                    harness = harness.as_str()
                ))
                .description(crate::tr!(
                    "composer.force_terminate_description",
                    harness = harness.as_str()
                ))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("composer.force_terminate"))
                        .cancel_text(crate::tr!("composer.force_terminate_cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _cx| store.force_terminate(session_id.clone()));
                    true
                })
        });
    }
}
