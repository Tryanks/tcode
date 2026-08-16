use super::super::*;

impl Composer {
    /// Whether a send right now continues planning rather than starting work.
    /// The plan stays implementable from either mode, but only Plan mode turns
    /// typed feedback into refinement.
    pub(in super::super) fn refines_the_plan(&self, cx: &App) -> bool {
        self.workspace_store
            .read(cx)
            .composer_state()
            .interaction_mode
            == InteractionMode::Plan
    }

    /// The Implement split-button: primary "Implement" + a chevron menu with
    /// "Implement in a new thread" (S1 §5).
    pub(in super::super) fn render_implement_split(&self, cx: &mut Context<Self>) -> AnyElement {
        let primary = cx.theme().primary;
        let fg = cx.theme().primary_foreground;
        let store_main = self.workspace_store.clone();

        let chevron = crate::material::overlay_popover("implement-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("implement-menu-trigger")
                    .primary()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let app = cx.entity();
                let store = store_main.clone();
                let popover = cx.entity();
                v_flex()
                    .w(px(220.))
                    .p_1()
                    .child(
                        h_flex()
                            .id("implement-new-thread")
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(Icon::new(IconName::Plus).xsmall())
                            .child(crate::tr!("plan.implement_new_thread"))
                            .on_click(move |_, window, cx| {
                                store.update(cx, |store, _cx| {
                                    let Some(markdown) = store.composer_state().plan_ready_markdown
                                    else {
                                        return;
                                    };
                                    let title = match tcode_core::session::plan_title(&markdown) {
                                        Some(title) => {
                                            crate::tr!("plan.implement_titled", title = title)
                                                .into_owned()
                                        }
                                        None => crate::tr!("plan.implement_untitled").into_owned(),
                                    };
                                    store.implement_plan_in_new_thread(title);
                                });
                                let _ = &app;
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    )
                    .into_any_element()
            });

        let store_impl = self.workspace_store.clone();
        h_flex()
            .flex_none()
            .h(px(32.))
            .items_center()
            .rounded(crate::material::radius_button())
            .bg(primary)
            .text_color(fg)
            .overflow_hidden()
            .child(
                h_flex()
                    .id("implement-main")
                    .h_full()
                    .px_3()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .font_medium()
                    .hover(|s| s.opacity(0.9))
                    .child(crate::tr!("plan.implement"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        store_impl.update(cx, |store, _cx| store.implement_plan());
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(fg).opacity(0.3))
            .child(chevron)
            .into_any_element()
    }

    /// The "Plan Ready" header strip shown atop the composer while a proposed
    /// plan awaits a decision (S1 §5).
    pub(in super::super) fn render_plan_ready_header(
        &self,
        title: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.workspace_store.clone();
        h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_none()
                    .text_color(cx.theme().primary)
                    .text_size(px(11.))
                    .line_height(px(18.))
                    .font_medium()
                    .child(crate::tr!("plan.ready")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .child(
                Button::new("dismiss-plan")
                    .ghost()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(crate::tr!("plan.dismiss"))
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, _cx| store.dismiss_plan());
                    }),
            )
            .into_any_element()
    }
}
