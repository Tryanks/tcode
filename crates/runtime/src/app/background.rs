//! A thread's background work as its host lists it to clients: what the
//! harness lists, the stops sent for it, and the calls going on in the
//! background that the harness does not list.

use std::collections::{HashMap, HashSet};

use agent::{BackgroundKind, BackgroundTask, ItemContent};
use tcode_core::session::Timeline;
use tcode_protocol::BackgroundTaskStatus;

/// The background work of one live harness process.
#[derive(Default)]
pub(super) struct BackgroundWork {
    /// What the harness lists now, in its order.
    tasks: Vec<BackgroundTask>,
    /// When a stop went out for each listed task it has not dropped yet
    /// (unix ms).
    stops: HashMap<String, u64>,
    /// The calls the harness has listed work of: one it stops listing
    /// while the call is still open has ended, and the harness has yet to
    /// say so in the conversation.
    listed_calls: HashSet<String>,
}

impl BackgroundWork {
    /// The harness's list now, replacing the last.
    pub(super) fn list(&mut self, tasks: &[BackgroundTask]) {
        self.stops
            .retain(|id, _| tasks.iter().any(|task| &task.id == id));
        self.listed_calls
            .extend(tasks.iter().filter_map(|task| task.item_id.clone()));
        self.tasks = tasks.to_vec();
    }

    /// A stop went out at `now` for `task_id`, or for everything listed.
    pub(super) fn stopping(&mut self, task_id: Option<&str>, now: u64) {
        for task in &self.tasks {
            if task_id.is_none_or(|id| id == task.id) {
                self.stops.entry(task.id.clone()).or_insert(now);
            }
        }
    }

    /// The process is gone, and its work with it.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// The work as clients see it: what the harness lists, in its order,
    /// then the commands Tcode's task tools run for the thread in the
    /// background (`host`), then the calls of `timeline` going on in the
    /// background that none of that claims.
    pub(super) fn statuses(
        &self,
        timeline: &Timeline,
        host: &[BackgroundTask],
    ) -> Vec<BackgroundTaskStatus> {
        let claimed: HashSet<&str> = self
            .tasks
            .iter()
            .chain(host)
            .filter_map(|task| task.item_id.as_deref())
            .collect();
        let listed = self
            .tasks
            .iter()
            .map(|task| BackgroundTaskStatus {
                task: task.clone(),
                listed: true,
                stopping_since: self.stops.get(&task.id).copied(),
                ended: false,
            })
            .chain(host.iter().map(|task| BackgroundTaskStatus {
                task: task.clone(),
                listed: true,
                stopping_since: None,
                ended: false,
            }));
        let unlisted = timeline
            .running_background_items()
            .filter(|(entry, _)| !claimed.contains(entry.id.as_str()))
            .map(|(entry, content)| {
                let (kind, name) = match content {
                    ItemContent::CommandExecution { command, .. } => {
                        (BackgroundKind::Command, command.clone())
                    }
                    ItemContent::Subagent {
                        description,
                        agent_type,
                        ..
                    } => (
                        BackgroundKind::Agent,
                        if description.trim().is_empty() {
                            agent_type.clone()
                        } else {
                            description.clone()
                        },
                    ),
                    ItemContent::ToolCall { name, .. } => (BackgroundKind::Other, name.clone()),
                    _ => (BackgroundKind::Other, entry.id.clone()),
                };
                BackgroundTaskStatus {
                    task: BackgroundTask {
                        id: entry.id.clone(),
                        name,
                        kind,
                        item_id: Some(entry.id.clone()),
                        started_at: entry.ts.unwrap_or_default(),
                        started_reported: false,
                    },
                    listed: false,
                    stopping_since: None,
                    ended: self.listed_calls.contains(&entry.id),
                }
            });
        listed.chain(unlisted).collect()
    }
}
