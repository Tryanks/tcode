//! The notices the timeline draws from more than the row they stand in: a
//! change of model, a handoff to another harness, what Tcode or another
//! thread sent this thread's model, other threads' actions, the thread's
//! unreadable records, a message's settings, and the rewind of a message
//! that failed.
use super::*;
use crate::chat::components::notices::{self, Capsule, Expandable, MessageTag, Sender};
use crate::chat::model::{message_tags, one_line, standing_rewind_failure};
use crate::provider_card::{profile_glyph, provider_glyph};
use agent::ProviderKind;
use gpui::ClickEvent;
use std::path::Path;
use tcode_core::notice::{HostNotice, control_thread};

/// The expansion key of the thread's banner for records it cannot read.
const UNREADABLE_KEY: &str = "unreadable-records";

/// The harness an entry of the thread was written under, where a handoff
/// says which: the one handed to before it, else the one handed from after
/// it. `None` where the thread was never handed over, so its own is meant.
fn provider_at(entries: &[std::sync::Arc<TimelineEntry>], at: usize) -> Option<ProviderKind> {
    let relay = |entry: &TimelineEntry| match &entry.content {
        EntryContent::ProviderRelay {
            from_provider,
            to_provider,
            ..
        } => Some((*from_provider, *to_provider)),
        _ => None,
    };
    entries[..at]
        .iter()
        .rev()
        .find_map(|entry| relay(entry).map(|(_, to)| to))
        .or_else(|| {
            entries[at + 1..]
                .iter()
                .find_map(|entry| relay(entry).map(|(from, _)| from))
        })
}

/// Opens the thread `thread_id` names.
fn open_thread(thread_id: String) -> components::changed_files::ClickHandler {
    Box::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
        window.dispatch_action(
            Box::new(crate::markdown::OpenThreadAnchor {
                thread_id: thread_id.clone(),
                position: None,
            }),
            cx,
        );
    })
}

impl TimelineView {
    /// The entries of the turn the window numbers `turn`, read from the
    /// thread's timeline.
    fn turn_entries<R>(
        &self,
        turn: usize,
        read: impl FnOnce(&[Arc<TimelineEntry>]) -> R,
        cx: &App,
    ) -> Option<R> {
        self.workspace_store.read(cx).with_timeline_for(
            self.session_id.as_deref(),
            |timeline| {
                let entries = &timeline.entries;
                let start = entries.partition_point(|entry| entry.turn < turn);
                let len = entries[start..].partition_point(|entry| entry.turn == turn);
                read(&entries[start..start + len])
            },
            cx,
        )
    }

    /// The notes on its settings that follow the user message `entry_id`.
    pub(super) fn message_tags_of(&self, turn: usize, entry_id: &str, cx: &App) -> Vec<MessageTag> {
        self.turn_entries(
            turn,
            |entries| {
                entries
                    .iter()
                    .position(|entry| entry.id == entry_id)
                    .map(|at| message_tags(&entries[at..]))
                    .unwrap_or_default()
            },
            cx,
        )
        .unwrap_or_default()
    }

    /// Why the rewind to `checkpoint_id`, or with `undo` its undo, failed,
    /// while no message was sent since.
    pub(super) fn rewind_failure_label(
        &self,
        checkpoint_id: &str,
        undo: bool,
        cx: &App,
    ) -> Option<String> {
        let store = self.workspace_store.read(cx);
        let harness = store.harness_name_for(self.session_id.as_deref(), cx);
        store
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    let (mode, failure) =
                        standing_rewind_failure(&timeline.entries, checkpoint_id, undo)?;
                    let reason = components::dividers::rewind_reason(failure, &harness);
                    Some(
                        match mode {
                            Some(_) => crate::tr!("rewind.failed", reason = reason),
                            None => crate::tr!("rewind.undo_failed", reason = reason),
                        }
                        .into_owned(),
                    )
                },
                cx,
            )
            .flatten()
    }

    /// A run of one other thread's actions on this thread's run: who, which
    /// opens it, and what it did, each action told once with how often.
    pub(super) fn compose_control_actions(
        &self,
        run: &[&TimelineEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let actions: Vec<&agent::ControlAction> = run
            .iter()
            .filter_map(|entry| match &entry.content {
                EntryContent::ControlAction(action) => Some(action),
                _ => None,
            })
            .collect();
        let latest = actions.last().expect("a run holds an action");
        let mut counted: Vec<(String, usize)> = Vec::new();
        let mut details = Vec::new();
        for action in &actions {
            let label = crate::control::action_label(&action.action);
            if let Some(detail) = action.detail.as_deref().filter(|detail| !detail.is_empty()) {
                details.push(format!("{label}: {detail}"));
            }
            match counted.iter_mut().find(|(known, _)| *known == label) {
                Some((_, count)) => *count += 1,
                None => counted.push((label, 1)),
            }
        }
        let said = crate::i18n::join_list(counted.into_iter().map(|(label, count)| {
            if count > 1 {
                crate::tr!("chat.control_action_times", action = label, count = count).into_owned()
            } else {
                label
            }
        }));
        notices::control_actions_line(
            &run[0].id,
            &latest.actor_title,
            open_thread(latest.actor_thread_id.clone()),
            said,
            details,
            cx,
        )
    }

    /// The banner atop the thread's timeline while it holds records this
    /// build cannot read, opening on them as they are stored. `turn` is the
    /// turn of the timeline's first row, whose measure it changes.
    pub(super) fn compose_unreadable_banner(
        &self,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let lines = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    timeline
                        .entries
                        .iter()
                        .filter_map(|entry| match &entry.content {
                            EntryContent::Unreadable { line } => Some(line.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                },
                cx,
            )
            .unwrap_or_default();
        if lines.is_empty() {
            return None;
        }
        Some(notices::expandable_alert(
            Expandable {
                key: UNREADABLE_KEY,
                label: if lines.len() == 1 {
                    crate::tr!("chat.unreadable_record_one")
                } else {
                    crate::tr!("chat.unreadable_records", count = lines.len())
                }
                .into_owned(),
                texts: lines,
                expanded: self.expanded.contains(UNREADABLE_KEY),
            },
            cx.listener(move |this, _, _, cx| this.toggle_expanded(turn, UNREADABLE_KEY, cx)),
            &self.motion,
        ))
    }

    pub(super) fn compose_model_change(
        &self,
        entry: &TimelineEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryContent::ModelChanged { from, to, reason } = &entry.content else {
            unreachable!();
        };
        let store = self.workspace_store.read(cx);
        let session_id = self.session_id.as_deref();
        let meta = session_id.and_then(|id| store.thread_meta(id));
        // Where it stands in the thread: the harness it was under, and
        // whether anything has changed the model since.
        let (provider, latest) = store
            .with_timeline_for(
                session_id,
                |timeline| {
                    let at = timeline
                        .entries
                        .iter()
                        .rposition(|other| other.id == entry.id)?;
                    let latest = !timeline.entries[at + 1..].iter().any(|later| {
                        matches!(
                            later.content,
                            EntryContent::ModelChanged { .. } | EntryContent::ProviderRelay { .. }
                        )
                    });
                    Some((provider_at(&timeline.entries, at), latest))
                },
                cx,
            )
            .flatten()
            .unwrap_or((None, false));
        let (glyph, harness) = match provider {
            Some(provider) => (
                Some(provider_glyph(provider)),
                provider.display_name().to_owned(),
            ),
            None => (
                meta.map(|meta| profile_glyph(store, meta.provider, meta.profile_id.as_deref())),
                store.harness_name_for(session_id, cx),
            ),
        };
        let name = |model: &str| self.model_name(model, cx);
        let capsule = |model: &str| Capsule {
            glyph: glyph.clone(),
            label: name(model),
        };
        let rerouted = reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .map(|reason| {
                let note = crate::tr!(
                    "chat.model_rerouted",
                    harness = harness.as_str(),
                    to = name(to),
                    reason = reason
                )
                .into_owned();
                // Switching back sets the model the thread's next turn asks
                // for, which is offered only while nothing has changed it
                // since and the thread does not already ask for it.
                let switch_back = from
                    .clone()
                    .zip(meta.filter(|_| latest))
                    .filter(|(from, meta)| meta.model.as_deref() != Some(from.as_str()))
                    .zip(self.session_id.clone())
                    .map(|((from, meta), session_id)| {
                        let label =
                            crate::tr!("chat.model_switch_back", model = name(&from)).into_owned();
                        let (provider, profile_id, acp_agent_id) = (
                            meta.provider,
                            meta.profile_id.clone(),
                            meta.acp_agent_id.clone(),
                        );
                        let store = self.workspace_store.clone();
                        let on_click: components::changed_files::ClickHandler =
                            Box::new(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                let (session_id, from) = (session_id.clone(), from.clone());
                                store.update(cx, |store, _| match acp_agent_id.clone() {
                                    Some(agent) => store.set_active_acp_agent_for(
                                        session_id,
                                        agent,
                                        Some(from),
                                    ),
                                    None => store.set_active_model_for(
                                        session_id,
                                        provider,
                                        Some(from),
                                        profile_id.clone(),
                                    ),
                                });
                            });
                        (label, on_click)
                    });
                (note, switch_back)
            });
        notices::model_change(
            &entry.id,
            notices::ModelChange {
                from: from.as_deref().map(capsule),
                to: capsule(to),
                rerouted,
            },
            cx,
        )
    }

    pub(super) fn compose_relay(
        &self,
        turn: usize,
        entry: &TimelineEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryContent::ProviderRelay {
            from_provider,
            to_provider,
            prompt,
            moved_from,
            ..
        } = &entry.content
        else {
            unreachable!();
        };
        let (from, to) = (*from_provider, *to_provider);
        let capsule = |provider: ProviderKind| Capsule {
            glyph: Some(provider_glyph(provider)),
            label: provider.display_name().to_owned(),
        };
        let note = match moved_from {
            Some(dir) => Some(crate::tr!(
                "chat.moved",
                from = dir.to_string_lossy(),
                to = to.display_name()
            )),
            // The same harness in a session started anew: another account of
            // it, or a rewind or fork Tcode made for a harness with neither.
            None if from == to => Some(crate::tr!("chat.handed_over", to = to.display_name())),
            None => None,
        };
        let key = format!("relay-handoff-{}", entry.id);
        let toggle_key = key.clone();
        notices::relay_handoff(
            &entry.id,
            &key,
            notices::Relay {
                from: capsule(from),
                to: capsule(to),
                note: note.map(|note| note.into_owned()),
                relayed: prompt.clone(),
                expanded: self.expanded.contains(&key),
            },
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            &self.motion,
        )
    }

    /// What Tcode, or the thread a `[control]` message is about, sent this
    /// thread's model: the message as it was sent, coming in.
    pub(super) fn compose_host_message(
        &self,
        (entry, text, cwd, steering): (&TimelineEntry, &str, &Path, Option<SteeringStatus>),
        notice: &HostNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sender = match notice {
            HostNotice::Control(messages) => {
                let store = self.workspace_store.read(cx);
                let mut threads: Vec<(String, String)> = Vec::new();
                for (thread_id, title) in messages
                    .iter()
                    .filter_map(|message| control_thread(message))
                {
                    if !threads.iter().any(|(id, _)| id == thread_id) {
                        threads.push((thread_id.to_owned(), title));
                    }
                }
                Sender::Threads(
                    threads
                        .into_iter()
                        .map(|(thread_id, title)| {
                            let known = store.thread_meta(&thread_id).is_some()
                                || store
                                    .archived_sessions()
                                    .iter()
                                    .any(|meta| meta.id == thread_id);
                            let on_open = known.then(|| open_thread(thread_id));
                            (one_line(&title).into(), on_open)
                        })
                        .collect(),
                )
            }
            _ => Sender::Tcode,
        };
        notices::incoming(
            notices::Incoming {
                id: &entry.id,
                sender,
                text,
                markdown: self.md_states.get(&entry.id).map(|md| md.state.clone()),
                image_host: self.workspace_store.read(cx).image_host(),
                cwd,
                steering,
                compact: self.window_state.read(cx).compact,
            },
            window,
            cx,
        )
    }
}
