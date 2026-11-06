//! Whether a keyboard step whose keys could not be delivered in the
//! background is typed with its app in front: by the choice the user
//! remembered for the app or for other apps, or by asking them in the
//! thread, and the words the model reads about what happened.

use std::time::Duration;

use serde::Serialize;
use tcode_core::computer_use::{ForegroundAction, ForegroundAnswer};
use tcode_core::settings::{ComputerUseSettings, ForegroundChoice};

use crate::backend::{ActionKind, ActionRequest, ActionResult, BackendError, Delivery, RootInfo};

/// How long a call waits for the user's answer. Codex gives an MCP tool 60
/// seconds by default; the step has to come back before that.
pub const ANSWER_WAIT: Duration = Duration::from_secs(45);

/// A question for the user, carried to the host that shows it in the
/// thread; `reply` takes the answer while the call still waits.
pub struct ForegroundAsk {
    pub session_id: String,
    pub app_name: String,
    pub bundle_id: String,
    pub window_title: String,
    pub action: ForegroundAction,
    pub reply: AnswerSender,
}

/// Takes the user's answer to a [`ForegroundAsk`].
pub type AnswerSender = tokio::sync::oneshot::Sender<ForegroundAnswer>;

/// The questions every session's calls ask, for the host to show.
pub type ForegroundAsks = tokio::sync::mpsc::UnboundedReceiver<ForegroundAsk>;

/// One session's way to the host.
#[derive(Clone)]
pub(crate) struct ForegroundAsker {
    session_id: String,
    asks: tokio::sync::mpsc::UnboundedSender<ForegroundAsk>,
}

impl ForegroundAsker {
    pub(crate) fn new(
        session_id: String,
        asks: tokio::sync::mpsc::UnboundedSender<ForegroundAsk>,
    ) -> Self {
        Self { session_id, asks }
    }
}

/// What was decided for one step, and on whose word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    /// The user allowed it when asked, for this step.
    AllowedOnce,
    /// The user allowed it when asked, and for the app from now on.
    AllowedAlways,
    /// A choice in Settings → Computer Use allows it.
    AllowedBySetting,
    Declined,
    DeclinedBySetting,
    /// Asked, and no answer came in [`ANSWER_WAIT`].
    Unanswered,
    /// There was no host to ask.
    NotAsked,
}

impl Decision {
    pub(crate) fn allows(self) -> bool {
        matches!(
            self,
            Self::AllowedOnce | Self::AllowedAlways | Self::AllowedBySetting
        )
    }
}

/// What the keys of `request` would do once the app is in front.
pub(crate) fn action_of(request: &ActionRequest) -> ForegroundAction {
    let characters = request
        .text
        .as_deref()
        .map_or(0, |text| text.chars().count());
    match request.kind {
        ActionKind::Keypress => ForegroundAction::Keys {
            keys: request.keys.clone().unwrap_or_default(),
        },
        ActionKind::SetText => ForegroundAction::Replace { characters },
        _ => ForegroundAction::Type { characters },
    }
}

/// Decide by the user's remembered choice for the app or for other apps,
/// and otherwise ask them and wait up to `wait` for the answer.
pub(crate) async fn decide(
    settings: &ComputerUseSettings,
    asker: Option<&ForegroundAsker>,
    root: &RootInfo,
    action: ForegroundAction,
    wait: Duration,
) -> (Decision, bool) {
    let (choice, remembered) = settings.foreground_choice(&root.bundle_id);
    let decision = match choice {
        ForegroundChoice::Allow => Decision::AllowedBySetting,
        ForegroundChoice::Deny => Decision::DeclinedBySetting,
        ForegroundChoice::Ask => {
            let Some(asker) = asker else {
                return (Decision::NotAsked, remembered);
            };
            let (reply, answer) = tokio::sync::oneshot::channel();
            let sent = asker.asks.send(ForegroundAsk {
                session_id: asker.session_id.clone(),
                app_name: root.app_name.clone(),
                bundle_id: root.bundle_id.clone(),
                window_title: root.title.clone(),
                action,
                reply,
            });
            if sent.is_err() {
                return (Decision::NotAsked, remembered);
            }
            match tokio::time::timeout(wait, answer).await {
                Ok(Ok(ForegroundAnswer::Once)) => Decision::AllowedOnce,
                Ok(Ok(ForegroundAnswer::Always)) => Decision::AllowedAlways,
                Ok(Ok(ForegroundAnswer::Deny)) => Decision::Declined,
                Ok(Err(_)) => Decision::NotAsked,
                Err(_) => Decision::Unanswered,
            }
        }
    };
    (decision, remembered)
}

/// The step as the model reads it: what was decided, on whose word, and,
/// when the app came to the front, whether the keys went through.
pub(crate) fn step_result(
    decision: Decision,
    remembered: bool,
    app: &str,
    background_error: &str,
    typed: Option<Result<(), BackendError>>,
) -> ActionResult {
    let start = format!("background key delivery could not start ({background_error})");
    let subject = if remembered {
        app.to_owned()
    } else {
        "apps without a remembered choice".to_owned()
    };
    let why = match decision {
        Decision::AllowedOnce => {
            format!("the user allowed bringing {app} to the front for this step")
        }
        Decision::AllowedAlways => format!(
            "the user allowed bringing {app} to the front for typing, for this step and from now on"
        ),
        Decision::AllowedBySetting => format!(
            "{subject} {} set to come to the front for typing in Settings → Computer Use",
            if remembered { "is" } else { "are" }
        ),
        Decision::Declined => {
            return ActionResult::didnt(
                format!("{start}; the user declined bringing {app} to the front for typing."),
                Delivery::None,
            );
        }
        Decision::DeclinedBySetting => {
            return ActionResult::didnt(
                format!(
                    "{start}; {subject} {} set not to come to the front for typing in Settings → Computer Use.",
                    if remembered { "is" } else { "are" }
                ),
                Delivery::None,
            );
        }
        Decision::Unanswered => {
            return ActionResult::didnt(
                format!(
                    "{start}; the user was asked whether {app} may come to the front and has not answered in {} seconds. The question stays open in Tcode; calling act_ui again applies their answer once given.",
                    ANSWER_WAIT.as_secs()
                ),
                Delivery::None,
            );
        }
        Decision::NotAsked => {
            return ActionResult::didnt(
                format!(
                    "{start}; Tcode could not ask the user, so {app} was not brought to the front."
                ),
                Delivery::None,
            );
        }
    };
    match typed {
        Some(Ok(())) => ActionResult::unknown(
            format!(
                "{start}; {why}: it was activated, the keys were typed, and the app the user was in was restored. No pointer events were sent."
            ),
            Delivery::ForegroundHid,
        ),
        Some(Err(error)) => ActionResult::didnt(
            format!("{start}; {why}, but typing with it in front failed: {error}"),
            Delivery::None,
        ),
        None => ActionResult::didnt(
            format!("{start}; {why}, but the keys were not typed."),
            Delivery::None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::settings::ForegroundApp;

    fn notes() -> RootInfo {
        RootInfo {
            app_name: "Notes".into(),
            bundle_id: "com.apple.Notes".into(),
            title: "Groceries".into(),
            ..RootInfo::default()
        }
    }

    fn typing() -> ForegroundAction {
        ForegroundAction::Type { characters: 12 }
    }

    /// A remembered choice for the app, or the one for other apps, decides
    /// without a question; only "ask" reaches the user, with the app, its
    /// window and what would be typed, and each answer decides the step.
    #[tokio::test(start_paused = true)]
    async fn the_remembered_choice_decides_and_otherwise_the_user_is_asked() {
        let (sender, mut asks) = tokio::sync::mpsc::unbounded_channel();
        let asker = ForegroundAsker::new("thread".into(), sender);
        let mut settings = ComputerUseSettings::default();
        settings.foreground_apps.insert(
            "com.apple.Notes".into(),
            ForegroundApp {
                name: "Notes".into(),
                allow: true,
            },
        );
        assert_eq!(
            decide(&settings, Some(&asker), &notes(), typing(), ANSWER_WAIT).await,
            (Decision::AllowedBySetting, true)
        );
        settings.foreground_apps.clear();
        settings.foreground_default = ForegroundChoice::Deny;
        assert_eq!(
            decide(&settings, Some(&asker), &notes(), typing(), ANSWER_WAIT).await,
            (Decision::DeclinedBySetting, false)
        );
        assert!(asks.try_recv().is_err(), "a setting decides without asking");

        settings.foreground_default = ForegroundChoice::Ask;
        let root = notes();
        for (answer, decision) in [
            (Some(ForegroundAnswer::Once), Decision::AllowedOnce),
            (Some(ForegroundAnswer::Always), Decision::AllowedAlways),
            (Some(ForegroundAnswer::Deny), Decision::Declined),
            (None, Decision::Unanswered),
        ] {
            let decided = decide(&settings, Some(&asker), &root, typing(), ANSWER_WAIT);
            let answered = async {
                let ask = asks.recv().await.unwrap();
                assert_eq!(
                    (
                        ask.session_id.as_str(),
                        ask.app_name.as_str(),
                        ask.bundle_id.as_str(),
                        ask.window_title.as_str(),
                        &ask.action
                    ),
                    ("thread", "Notes", "com.apple.Notes", "Groceries", &typing())
                );
                match answer {
                    Some(answer) => ask.reply.send(answer).unwrap(),
                    // The host keeps the question open; the call stops waiting.
                    None => std::mem::forget(ask.reply),
                }
            };
            let (decided, ()) = tokio::join!(decided, answered);
            assert_eq!(decided, (decision, false), "{answer:?}");
        }
    }

    /// The model reads, for each way a step was decided, what the user
    /// chose and whether the app came to the front.
    #[test]
    fn the_model_reads_what_the_user_chose() {
        let message = |decision, remembered, typed| {
            step_result(decision, remembered, "Notes", "no tap", typed).message
        };
        let start = "background key delivery could not start (no tap); ";
        let typed = "it was activated, the keys were typed, and the app the user was in was restored. No pointer events were sent.";
        assert_eq!(
            message(Decision::AllowedOnce, false, Some(Ok(()))),
            format!("{start}the user allowed bringing Notes to the front for this step: {typed}")
        );
        assert_eq!(
            message(Decision::AllowedAlways, false, Some(Ok(()))),
            format!(
                "{start}the user allowed bringing Notes to the front for typing, for this step and from now on: {typed}"
            )
        );
        assert_eq!(
            message(Decision::AllowedBySetting, true, Some(Ok(()))),
            format!(
                "{start}Notes is set to come to the front for typing in Settings → Computer Use: {typed}"
            )
        );
        assert_eq!(
            message(Decision::AllowedBySetting, false, Some(Ok(()))),
            format!(
                "{start}apps without a remembered choice are set to come to the front for typing in Settings → Computer Use: {typed}"
            )
        );
        assert_eq!(
            message(Decision::Declined, false, None),
            format!("{start}the user declined bringing Notes to the front for typing.")
        );
        assert_eq!(
            message(Decision::DeclinedBySetting, true, None),
            format!(
                "{start}Notes is set not to come to the front for typing in Settings → Computer Use."
            )
        );
        assert_eq!(
            message(Decision::Unanswered, false, None),
            format!(
                "{start}the user was asked whether Notes may come to the front and has not answered in 45 seconds. The question stays open in Tcode; calling act_ui again applies their answer once given."
            )
        );
        let allowed = step_result(
            Decision::AllowedOnce,
            false,
            "Notes",
            "no tap",
            Some(Ok(())),
        );
        assert_eq!(
            (allowed.outcome, allowed.delivery),
            (
                crate::backend::ActionOutcome::Unknown,
                Delivery::ForegroundHid
            ),
            "keys typed in front are delivered, not verified"
        );
        let declined = step_result(Decision::Declined, false, "Notes", "no tap", None);
        assert_eq!(
            (declined.outcome, declined.delivery),
            (crate::backend::ActionOutcome::Didnt, Delivery::None)
        );
    }
}
