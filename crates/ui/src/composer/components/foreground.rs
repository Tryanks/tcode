use super::super::*;
use tcode_core::computer_use::{ForegroundAction, ForegroundAnswer, ForegroundPrompt};

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// Computer use's question whether an app may come to the front for
/// typing, kept while its card folds away so it leaves as it was.
#[derive(Clone, PartialEq)]
pub(in super::super) struct ForegroundQuestion {
    session_id: String,
    prompt: ForegroundPrompt,
    harness: String,
    /// The machine it happens on, named when this client is another one.
    host: Option<String>,
}

impl Composer {
    pub(in super::super) fn foreground_question(&self, cx: &App) -> Option<ForegroundQuestion> {
        let session_id = self.session_id.clone()?;
        let store = self.workspace_store.read(cx);
        let prompt = store
            .session_status_for(Some(&session_id), cx)?
            .foreground_prompt
            .clone()?;
        Some(ForegroundQuestion {
            session_id,
            prompt,
            harness: super::trigger_menu::harness_name(&self.composer_state(cx), store),
            host: store.remote_host_name().map(str::to_owned),
        })
    }

    /// The card for the question; a new question cross-fades over the one
    /// it follows.
    pub(in super::super) fn render_foreground_panel(
        &self,
        question: &ForegroundQuestion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.motion.cross_fade(
            self.motion_key("foreground-question"),
            question.clone(),
            CHANGE,
            window,
            cx,
            |question, _, cx| self.render_foreground_question(question, cx),
        )
    }

    fn render_foreground_question(
        &self,
        question: &ForegroundQuestion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prompt = &question.prompt;
        let app = prompt.app_name.as_str();
        let respond = |answer: ForegroundAnswer| {
            let (session_id, prompt_id) = (question.session_id.clone(), prompt.id.clone());
            cx.listener(move |this: &mut Self, _, _, cx| {
                let (session_id, prompt_id) = (session_id.clone(), prompt_id.clone());
                this.workspace_store.update(cx, |store, _| {
                    store.respond_foreground_prompt(session_id, prompt_id, answer)
                });
            })
        };
        let mut body = if prompt.window_title.is_empty() {
            crate::tr!("computer_use.prompt.window_untitled").into_owned()
        } else {
            crate::tr!(
                "computer_use.prompt.window",
                window = prompt.window_title.as_str()
            )
            .into_owned()
        };
        body.push_str(&crate::tr!("list.sentence_separator"));
        body.push_str(&match &prompt.action {
            ForegroundAction::Type { characters } => {
                crate::tr!("computer_use.prompt.type", count = characters)
            }
            ForegroundAction::Replace { characters } => {
                crate::tr!("computer_use.prompt.replace", count = characters)
            }
            ForegroundAction::Keys { keys } => {
                crate::tr!("computer_use.prompt.keys", keys = keys.join(" "))
            }
        });
        body.push_str(&crate::tr!("list.sentence_separator"));
        body.push_str(&match &question.host {
            Some(host) => crate::tr!("computer_use.prompt.remote", host = host.as_str()),
            None => crate::tr!("computer_use.prompt.while"),
        });
        let title_line = cx.theme().tokens.typography.md.line_height;
        let buttons = [
            (
                "foreground-allow-once",
                crate::tr!("computer_use.prompt.once").into_owned(),
                ForegroundAnswer::Once,
            ),
            (
                "foreground-allow-always",
                crate::tr!("computer_use.prompt.always", app = app).into_owned(),
                ForegroundAnswer::Always,
            ),
            (
                "foreground-deny",
                crate::tr!("computer_use.prompt.deny").into_owned(),
                ForegroundAnswer::Deny,
            ),
        ]
        .into_iter()
        .map(|(id, label, answer)| {
            let button = Button::new(id)
                .debug_selector(move || id.into())
                .motion(&self.button_motion())
                .chip()
                .touch(self.compact)
                .disabled(!self.interactive(cx))
                .label(label)
                .on_click(respond(answer));
            match answer {
                ForegroundAnswer::Once => button.primary(),
                ForegroundAnswer::Always => button.outline(),
                ForegroundAnswer::Deny => button.danger().outline(),
            }
        });
        let buttons = if self.compact {
            v_flex()
                .w_full()
                .gap_2()
                .children(buttons.map(|button| button.w_full()).collect::<Vec<_>>())
        } else {
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(buttons.collect::<Vec<_>>())
        };
        crate::material::card_raised(cx)
            .w_full()
            .debug_selector(|| "foreground-question".into())
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .text_caption(cx)
                            .line_height(title_line)
                            .child(crate::tr!("computer_use.prompt.caption")),
                    )
                    .child(div().flex_1().min_w_0().text_title(cx).child(crate::tr!(
                        "computer_use.prompt.title",
                        harness = question.harness.as_str(),
                        app = app
                    ))),
            )
            .child(div().text_body(self.compact, cx).child(body))
            .child(buttons)
            .child(
                div()
                    .text_secondary(self.compact, cx)
                    .child(crate::tr!("computer_use.prompt.timeout")),
            )
            .into_any_element()
    }
}
