use super::super::*;

use crate::store::FallbackBlock;
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
}
