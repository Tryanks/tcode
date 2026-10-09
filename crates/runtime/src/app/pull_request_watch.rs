use super::active_session::QueuedMessageKind;
use super::*;
use tcode_core::pull_request::{self, PullRequestKey, PullRequestSource, PullRequestState};
use tcode_core::pull_request_watch::{
    self as watch, PendingWake, PullRequestRemark, PullRequestWatch, PullRequestWatchRead,
    WatchNotice,
};
use tcode_protocol::{CommandResponse, ProtocolError};
use tcode_services::forge::{Fingerprint, Forge, ForgeError, ForgeErrorKind, Tails};

/// Passes are this far apart, counted from the end of the previous one.
const PASS_SPACING: Duration = Duration::from_secs(2 * 60);
/// Without a host fingerprint, a pull request with nothing in flight is read again only when its
/// sync snapshot moves, or after this long.
const QUIET_REREAD: Duration = Duration::from_secs(10 * 60);
/// With a host fingerprint, how long until the activity is read again anyway: the fingerprint
/// does not see edits to comments inside review threads.
const FINGERPRINT_REREAD: Duration = Duration::from_secs(30 * 60);
const GROUP_CONCURRENCY: usize = 4;

/// Reads are shared per project and pull request; each thread keeps its own watermarks.
type GroupKey = (Option<String>, PullRequestKey);

/// The last successful read of a pull request, kept in memory: a restart reads each once.
pub(super) struct LastRead {
    /// When the activity was last read.
    at: Instant,
    snapshot: String,
    /// A check was still running or mergeability unknown, so the detail can move unannounced.
    in_flight: bool,
    remarks_complete: bool,
    /// The watch generations this read evaluated; one started since takes a full first look.
    watches: HashSet<(String, String)>,
    status: Option<String>,
    remarks: Option<String>,
}

pub(super) struct WatchRuntime {
    forge: Arc<dyn Forge>,
    last_reads: HashMap<GroupKey, LastRead>,
    failures: HashMap<GroupKey, u32>,
    /// Review-thread replies past each thread's first page.
    tails: HashMap<GroupKey, Tails>,
    passing: bool,
    /// Threads whose latest run the user stopped: an agent start is refused until a new turn.
    stopped: HashSet<String>,
    worker: Option<HostTask<()>>,
}

impl WatchRuntime {
    pub(super) fn new(forge: Arc<dyn Forge>) -> Self {
        Self {
            forge,
            last_reads: HashMap::new(),
            failures: HashMap::new(),
            tails: HashMap::new(),
            passing: false,
            stopped: HashSet::new(),
            worker: None,
        }
    }
}

#[derive(Clone)]
struct Target {
    thread: String,
    started_at: String,
    url: String,
}

struct Group {
    key: GroupKey,
    targets: Vec<Target>,
    snapshot: String,
}

#[derive(Clone, Copy)]
struct Plan {
    detail: bool,
    activity: bool,
    /// The half-hour backstop is due: cached review-thread tails are paged again, so an edited
    /// reply past a thread's first page is seen though the thread's count is unchanged.
    reread_tails: bool,
}

type GroupRead = Result<(PullRequestWatchRead, Option<Option<Vec<PullRequestRemark>>>), ForgeError>;

fn snapshot_fingerprint(link: &pull_request::ThreadPullRequestLink) -> String {
    link.snapshot.as_ref().map_or_else(String::new, |snapshot| {
        format!(
            "{:?} {} {:?} {:?} {:?} {}",
            snapshot.state,
            snapshot.updated_at,
            snapshot.checks_state,
            snapshot.mergeability,
            snapshot.review_decision,
            snapshot.is_draft
        )
    })
}

fn rate_limited(error: &ForgeError) -> bool {
    matches!(
        error.kind,
        ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. }
    )
}

/// Which reads a pass makes for one pull request. With a host fingerprint the detail is read
/// when anything moved or a check is still running, the activity only when the remarks moved;
/// without one, both whenever the sync snapshot moved or something is in flight.
fn plan(
    group: &Group,
    last: Option<&LastRead>,
    fingerprint: Option<&Fingerprint>,
    now: Instant,
) -> Plan {
    let full = Plan {
        detail: true,
        activity: true,
        reread_tails: false,
    };
    let Some(last) = last.filter(|last| {
        group.targets.iter().all(|target| {
            last.watches
                .contains(&(target.thread.clone(), target.started_at.clone()))
        })
    }) else {
        return full;
    };
    let elapsed = now.saturating_duration_since(last.at);
    let Some(fingerprint) = fingerprint else {
        let read = last.in_flight
            || !last.remarks_complete
            || last.snapshot != group.snapshot
            || elapsed >= QUIET_REREAD;
        return Plan {
            detail: read,
            activity: read,
            reread_tails: false,
        };
    };
    let reread_tails = elapsed >= FINGERPRINT_REREAD;
    let activity = last.remarks.as_ref() != Some(&fingerprint.remarks)
        || !last.remarks_complete
        || reread_tails;
    Plan {
        detail: activity || last.status.as_ref() != Some(&fingerprint.status) || last.in_flight,
        activity,
        reread_tails,
    }
}

fn refused(message: &str) -> ProtocolError {
    ProtocolError {
        code: "pull_request_watch_refused".into(),
        message: message.into(),
    }
}

impl AppState {
    pub(super) fn start_pull_request_watch_worker(&mut self, cx: &mut HostCx) {
        let host = cx.clone();
        self.pull_request_watches.worker = Some(cx.spawn_background(async move {
            loop {
                let Ok(pass) = host
                    .enqueue_and_wait(|state, cx| state.sweep_pull_request_watches(cx))
                    .await
                else {
                    break;
                };
                pass.await;
                smol::Timer::after(PASS_SPACING).await;
            }
        }));
    }

    pub(super) fn stop_pull_request_watch_worker(&mut self) {
        self.pull_request_watches.worker = None;
    }

    /// One pass over every watched pull request: a batched fingerprint read, then the detail and
    /// activity reads it calls for, each shared by every thread of a project that watches it.
    pub(super) fn sweep_pull_request_watches(&mut self, cx: &mut HostCx) -> HostTask<()> {
        if std::mem::replace(&mut self.pull_request_watches.passing, true) {
            return cx.spawn_background(async {});
        }
        let mut groups: Vec<Group> = Vec::new();
        // What is kept for a pull request lasts while any thread watches it, read or not.
        let mut present = HashSet::new();
        let mut ending = Vec::new();
        let mut undelivered = Vec::new();
        for meta in &self.sessions {
            if meta.archived_at.is_some() {
                continue;
            }
            for link in pull_request::watched(&meta.pull_requests) {
                let watch = link.watch.as_ref().unwrap();
                let key = (meta.project_id.clone(), link.key.clone());
                present.insert(key.clone());
                if let Some(wake) = &watch.pending_wake {
                    undelivered.push((meta.id.clone(), link.key.clone(), wake.clone()));
                    continue;
                }
                // A merged pull request cannot reopen; settled threads and subagents hold no watch.
                if link
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.state == PullRequestState::Merged)
                    || meta.is_settled()
                    || meta.parent_session_id.is_some()
                {
                    ending.push((meta.id.clone(), link.key.clone(), watch.started_at.clone()));
                    continue;
                }
                let target = Target {
                    thread: meta.id.clone(),
                    started_at: watch.started_at.clone(),
                    url: link.url.clone(),
                };
                match groups.iter_mut().find(|group| group.key == key) {
                    Some(group) => group.targets.push(target),
                    None => groups.push(Group {
                        key,
                        targets: vec![target],
                        snapshot: String::new(),
                    }),
                }
            }
        }
        for group in &mut groups {
            let mut snapshots: Vec<_> = group
                .targets
                .iter()
                .filter_map(|target| self.find_meta(&target.thread))
                .flat_map(|meta| meta.pull_requests)
                .filter(|link| link.key == group.key.1 && link.visible())
                .map(|link| snapshot_fingerprint(&link))
                .collect();
            snapshots.sort();
            snapshots.dedup();
            group.snapshot = snapshots.join("\n");
        }
        let watches = &mut self.pull_request_watches;
        watches.last_reads.retain(|key, _| present.contains(key));
        watches.failures.retain(|key, _| present.contains(key));
        watches.tails.retain(|key, _| present.contains(key));
        for (thread, key, started_at) in ending {
            self.record_pull_request_watch(&thread, &key, &started_at, None, None, cx);
        }
        for (thread, key, wake) in undelivered {
            self.deliver_pull_request_wake(&thread, &key, &wake, cx);
        }
        let forge = self.pull_request_watches.forge.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            let keys: Vec<_> = groups.iter().map(|group| group.key.1.clone()).collect();
            let mut fingerprints = {
                let forge = forge.clone();
                host.unblock(move || forge.fingerprints(&keys)).await
            };
            if fingerprints.len() != groups.len() {
                let error = ForgeError {
                    kind: ForgeErrorKind::Uncertain,
                    description: format!(
                        "{} fingerprints answered for {} pull requests",
                        fingerprints.len(),
                        groups.len()
                    ),
                };
                fingerprints = groups.iter().map(|_| Err(error.clone())).collect();
            }
            let mut fingerprinted = Vec::new();
            for (group, fingerprint) in groups.into_iter().zip(fingerprints) {
                match fingerprint {
                    Ok(fingerprint) => fingerprinted.push((group, fingerprint)),
                    // The host asks us to wait: skip this pull request, which is no failure.
                    Err(error) if rate_limited(&error) => {}
                    // The snapshot-gated reads report a host that cannot be read.
                    Err(error) => {
                        log::debug!("pull request watch fingerprint failed: {error}");
                        fingerprinted.push((group, None));
                    }
                }
            }
            let Ok(planned) = host
                .enqueue_and_wait(move |state, _| {
                    let watches = &mut state.pull_request_watches;
                    let now = Instant::now();
                    fingerprinted
                        .into_iter()
                        .filter_map(|(group, fingerprint)| {
                            let plan = plan(
                                &group,
                                watches.last_reads.get(&group.key),
                                fingerprint.as_ref(),
                                now,
                            );
                            let tails = plan.activity.then(|| {
                                watches
                                    .tails
                                    .remove(&group.key)
                                    .filter(|_| !plan.reread_tails)
                                    .unwrap_or_default()
                            });
                            plan.detail.then_some((group, fingerprint, plan, tails))
                        })
                        .collect::<Vec<_>>()
                })
                .await
            else {
                return;
            };
            let mut planned = planned.into_iter().peekable();
            while planned.peek().is_some() {
                let reading: Vec<_> = planned
                    .by_ref()
                    .take(GROUP_CONCURRENCY)
                    .map(|(group, fingerprint, plan, tails)| {
                        let detail = {
                            let forge = forge.clone();
                            let key = group.key.1.clone();
                            host.unblock(move || forge.watch_detail(&key))
                        };
                        let activity = tails.map(|mut tails| {
                            let forge = forge.clone();
                            let key = group.key.1.clone();
                            host.unblock(move || {
                                let read = forge.activity(&key, &mut tails);
                                (read, tails)
                            })
                        });
                        (group, fingerprint, plan, detail, activity)
                    })
                    .collect();
                for (group, fingerprint, plan, detail, activity) in reading {
                    let detail = detail.await;
                    let (activity, tails) = match activity {
                        Some(task) => {
                            let (read, tails) = task.await;
                            (Some(read), Some(tails))
                        }
                        None => (None, None),
                    };
                    let read: GroupRead = match (detail, activity) {
                        (Err(error), _) | (_, Some(Err(error))) => Err(error),
                        (Ok(detail), activity) => Ok((detail, activity.map(Result::unwrap))),
                    };
                    let _ = host
                        .enqueue_and_wait(move |state, cx| {
                            if let Some(tails) = tails {
                                state
                                    .pull_request_watches
                                    .tails
                                    .insert(group.key.clone(), tails);
                            }
                            state.finish_pull_request_watch_read(
                                group,
                                fingerprint,
                                plan,
                                read,
                                cx,
                            );
                        })
                        .await;
                }
            }
            let _ = host
                .enqueue_and_wait(|state, _| state.pull_request_watches.passing = false)
                .await;
        })
    }

    fn finish_pull_request_watch_read(
        &mut self,
        group: Group,
        fingerprint: Option<Fingerprint>,
        plan: Plan,
        read: GroupRead,
        cx: &mut HostCx,
    ) {
        let Group {
            key,
            targets,
            snapshot,
        } = group;
        let (detail, activity) = match read {
            Ok(read) => read,
            Err(error) => {
                self.pull_request_watches.last_reads.remove(&key);
                // The host's pause refuses later reads without a request, so waiting is free.
                if rate_limited(&error) {
                    return;
                }
                log::warn!("pull request watch read failed: {error}");
                let failures = self
                    .pull_request_watches
                    .failures
                    .entry(key.clone())
                    .or_default();
                *failures += 1;
                if *failures < watch::READ_FAILURE_LIMIT {
                    return;
                }
                // The count stays until every stop lands, so a failed stop is tried again.
                let mut landed = true;
                for target in &targets {
                    let text = watch::unreadable_message(key.1.number, &target.url);
                    landed &= self.record_pull_request_watch(
                        &target.thread,
                        &key.1,
                        &target.started_at,
                        None,
                        Some((text, WatchNotice::Unreadable)),
                        cx,
                    );
                }
                if landed {
                    self.pull_request_watches.failures.remove(&key);
                }
                return;
            }
        };
        self.pull_request_watches.failures.remove(&key);
        if detail.state != PullRequestState::Open {
            self.pull_request_watches.last_reads.remove(&key);
            for target in &targets {
                // Merged ends silently: the badge turning merged is the news.
                let last = (detail.state == PullRequestState::Closed).then(|| {
                    (
                        watch::closed_message(key.1.number, &target.url),
                        WatchNotice::Closed,
                    )
                });
                self.record_pull_request_watch(
                    &target.thread,
                    &key.1,
                    &target.started_at,
                    None,
                    last,
                    cx,
                );
            }
            return;
        }
        // An incomplete read never advances the watermark; a pass without the activity has none.
        let remarks = activity.flatten();
        let previous = self.pull_request_watches.last_reads.get(&key);
        let read = LastRead {
            at: if plan.activity {
                Instant::now()
            } else {
                previous.map_or_else(Instant::now, |last| last.at)
            },
            snapshot,
            in_flight: detail.mergeability == tcode_core::pull_request::Mergeability::Unknown
                || detail
                    .checks
                    .iter()
                    .any(|check| check.status == watch::CheckStatus::Pending),
            remarks_complete: if plan.activity {
                remarks.is_some()
            } else {
                previous.is_some_and(|last| last.remarks_complete)
            },
            watches: targets
                .iter()
                .map(|target| (target.thread.clone(), target.started_at.clone()))
                .collect(),
            status: fingerprint.as_ref().map(|print| print.status.clone()),
            remarks: fingerprint.map(|print| print.remarks),
        };
        self.pull_request_watches
            .last_reads
            .insert(key.clone(), read);
        for target in targets {
            let Some(current) = self.find_meta(&target.thread).and_then(|meta| {
                meta.pull_requests
                    .into_iter()
                    .find(|link| link.key == key.1 && link.visible())
                    .and_then(|link| link.watch)
            }) else {
                continue;
            };
            let report = watch::evaluate(&current, &detail, remarks.as_deref());
            if report.changes.is_empty() {
                if report.next != current {
                    self.record_pull_request_watch(
                        &target.thread,
                        &key.1,
                        &target.started_at,
                        Some(report.next),
                        None,
                        cx,
                    );
                }
                continue;
            }
            let text =
                watch::update_message(key.1.number, &target.url, &detail.base_branch, &report);
            let notice = WatchNotice::Update {
                kinds: report
                    .changes
                    .iter()
                    .map(watch::WatchChange::kind)
                    .collect(),
                stopped: report.exhausted,
            };
            let next = (!report.exhausted).then_some(report.next);
            self.record_pull_request_watch(
                &target.thread,
                &key.1,
                &target.started_at,
                next,
                Some((text, notice)),
                cx,
            );
        }
    }

    /// Apply what a pass saw to one thread's watch, only while the same generation is on, so a
    /// stop or restart that landed during the read wins. `next` is the watch to keep, `None` to
    /// end it; a wake without `next` is the last message the agent reads before it ends. A wake
    /// is written in the same metadata write as the watermark it reports, then queued behind any
    /// running turn.
    fn record_pull_request_watch(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        started_at: &str,
        next: Option<PullRequestWatch>,
        wake: Option<(String, WatchNotice)>,
        cx: &mut HostCx,
    ) -> bool {
        let Some(mut meta) = self
            .find_meta(thread)
            .filter(|meta| meta.archived_at.is_none())
        else {
            return false;
        };
        let Some(link) = meta
            .pull_requests
            .iter_mut()
            .find(|link| &link.key == key && link.visible())
        else {
            return false;
        };
        let Some(current) = link
            .watch
            .as_ref()
            .filter(|current| current.started_at == started_at && current.pending_wake.is_none())
        else {
            return false;
        };
        let number = key.number;
        let delivered = match wake {
            None => {
                if next.is_none() {
                    log::info!("pull request watch ended: {thread} #{number}");
                }
                link.watch = next;
                None
            }
            Some((text, notice)) => {
                let last = next.is_none();
                let mut recorded = next.unwrap_or_else(|| current.clone());
                let pending = PendingWake {
                    id: uuid::Uuid::new_v4().to_string(),
                    text,
                    last,
                };
                recorded.pending_wake = Some(pending.clone());
                link.watch = Some(recorded);
                if last {
                    log::info!("pull request watch ending: {thread} #{number} ({notice:?})");
                }
                Some((pending, notice))
            }
        };
        self.save_pull_request_meta(meta, cx);
        if let Some((pending, notice)) = delivered {
            self.deliver_pull_request_wake(thread, key, &pending, cx);
            emit_runtime(
                cx,
                RuntimeEvent::Toast(RuntimeToast::PullRequestWatch {
                    session_id: thread.to_owned(),
                    number,
                    notice,
                }),
            );
        }
        true
    }

    /// Queue a persisted wake unless it already waits in the thread's queue.
    fn deliver_pull_request_wake(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        wake: &PendingWake,
        cx: &mut HostCx,
    ) {
        let kind = QueuedMessageKind::PullRequestWake {
            key: key.clone(),
            id: wake.id.clone(),
        };
        if self
            .resident(thread)
            .is_some_and(|session| session.queue.iter().any(|message| message.kind == kind))
        {
            return;
        }
        let text = wake.text.clone();
        self.queue_automatic_turn(
            thread,
            move |session| {
                session.push_server_message(text, kind);
            },
            cx,
        );
    }

    /// The provider accepted a wake: it is delivered, and an ending watch ends here.
    pub(super) fn acknowledge_pull_request_wake(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        id: &str,
        cx: &mut HostCx,
    ) {
        let Some(mut meta) = self.find_meta(thread) else {
            return;
        };
        let Some(link) = meta.pull_requests.iter_mut().find(|link| {
            &link.key == key
                && link
                    .watch
                    .as_ref()
                    .and_then(|watch| watch.pending_wake.as_ref())
                    .is_some_and(|wake| wake.id == id)
        }) else {
            return;
        };
        let watch = link.watch.as_mut().unwrap();
        if watch.pending_wake.take().is_some_and(|wake| wake.last) {
            link.watch = None;
        }
        self.save_pull_request_meta(meta, cx);
    }

    /// A new turn ran on this thread, so its agent may start watches again.
    pub(super) fn clear_pull_request_watch_stop(&mut self, thread: &str) {
        self.pull_request_watches.stopped.remove(thread);
    }

    /// Drop the thread's queued wakes, all of them or those of one pull request. A wake the
    /// provider is already accepting is not taken back.
    pub(super) fn discard_pull_request_wakes(
        &mut self,
        thread: &str,
        key: Option<&PullRequestKey>,
    ) {
        if let Some(session) = self.resident_mut(thread) {
            let in_flight = session.delivery_in_flight;
            session.queue.retain(|message| {
                Some(message.id) == in_flight
                    || !matches!(
                        &message.kind,
                        QueuedMessageKind::PullRequestWake { key: wake, .. }
                            if key.is_none_or(|key| key == wake)
                    )
            });
        }
    }

    /// Stop: end every watch on the thread, discard its undelivered wakes, and refuse an agent
    /// start that completes after it.
    pub(super) fn stop_pull_request_watches(&mut self, thread: &str, cx: &mut HostCx) {
        self.pull_request_watches.stopped.insert(thread.to_owned());
        self.discard_pull_request_wakes(thread, None);
        if let Some(mut meta) = self.find_meta(thread)
            && clear_watches(&mut meta)
        {
            self.save_pull_request_meta(meta, cx);
        }
    }

    /// Settling or archiving a thread ends its watches in the same write.
    pub(super) fn end_pull_request_watches(&mut self, meta: &mut SessionMeta) {
        if clear_watches(meta) {
            self.discard_pull_request_wakes(&meta.id, None);
        }
    }

    /// Start or stop watching. An agent start may link the pull request in the same write, and
    /// is refused once the user stopped the run it belongs to.
    fn set_pull_request_watch(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        url: Option<String>,
        watching: bool,
        agent: bool,
        cx: &mut HostCx,
    ) -> Result<(bool, bool), ProtocolError> {
        let mut meta = self
            .find_meta(thread)
            .ok_or_else(|| refused("Unknown thread."))?;
        let was_watching = meta
            .pull_requests
            .iter()
            .any(|link| &link.key == key && link.visible() && link.watch.is_some());
        if !watching {
            let Some(link) = meta
                .pull_requests
                .iter_mut()
                .find(|link| &link.key == key && link.watch.is_some())
            else {
                return Ok((false, false));
            };
            link.watch = None;
            self.discard_pull_request_wakes(thread, Some(key));
            self.save_pull_request_meta(meta, cx);
            return Ok((false, was_watching));
        }
        if meta.parent_session_id.is_some() {
            return Err(refused(
                "This thread is a subagent, so it cannot watch pull requests. Its parent thread owns the pull request: finish your task and report back instead.",
            ));
        }
        if meta.archived_at.is_some() {
            return Err(refused(
                "This thread is archived, so it cannot watch pull requests.",
            ));
        }
        if meta.is_settled() {
            return Err(refused(
                "This thread is settled. Unsettle it before starting a new watch.",
            ));
        }
        if agent && self.pull_request_watches.stopped.contains(thread) {
            return Err(refused(
                "The user stopped this thread, so it cannot start watching now.",
            ));
        }
        let existing = meta
            .pull_requests
            .iter()
            .find(|link| &link.key == key && link.visible());
        // A merged pull request cannot reopen. A closed one can, so its first read decides.
        if existing
            .and_then(|link| link.snapshot.as_ref())
            .is_some_and(|snapshot| snapshot.state == PullRequestState::Merged)
        {
            return Err(refused(
                "The pull request is merged, so there is nothing to watch.",
            ));
        }
        if existing.is_some_and(|link| link.watch.as_ref().is_some_and(PullRequestWatch::active)) {
            return Ok((true, true));
        }
        let linked = match (existing.is_some(), url) {
            (true, _) => false,
            (false, Some(url)) => pull_request::link_pull_request(
                &mut meta.pull_requests,
                key.clone(),
                url,
                PullRequestSource::Agent,
                now_secs(),
                true,
            ),
            (false, None) => return Err(refused("This pull request is not linked.")),
        };
        if let Some(link) = meta
            .pull_requests
            .iter_mut()
            .find(|link| &link.key == key && link.visible())
        {
            link.watch = Some(PullRequestWatch::new(now_millis()));
        }
        self.save_pull_request_meta(meta, cx);
        if linked {
            self.request_pull_request_sync(key.clone(), cx);
        }
        Ok((true, false))
    }

    /// The user's Watch for changes / Stop watching.
    pub fn watch_pull_request(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        watching: bool,
        cx: &mut HostCx,
    ) -> Result<CommandResponse, ProtocolError> {
        self.set_pull_request_watch(thread, key, None, watching, false, cx)
            .map(|_| CommandResponse::Unit)
    }

    /// `watch_pull_request` and `unwatch_pull_request` from the thread's agent.
    pub(super) fn watch_pull_request_from_agent(
        &mut self,
        thread: &str,
        key: PullRequestKey,
        url: String,
        watching: bool,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let (now, before) = self
            .set_pull_request_watch(thread, &key, Some(url.clone()), watching, true, cx)
            .map_err(|error| error.message)?;
        Ok(serde_json::json!({
            "host": key.host,
            "repository": key.repository,
            "number": key.number,
            "url": url,
            "watching": now,
            "wasWatching": before,
        }))
    }
}

fn clear_watches(meta: &mut SessionMeta) -> bool {
    let mut cleared = false;
    for link in &mut meta.pull_requests {
        cleared |= link.watch.take().is_some();
    }
    if cleared {
        log::info!("pull request watches ended: {}", meta.id);
    }
    cleared
}

#[cfg(test)]
#[path = "pull_request_watch_tests.rs"]
mod tests;
