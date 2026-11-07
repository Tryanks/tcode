//! The questions computer use asks the user in a thread: may an app come to
//! the front for keys that could not reach it in the background. The host
//! holds each until it is answered from any client, so an answer given
//! after the call stopped waiting still decides the thread's next attempt.

use super::*;
use computer_use_mcp::foreground::{AnswerSender, ForegroundAsk, ForegroundAsks};
use tcode_core::computer_use::{ForegroundAnswer, ForegroundPrompt};
use tcode_core::settings::ForegroundApp;

/// A question the host holds for a thread.
pub(super) struct HeldPrompt {
    prompt: ForegroundPrompt,
    /// The call waiting for the answer, while it waits.
    reply: Option<AnswerSender>,
    /// An answer given after the call stopped waiting, kept for the
    /// thread's next attempt with the same app.
    late_answer: Option<ForegroundAnswer>,
}

impl AppState {
    /// Hand every question computer use asks to the host loop.
    pub fn pump_foreground_asks(&mut self, asks: ForegroundAsks, cx: &mut HostCx) {
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let mut asks = asks;
            while let Some(ask) = asks.recv().await {
                host_cx.enqueue(move |state, cx| state.ask_foreground(ask, cx));
            }
        });
    }

    /// Show a question in its thread, or answer it at once with what the
    /// user said after an earlier call for the same app stopped waiting. A
    /// question already open for the app takes the new call's wait.
    pub(super) fn ask_foreground(&mut self, ask: ForegroundAsk, _cx: &mut HostCx) {
        let ForegroundAsk {
            session_id,
            app_name,
            bundle_id,
            window_title,
            action,
            reply,
        } = ask;
        let prompts = self
            .foreground_prompts
            .entry(session_id.clone())
            .or_default();
        if let Some(index) = prompts
            .iter()
            .position(|held| held.prompt.bundle_id == bundle_id)
        {
            if let Some(answer) = prompts[index].late_answer {
                prompts.remove(index);
                if prompts.is_empty() {
                    self.foreground_prompts.remove(&session_id);
                }
                let _ = reply.send(answer);
                return;
            }
            let held = &mut prompts[index];
            held.prompt.window_title = window_title;
            held.prompt.action = action;
            held.reply = Some(reply);
            return;
        }
        prompts.push(HeldPrompt {
            prompt: ForegroundPrompt {
                id: uuid::Uuid::new_v4().to_string(),
                app_name,
                bundle_id,
                window_title,
                action,
                asked_at: now_secs(),
            },
            reply: Some(reply),
            late_answer: None,
        });
    }

    /// The question the thread shows, oldest first; one already answered
    /// late waits for the next attempt and is not asked again.
    pub(super) fn open_foreground_prompt(&self, session_id: &str) -> Option<ForegroundPrompt> {
        self.foreground_prompts
            .get(session_id)?
            .iter()
            .find(|held| held.late_answer.is_none())
            .map(|held| held.prompt.clone())
    }

    /// The user's answer: to the call still waiting, else kept for the
    /// thread's next attempt. "Always" also remembers the app as allowed.
    pub fn respond_foreground_prompt(
        &mut self,
        session_id: &str,
        prompt_id: &str,
        answer: ForegroundAnswer,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let answered = || ProtocolError {
            code: "unknown_request".into(),
            message: "This question was already answered.".into(),
        };
        let prompts = self
            .foreground_prompts
            .get_mut(session_id)
            .ok_or_else(answered)?;
        let index = prompts
            .iter()
            .position(|held| held.prompt.id == prompt_id && held.late_answer.is_none())
            .ok_or_else(answered)?;
        let held = &mut prompts[index];
        let app = (held.prompt.bundle_id.clone(), held.prompt.app_name.clone());
        let delivered = held
            .reply
            .take()
            .is_some_and(|reply| reply.send(answer).is_ok());
        if delivered || answer == ForegroundAnswer::Always {
            prompts.remove(index);
            if prompts.is_empty() {
                self.foreground_prompts.remove(session_id);
            }
        } else {
            held.late_answer = Some(answer);
        }
        if answer == ForegroundAnswer::Always {
            let (bundle_id, name) = app;
            self.patch_settings(
                tcode_protocol::SettingsPatch::ComputerUseForegroundApp {
                    bundle_id,
                    app: Some(ForegroundApp { name, allow: true }),
                },
                cx,
            );
        }
        Ok(())
    }

    /// Withdraw the thread's questions: its turn was interrupted or its
    /// computer use went away.
    pub(super) fn withdraw_foreground_prompts(&mut self, session_id: &str) {
        if self.foreground_prompts.contains_key(session_id) {
            self.foreground_prompts.remove(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::active_session::Runtime;
    use crate::app::test_support::*;
    use tcode_core::computer_use::ForegroundAction;
    use tcode_protocol::{Command, CommandResponse, HostMessage};

    fn ask(
        app: &str,
        bundle_id: &str,
    ) -> (
        ForegroundAsk,
        tokio::sync::oneshot::Receiver<ForegroundAnswer>,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        (
            ForegroundAsk {
                session_id: "thread".into(),
                app_name: app.into(),
                bundle_id: bundle_id.into(),
                window_title: "Groceries".into(),
                action: ForegroundAction::Type { characters: 12 },
                reply,
            },
            answer,
        )
    }

    fn host() -> (TestAppContext, TestStore, TestEntity) {
        let mut cx = TestAppContext::default();
        let store = TestStore::new("tcode-foreground-prompt");
        let state = cx.new_entity(TestClientState::new((*store).clone()));
        state.update(&mut cx, |state, _| {
            let mut meta = SessionMeta::new(
                ProviderKind::ClaudeCode,
                PathBuf::from("/tmp/synthetic-checkout"),
                None,
            );
            meta.id = "thread".into();
            state.sessions.push(meta.clone());
            let mut session = ActiveSession::new(meta, false, Vec::new());
            session.runtime = Runtime::Starting { generation: 1 };
            state.residents.parked.insert("thread".into(), session);
        });
        (cx, store, state)
    }

    /// What the thread shows: the open question, and the thread waiting on
    /// the user as it does for an approval.
    fn shown(state: &TestEntity) -> (Option<ForegroundPrompt>, bool) {
        state.read(|state| {
            (
                state
                    .session_status_snapshot("thread")
                    .unwrap()
                    .foreground_prompt,
                state.index_snapshot().summary.activity["thread"].waiting_for_approval,
            )
        })
    }

    fn respond(cx: &mut TestAppContext, state: &TestEntity, id: u64, answer: ForegroundAnswer) {
        let prompt_id = shown(state).0.expect("a question is open").id;
        state.dispatch_command(
            cx,
            id,
            Command::RespondForegroundPrompt {
                session_id: "thread".into(),
                prompt_id,
                answer,
            },
        );
        cx.run_until_parked();
        assert!(cx.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Ack {
                id: acked,
                result: Ok(CommandResponse::Unit)
            } if *acked == id
        )));
    }

    /// A question shows in its thread until it is answered from a client,
    /// and the answer reaches the call that waits for it.
    #[test]
    fn a_question_waits_in_its_thread_and_the_answer_reaches_the_call() {
        let (mut cx, _store, state) = host();
        let (question, mut answer) = ask("Notes", "com.apple.Notes");
        state.update(&mut cx, |state, cx| state.ask_foreground(question, cx));
        let (prompt, waiting) = shown(&state);
        let prompt = prompt.unwrap();
        assert_eq!(
            (
                prompt.app_name.as_str(),
                prompt.bundle_id.as_str(),
                prompt.window_title.as_str()
            ),
            ("Notes", "com.apple.Notes", "Groceries")
        );
        assert!(waiting, "the thread waits on the user");

        respond(&mut cx, &state, 1, ForegroundAnswer::Once);
        assert_eq!(answer.try_recv(), Ok(ForegroundAnswer::Once));
        assert_eq!(shown(&state), (None, false));
        assert!(
            state.read(|state| state.settings.computer_use.foreground_apps.is_empty()),
            "allowing once remembers nothing"
        );
    }

    /// "Always" remembers the app in Settings → Computer Use. An answer given
    /// after the call stopped waiting leaves the thread and decides its next
    /// attempt with that app without asking again.
    #[test]
    fn always_remembers_the_app_and_a_late_answer_decides_the_next_attempt() {
        let (mut cx, _store, state) = host();
        let (question, answer) = ask("Notes", "com.apple.Notes");
        state.update(&mut cx, |state, cx| state.ask_foreground(question, cx));
        drop(answer);
        respond(&mut cx, &state, 1, ForegroundAnswer::Always);
        assert_eq!(
            state.read(|state| state.settings.computer_use.foreground_apps.clone()),
            [(
                "com.apple.Notes".to_owned(),
                ForegroundApp {
                    name: "Notes".into(),
                    allow: true
                }
            )]
            .into()
        );
        assert_eq!(shown(&state), (None, false));

        let (question, answer) = ask("Terminal", "com.apple.Terminal");
        state.update(&mut cx, |state, cx| state.ask_foreground(question, cx));
        drop(answer);
        respond(&mut cx, &state, 2, ForegroundAnswer::Deny);
        assert_eq!(
            shown(&state),
            (None, false),
            "an answered question leaves the thread"
        );

        let (question, mut answer) = ask("Terminal", "com.apple.Terminal");
        state.update(&mut cx, |state, cx| state.ask_foreground(question, cx));
        assert_eq!(answer.try_recv(), Ok(ForegroundAnswer::Deny));
        assert_eq!(shown(&state), (None, false), "it is not asked again");
        assert!(
            !state.read(|state| state
                .settings
                .computer_use
                .foreground_apps
                .contains_key("com.apple.Terminal")),
            "declining once remembers nothing"
        );
    }
}
