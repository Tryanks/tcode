use super::*;

/// Target minimum time for the latest activity and its immediate predecessor.
/// An activity with two newer successors folds immediately instead.
const AUTO_ACTIVITY_MIN_VISIBILITY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoActivityExpansion {
    Manual {
        expanded: bool,
    },
    Expanded {
        visible_since: Instant,
    },
    CollapsePending {
        generation: u64,
        visible_since: Instant,
    },
    Collapsed,
}

#[derive(Debug, Default)]
pub(in crate::chat) struct AutoActivityExpansions {
    entries_by_session: HashMap<String, HashMap<String, AutoActivityExpansion>>,
    /// Expandable details already present when the active session is hydrated.
    /// Each key is consumed on first render so later live updates use the
    /// ordinary minimum-visibility state machine.
    session_snapshot: Option<AutoActivitySessionSnapshot>,
    next_generation: u64,
}

#[derive(Debug)]
struct AutoActivitySessionSnapshot {
    session_key: String,
    /// `None` while the session status has switched but its asynchronous
    /// timeline snapshot has not arrived yet.
    pending_keys: Option<HashSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::chat) enum AutoActivityRecency {
    Latest,
    ImmediatelySuperseded,
    Older,
}

impl AutoActivityRecency {
    pub(in crate::chat) fn from_newer_activity_count(count: usize) -> Self {
        match count {
            0 => Self::Latest,
            1 => Self::ImmediatelySuperseded,
            _ => Self::Older,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AutoActivityObservation {
    expanded: bool,
    collapse: Option<(u64, Duration)>,
}

impl AutoActivityExpansions {
    pub(in crate::chat) fn activate_session(&mut self, session_key: Option<&str>) {
        self.session_snapshot = session_key.map(|session_key| AutoActivitySessionSnapshot {
            session_key: session_key.to_string(),
            pending_keys: None,
        });
    }

    pub(in crate::chat) fn awaiting_session_snapshot(&self, session_key: Option<&str>) -> bool {
        self.session_snapshot.as_ref().is_some_and(|snapshot| {
            Some(snapshot.session_key.as_str()) == session_key && snapshot.pending_keys.is_none()
        })
    }

    pub(in crate::chat) fn hydrate_session_snapshot(
        &mut self,
        session_key: &str,
        keys: HashSet<String>,
    ) {
        if let Some(snapshot) = self
            .session_snapshot
            .as_mut()
            .filter(|snapshot| snapshot.session_key == session_key)
            && snapshot.pending_keys.is_none()
        {
            snapshot.pending_keys = Some(keys);
        }
    }

    fn observe(
        &mut self,
        session_key: &str,
        key: &str,
        enabled: bool,
        recency: AutoActivityRecency,
        now: Instant,
    ) -> AutoActivityObservation {
        let current = self
            .entries_by_session
            .get(session_key)
            .and_then(|entries| entries.get(key))
            .copied();
        if let Some(AutoActivityExpansion::Manual { expanded }) = current {
            return AutoActivityObservation {
                expanded,
                collapse: None,
            };
        }

        if !enabled {
            if let Some(entries) = self.entries_by_session.get_mut(session_key) {
                entries.remove(key);
            }
            return AutoActivityObservation {
                expanded: false,
                collapse: None,
            };
        }

        let first_snapshot_observation = self
            .session_snapshot
            .as_mut()
            .filter(|snapshot| snapshot.session_key == session_key)
            .and_then(|snapshot| snapshot.pending_keys.as_mut())
            .is_some_and(|keys| keys.remove(key));
        if first_snapshot_observation {
            let latest = recency == AutoActivityRecency::Latest;
            let state = if latest {
                AutoActivityExpansion::Expanded { visible_since: now }
            } else {
                AutoActivityExpansion::Collapsed
            };
            self.insert(session_key, key, state);
            return AutoActivityObservation {
                expanded: latest,
                collapse: None,
            };
        }

        if recency == AutoActivityRecency::Older {
            // During rapid bursts, keep at most the current detail and its
            // immediate predecessor open; a pending timer must not hold an
            // activity open after a second successor arrives.
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            return AutoActivityObservation {
                expanded: false,
                collapse: None,
            };
        }

        let visible_since = match current {
            Some(AutoActivityExpansion::Expanded { visible_since })
            | Some(AutoActivityExpansion::CollapsePending { visible_since, .. }) => visible_since,
            _ => now,
        };
        if recency == AutoActivityRecency::Latest {
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::Expanded { visible_since },
            );
            return AutoActivityObservation {
                expanded: true,
                collapse: None,
            };
        }

        match current {
            Some(AutoActivityExpansion::CollapsePending { .. }) => {
                return AutoActivityObservation {
                    expanded: true,
                    collapse: None,
                };
            }
            Some(AutoActivityExpansion::Collapsed) => {
                return AutoActivityObservation {
                    expanded: false,
                    collapse: None,
                };
            }
            _ => {}
        }
        let remaining = AUTO_ACTIVITY_MIN_VISIBILITY
            .saturating_sub(now.saturating_duration_since(visible_since));
        if remaining.is_zero() {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            AutoActivityObservation {
                expanded: false,
                collapse: None,
            }
        } else {
            self.next_generation = self.next_generation.wrapping_add(1);
            let generation = self.next_generation;
            self.insert(
                session_key,
                key,
                AutoActivityExpansion::CollapsePending {
                    generation,
                    visible_since,
                },
            );
            AutoActivityObservation {
                expanded: true,
                collapse: Some((generation, remaining)),
            }
        }
    }

    fn finish_collapse(&mut self, session_key: &str, key: &str, generation: u64) -> bool {
        if matches!(
            self.entries_by_session
                .get(session_key)
                .and_then(|entries| entries.get(key)),
            Some(AutoActivityExpansion::CollapsePending {
                generation: pending,
                ..
            }) if *pending == generation
        ) {
            self.insert(session_key, key, AutoActivityExpansion::Collapsed);
            true
        } else {
            false
        }
    }

    fn insert(&mut self, session_key: &str, key: &str, state: AutoActivityExpansion) {
        self.entries_by_session
            .entry(session_key.to_string())
            .or_default()
            .insert(key.to_string(), state);
    }
}

pub(in crate::chat) fn auto_activity_snapshot_keys(
    entries: &[Arc<TimelineEntry>],
) -> HashSet<String> {
    let mut keys = HashSet::new();
    for entry in entries {
        match &entry.content {
            EntryContent::Item(ItemContent::CommandExecution { .. }) => {
                keys.insert(format!("activity-{}", entry.id));
            }
            EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                for (file_index, change) in changes.iter().enumerate() {
                    if live_edit_counts(change.diff.as_deref()).is_some() {
                        keys.insert(format!("activity-{}-file-{file_index}", entry.id));
                    }
                }
            }
            _ => {}
        }
    }
    keys
}

impl ChatView {
    pub(in crate::chat) fn toggle_expanded(
        &mut self,
        turn: usize,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        self.rows_regrouped = true;
        self.remeasure_expanded(turn, cx);
    }

    pub(in crate::chat) fn toggle_activity_expanded(
        &mut self,
        (turn, entry_id): (usize, &str),
        key: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_key) = self.session_key.as_deref() {
            self.auto_activity_expansions.insert(
                session_key,
                key,
                AutoActivityExpansion::Manual {
                    expanded: !expanded,
                },
            );
        }
        self.remeasure_activity(turn, entry_id, cx);
        cx.notify();
    }

    fn remeasure_expanded(&mut self, turn: usize, cx: &mut Context<Self>) {
        // The next frame refreshes the cached turn fingerprint; the direct
        // remeasure covers collapsibles whose state is intentionally not
        // fingerprinted.
        self.timeline_stale = true;
        self.remeasure_turn(turn);
        cx.notify();
    }

    pub(in crate::chat) fn auto_activity_expanded(
        &mut self,
        (turn, entry_id): (usize, &str),
        key: &str,
        enabled: bool,
        recency: AutoActivityRecency,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session_key) = self.session_key.clone() else {
            return false;
        };
        let auto = self.auto_activity_expansions.observe(
            &session_key,
            key,
            enabled,
            recency,
            cx.background_executor().now(),
        );
        if let Some((generation, delay)) = auto.collapse {
            let collapse_key = key.to_string();
            let collapse_entry_id = entry_id.to_string();
            let collapse_session_key = session_key.clone();
            let timer = cx.background_executor().timer(delay);
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    if this.auto_activity_expansions.finish_collapse(
                        &collapse_session_key,
                        &collapse_key,
                        generation,
                    ) && this.session_key.as_deref() == Some(collapse_session_key.as_str())
                    {
                        this.remeasure_activity(turn, &collapse_entry_id, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        auto.expanded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;
    use gpui::TestAppContext;
    use tcode_core::session::TurnMeta;

    /// A command's half-second is its own thread's: a collapse pending for a
    /// command in the thread the pane leaves neither closes nor shortens a
    /// command of the same id in the thread it binds to.
    #[gpui::test]
    fn a_pending_collapse_stays_with_its_thread(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let running = |commands: &[&str]| {
            let mut timeline = Timeline::default();
            timeline.turns = vec![TurnMeta {
                running: true,
                ..TurnMeta::default()
            }];
            timeline.entries = vec![entry("user", user_item("go"))];
            timeline
                .entries
                .extend(commands.iter().map(|id| command_with_output(id)));
            timeline
        };
        let mut threads = seed_two_threads(cx, running(&["shared"]));
        cx.update(|cx| cx.set_reduce_motion(true));
        let window_state = cx.new(|_| WindowState::new(false));
        let store = threads.store.clone();
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(threads.first.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));
        let show = |thread: &str, commands: &[&str], cx: &mut VisualTestContext| {
            store.update(cx, |store, cx| {
                store.set_session_replica_for_test(thread.to_string(), running(commands), cx);
                cx.notify();
            });
            draw_settled(cx);
        };
        let wait = |duration: u64, cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(duration));
            draw_settled(cx);
        };
        let (first, second) = (threads.first.clone(), threads.second.clone());

        // The first thread's command is superseded: it folds in half a second.
        draw_settled(cx);
        show(&first, &["shared", "newer"], cx);
        assert!(command_open(cx, "shared").is_some());

        // The pane moves to the second thread before it does, and a command
        // of the same id starts there later.
        threads.bind(&view, &second, running(&[]), cx);
        draw_settled(cx);
        wait(300, cx);
        show(&second, &["shared"], cx);
        assert!(command_open(cx, "shared").is_some());
        wait(100, cx);
        show(&second, &["shared", "next"], cx);
        wait(200, cx);
        assert!(
            command_open(cx, "shared").is_some(),
            "the first thread's collapse ran out the second thread's command"
        );
        wait(400, cx);
        assert!(
            command_open(cx, "shared").is_none(),
            "the second thread's command stayed past its own minimum"
        );
        threads.shutdown(cx);
    }
}
