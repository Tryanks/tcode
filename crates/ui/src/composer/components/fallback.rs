use super::super::*;

use crate::store::{FallbackBlock, FallbackReview};
use agent::{ClassifierCategory, RewindMode};

/// Opus is the classifier's intended fallback target for legitimate work; the
/// flagged category picks which release takes the retry.
fn retry_model(category: Option<&ClassifierCategory>) -> &'static str {
    match category {
        Some(ClassifierCategory::Bio) => "claude-opus-5",
        _ => "claude-opus-4-8",
    }
}

impl Composer {
    /// The recovery card for a turn Claude Code's safety classifier stopped.
    /// Every action here is a user click — nothing retries or reroutes by itself.
    pub(in super::super) fn render_fallback_panel(
        &self,
        block: &FallbackBlock,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let reason = match block.category.as_ref() {
            Some(ClassifierCategory::Cyber) => crate::tr!("fallback.reason_cyber"),
            Some(ClassifierCategory::Bio) => crate::tr!("fallback.reason_bio"),
            _ => crate::tr!("fallback.reason_generic"),
        };
        let outcome = match block.fallback_model.as_ref() {
            Some(model) => crate::tr!("fallback.rerouted", model = model.clone()),
            None => crate::tr!(
                "fallback.blocked",
                model = block.model.clone().unwrap_or_default()
            ),
        };

        let model = retry_model(block.category.as_ref());
        // The refused prompt is the last user message of this session; the
        // rewind path additionally needs its turn to have a provider checkpoint.
        let refused = self.workspace_store.read(cx).last_user_message();
        let can_rewind = refused.as_ref().is_some_and(|(turn, _)| {
            self.workspace_store
                .read(cx)
                .chat_native_rewind_state(*turn)
                .is_some_and(|(available, disabled)| available && !disabled)
        });

        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(Icon::new(IconName::Info).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_medium()
                    .child(crate::tr!("fallback.title")),
            )
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "fallback-dismiss",
                    Role::Button,
                    crate::tr!("fallback.dismiss"),
                    cx,
                )
                .size(px(22.))
                .rounded(crate::material::radius_input())
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().muted))
                .child(Icon::new(IconName::Close).xsmall().text_color(muted))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.workspace_store
                        .update(cx, |store, _cx| store.dismiss_fallback_block());
                    cx.notify();
                })),
            );

        let retry_text = refused.as_ref().map(|(_, text)| text.clone());
        let rewind_turn = refused.as_ref().map(|(turn, _)| *turn);
        let actions = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(div().flex_1())
            .when(can_rewind, |this| {
                let turn = rewind_turn.unwrap_or_default();
                this.child(
                    Button::new("fallback-edit-retry")
                        .outline()
                        .small()
                        .h(px(28.))
                        .rounded(crate::material::radius_input())
                        .label(crate::tr!("fallback.edit_retry"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.rewind_turn(turn, RewindMode::Conversation);
                                store.dismiss_fallback_block();
                            });
                            cx.notify();
                        })),
                )
            })
            .when_some(retry_text, |this, text| {
                this.child(
                    Button::new("fallback-retry-opus")
                        .primary()
                        .small()
                        .h(px(28.))
                        .rounded(crate::material::radius_input())
                        .label(crate::tr!("fallback.retry_on", model = model))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.set_active_model(
                                    ProviderKind::ClaudeCode,
                                    Some(model.to_string()),
                                    None,
                                );
                                store.dismiss_fallback_block();
                            });
                            // The user reviews and sends it themselves.
                            this.set_input_text(text.clone(), window, cx);
                        })),
                )
            });

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .child(header)
            .child(div().text_size(px(13.)).child(reason))
            .child(div().text_size(px(11.)).text_color(muted).child(outcome))
            .when(!block.detail.trim().is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(block.detail.clone()),
                )
            })
            .child(actions)
            .into_any_element()
    }

    /// Seed the draft field once per suggestion: re-seeding only when the
    /// reviewer's text itself changes keeps the user's edits across re-renders.
    pub(in super::super) fn sync_fallback_review_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self
            .workspace_store
            .read(cx)
            .active_fallback_review()
            .map(|review| review.draft.clone());
        if self.fallback_review_seeded == draft {
            return;
        }
        self.fallback_review_seeded = draft.clone();
        let draft = draft.unwrap_or_default();
        self.fallback_review_input
            .update(cx, |state, cx| state.set_value(draft, window, cx));
    }

    /// The reviewer's second opinion on a classifier stop. It advises only: the
    /// clarification below is a draft the user edits and sends themselves.
    pub(in super::super) fn render_fallback_review_panel(
        &self,
        review: &FallbackReview,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let has_draft = !review.draft.trim().is_empty();
        let input = self.fallback_review_input.clone();
        let can_send = has_draft && !input.read(cx).value().trim().is_empty();

        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(Icon::new(IconName::Info).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_medium()
                    .child(crate::tr!("fallback.review_title")),
            )
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "fallback-review-dismiss",
                    Role::Button,
                    crate::tr!("fallback.dismiss"),
                    cx,
                )
                .size(px(22.))
                .rounded(crate::material::radius_input())
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|this| this.bg(cx.theme().muted))
                .child(Icon::new(IconName::Close).xsmall().text_color(muted))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.workspace_store
                        .update(cx, |store, _cx| store.dismiss_fallback_review());
                    cx.notify();
                })),
            );

        // Model-authored text, rendered as plain text.
        let assessment = div()
            .text_size(px(13.))
            .text_color(muted)
            .child(review.assessment.clone());

        let draft_row = h_flex()
            .w_full()
            .items_end()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().popover)
            .child(
                div().flex_1().min_w_0().child(
                    Textarea::new(&self.fallback_review_input)
                        .appearance(false)
                        .text_size(px(13.)),
                ),
            )
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "fallback-review-send",
                    Role::Button,
                    crate::tr!("fallback.review_send"),
                    cx,
                )
                .size(px(28.))
                .rounded(crate::material::radius_input())
                .flex()
                .items_center()
                .justify_center()
                .bg(if can_send {
                    cx.theme().primary
                } else {
                    cx.theme().muted
                })
                .cursor_pointer()
                .when(can_send, |this| this.hover(|this| this.opacity(0.9)))
                .child(
                    Icon::new(IconName::ArrowUp)
                        .xsmall()
                        .text_color(if can_send {
                            cx.theme().primary_foreground
                        } else {
                            cx.theme().muted_foreground
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let text = input.read(cx).value().trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    this.workspace_store.update(cx, |store, _cx| {
                        store.send_turn(text, Vec::new());
                        store.dismiss_fallback_review();
                    });
                    input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
                    this.fallback_review_seeded = None;
                    cx.notify();
                })),
            );

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .child(header)
            .child(assessment)
            // No draft means the reviewer did not call it a false positive:
            // there is nothing to send, so only the assessment stands.
            .when(has_draft, |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(crate::tr!("fallback.review_hint")),
                )
                .child(draft_row)
            })
            .into_any_element()
    }
}
