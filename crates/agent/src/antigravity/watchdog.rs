//! A prompt Antigravity leaves open with nothing more to say.
//!
//! `agy acp` can stop sending updates in the middle of a turn while its
//! process lives on, and leave the turn's `session/prompt` unanswered for
//! good; a `session/cancel` then goes unanswered too, so the thread would
//! stay working, or stopping, until the process were killed (reported to
//! T3 Code as pingdotgg/t3code#30271, and its fix taken here at the
//! maintainer's word). A turn that hears nothing from the agent for
//! [`SILENCE`] while nothing it waits on is open fails, and the process is
//! ended, so the next message starts the session again.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smol::channel::Sender;

use super::subagents::ProactiveTracker;
use crate::acp_session::{State, TurnEnd};
use crate::{AgentEvent, Clock, SessionProcesses, TurnStatus};

/// How long an open turn may go without a word from the agent.
pub(super) const SILENCE: Duration = Duration::from_secs(15 * 60);

/// How often the silence is measured: the most it can run over [`SILENCE`].
const CHECK: Duration = Duration::from_secs(60);

/// When the agent was last heard from in the open turn.
#[derive(Default)]
pub(super) struct Watchdog {
    heard: Mutex<Option<Instant>>,
}

impl Watchdog {
    /// The agent said something about the session.
    pub(super) fn hear(&self, now: Instant) {
        *self.heard.lock().unwrap_or_else(|held| held.into_inner()) = Some(now);
    }

    /// Whether the open turn has heard nothing for [`SILENCE`] at `now`.
    /// While the agent waits on something, a question to the user, a tool
    /// of the turn or a subagent, the silence is its own and the count starts
    /// over when it ends.
    pub(super) fn silent(&self, state: &State, subagents: bool, now: Instant) -> bool {
        let mut heard = self.heard.lock().unwrap_or_else(|held| held.into_inner());
        if state.turn().is_none() {
            *heard = None;
            return false;
        }
        if state.awaiting_user() || state.tool_running() || subagents {
            *heard = Some(now);
            return false;
        }
        match *heard {
            Some(at) => now.saturating_duration_since(at) >= SILENCE,
            None => {
                *heard = Some(now);
                false
            }
        }
    }
}

/// Fail the session's open turn once it falls [`Watchdog::silent`], and end
/// the agent's process. Runs on the session's connection, the thread its
/// notifications are handled on, and waits on the host's clock.
pub(super) async fn run(
    watchdog: Arc<Watchdog>,
    state: Arc<Mutex<State>>,
    proactive: Arc<Mutex<ProactiveTracker>>,
    events: Sender<AgentEvent>,
    processes: SessionProcesses,
    clock: Clock,
) {
    loop {
        clock.timer_at(clock.now() + CHECK).await;
        let ended = {
            let mut state = state.lock().unwrap_or_else(|held| held.into_inner());
            let subagents = proactive
                .lock()
                .unwrap_or_else(|held| held.into_inner())
                .subagents_running;
            match state.turn().map(str::to_owned) {
                Some(turn) if watchdog.silent(&state, subagents, clock.now()) => {
                    let usage = state.usage();
                    Some(state.end_turn(
                        &turn,
                        TurnEnd {
                            status: TurnStatus::Failed,
                            message: Some(format!(
                                "Antigravity sent nothing for {} minutes with no tool, question or subagent open, so the turn was stopped and Antigravity was ended; the next message starts it again.",
                                SILENCE.as_secs() / 60
                            )),
                            usage,
                        },
                    ))
                }
                _ => None,
            }
        };
        let Some(ended) = ended else {
            continue;
        };
        for event in ended {
            if events.try_send(event).is_err() {
                return;
            }
        }
        processes.stop();
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_client_protocol::v1::schema as acp;
    use serde_json::json;

    use super::*;
    use crate::ProviderKind;

    fn tool(state: &mut State, status: &str) {
        let update: acp::SessionUpdate = serde_json::from_value(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call-1",
            "title": "Run command",
            "kind": "execute",
            "status": status
        }))
        .unwrap();
        let _ = state.apply_update(update);
    }

    /// An open turn fails after fifteen minutes with no word from the agent,
    /// counted from what it last said; a tool of the turn still running, or a
    /// subagent, is waited on, and the count starts over when it ends. With
    /// no turn open there is nothing to fail.
    #[test]
    fn an_open_turn_fails_after_fifteen_silent_minutes_with_nothing_running() {
        let start = Instant::now();
        let at = |minutes: u64| start + Duration::from_secs(minutes * 60);
        let watchdog = Watchdog::default();
        let mut state = State::new(ProviderKind::Antigravity, PathBuf::from("/repo"));

        assert!(!watchdog.silent(&state, false, at(60)));

        let _ = state.begin_agent_turn("turn-1");
        watchdog.hear(at(0));
        assert!(!watchdog.silent(&state, false, at(14)));
        watchdog.hear(at(10));
        assert!(!watchdog.silent(&state, false, at(24)));
        assert!(watchdog.silent(&state, false, at(25)));

        tool(&mut state, "in_progress");
        assert!(!watchdog.silent(&state, false, at(60)));
        tool(&mut state, "completed");
        assert!(!watchdog.silent(&state, false, at(74)));
        assert!(watchdog.silent(&state, false, at(75)));

        assert!(!watchdog.silent(&state, true, at(200)));
        assert!(!watchdog.silent(&state, false, at(214)));
        assert!(watchdog.silent(&state, false, at(215)));
    }
}
