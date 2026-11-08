//! The background work a thread's harness lists above the field: each piece
//! with how long it has run and a stop, and a stop for all of it.

use super::super::*;
use crate::scroll::ScrollableElement as _;
use gpui::SharedString;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

impl Composer {
    pub(in super::super) fn render_background_strip(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let tasks = self.composer_state(cx).background_tasks.clone();
        let shown = self.motion.shown(
            self.motion_key("background-strip"),
            (!tasks.is_empty()).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let header = h_flex()
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_secondary(self.compact, cx)
                    .child(crate::tr!("composer.background_tasks").into_owned()),
            )
            .child(
                Button::new("background-stop-all")
                    .motion(&self.button_motion())
                    .debug_selector(|| "background-stop-all".into())
                    .ghost()
                    .xsmall()
                    .touch(self.compact)
                    .label(crate::tr!("composer.background_stop_all").into_owned())
                    .disabled(!self.interactive(cx))
                    .on_click(cx.listener(|this, _, _, cx| this.stop_background_task(None, cx))),
            );
        let this = &*self;
        let rows = tasks.into_iter().map(|task| {
            (
                SharedString::from(format!("background-{}", task.task.id)),
                task.task,
            )
        });
        let rows = crate::motion::keyed::keyed_owned(
            &this.motion,
            this.motion_key("background-rows"),
            crate::motion::keyed::Fold::Down,
            rows.collect::<Vec<_>>(),
            ROWS,
            window,
            cx,
            |_, task, _, cx| this.render_background_row(task, cx),
        )
        .render(v_flex().w_full());
        Some(super::super::column_fold(
            &shown,
            v_flex().w_full().child(header).child(
                div()
                    .id("background-tasks-scroll")
                    .w_full()
                    // A few rows before the strip scrolls, as the queue strip's.
                    .max_h(px(120.))
                    .overflow_y_scroll_area()
                    .child(rows),
            ),
        ))
    }

    fn render_background_row(
        &self,
        task: &agent::BackgroundTask,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = task.id.clone();
        let selector = format!("background-stop-{id}");
        h_flex()
            .id(SharedString::from(format!("background-row-{id}")))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .text_body(self.compact, cx)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(SharedString::from(
                        crate::chat::model::one_line(&task.name),
                    ))),
            )
            .child(div().flex_none().text_secondary(self.compact, cx).child(
                crate::widgets::clock::Clock::since(
                    SharedString::from(format!("background-elapsed-{id}")),
                    task.started_at,
                    |elapsed| crate::chat::format_elapsed_seconds(elapsed).into(),
                ),
            ))
            .child(
                Button::new(SharedString::from(selector.clone()))
                    .motion(&self.button_motion())
                    .debug_selector(move || selector.clone())
                    .ghost()
                    .xsmall()
                    .touch(self.compact)
                    .icon(IconName::Close)
                    .disabled(!self.interactive(cx))
                    .tooltip(crate::tr!("composer.background_stop"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stop_background_task(Some(id.clone()), cx)
                    })),
            )
            .into_any_element()
    }

    fn stop_background_task(&mut self, task_id: Option<String>, cx: &mut Context<Self>) {
        let session_id = self.session_id.clone().unwrap_or_default();
        self.workspace_store.update(cx, |store, _cx| {
            store.stop_background_task(session_id, task_id)
        });
    }
}
