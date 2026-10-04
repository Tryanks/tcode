use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

use tcode_services::store::{CompactOutcome, CompactRejection, MalformedLine};

use super::*;

/// One log compaction queued in the store writer, which runs it in the
/// writer's order: every append queued before it is in the log it reads, and
/// none queued after it can land between its read and its rename.
pub(super) struct CompactionRequest {
    /// Set when the request became moot before the writer reached it: a later
    /// request for the same log covers it, or the background pass's log
    /// became resident.
    pub(super) withdrawn: AtomicBool,
    /// Part of the background pass over existing logs, which leaves a log an
    /// earlier run already rewrote to the live triggers.
    pub(super) pass: bool,
}

/// What the store writer did with a [`CompactionRequest`].
pub(super) enum Compacted {
    Skipped,
    Done(CompactOutcome),
}

#[derive(Default)]
pub(super) struct Compactions {
    /// The request queued for each log, until the writer reports it done.
    queued: HashMap<String, Arc<CompactionRequest>>,
    /// The background pass over existing logs, while it runs.
    pass: Option<CompactionPass>,
}

/// Compacts the logs that existed at startup one at a time, so it never holds
/// more than one log in memory and never delays the writer by more than one
/// compaction.
struct CompactionPass {
    remaining: VecDeque<String>,
    started: Instant,
    rewritten: usize,
    unchanged: usize,
    rejected: usize,
    skipped: usize,
    bytes_before: u64,
    bytes_after: u64,
}

impl AppState {
    /// Compact a session's log once every write queued so far is in it. A
    /// request still queued for the log is withdrawn: this one runs later and
    /// covers everything that one would have.
    pub(super) fn schedule_compaction(&mut self, session_id: &str, cx: &mut HostCx) {
        self.queue_compaction(session_id, false, cx);
    }

    fn queue_compaction(&mut self, session_id: &str, pass: bool, cx: &mut HostCx) {
        let request = Arc::new(CompactionRequest {
            withdrawn: AtomicBool::new(false),
            pass,
        });
        if let Some(earlier) = self
            .compactions
            .queued
            .insert(session_id.to_string(), request.clone())
        {
            earlier.withdrawn.store(true, Ordering::Relaxed);
        }
        self.enqueue_store_write(
            StoreWrite::CompactLog {
                id: session_id.to_string(),
                request,
            },
            cx,
        );
    }

    /// A log that became resident is compacted by its turns and its release;
    /// rewriting it now would only renumber it for the next load.
    pub(super) fn withdraw_pass_compaction(&mut self, session_id: &str) {
        if let Some(request) = self.compactions.queued.get(session_id)
            && request.pass
        {
            request.withdrawn.store(true, Ordering::Relaxed);
            self.compactions.queued.remove(session_id);
        }
    }

    /// Delete what earlier runs' compactions left behind, then compact the
    /// existing logs in the background. Runs once the host has started, before
    /// anything in this run can compact: the originals this run keeps must
    /// survive until the next start.
    pub(crate) fn start_log_compaction(&mut self, cx: &mut HostCx) {
        self.enqueue_store_write(StoreWrite::DiscardCompactionLeftovers, cx);
        self.compactions.pass = Some(CompactionPass {
            remaining: self.sessions.iter().map(|meta| meta.id.clone()).collect(),
            started: Instant::now(),
            rewritten: 0,
            unchanged: 0,
            rejected: 0,
            skipped: 0,
            bytes_before: 0,
            bytes_after: 0,
        });
        self.continue_compaction_pass(cx);
    }

    pub(super) fn stop_compaction_pass(&mut self) {
        self.compactions.pass = None;
    }

    /// Queue the next cold log of the pass, or end the pass.
    fn continue_compaction_pass(&mut self, cx: &mut HostCx) {
        loop {
            let Some(pass) = self.compactions.pass.as_mut() else {
                return;
            };
            let Some(session_id) = pass.remaining.pop_front() else {
                let pass = self.compactions.pass.take().expect("checked above");
                log::info!(
                    "compacted existing logs in {:.1?}: {} rewritten ({} -> {} bytes), {} unchanged, {} rejected, {} skipped as open or rewritten by an earlier run",
                    pass.started.elapsed(),
                    pass.rewritten,
                    pass.bytes_before,
                    pass.bytes_after,
                    pass.unchanged,
                    pass.rejected,
                    pass.skipped,
                );
                return;
            };
            let cold = !self.event_records.contains_key(&session_id)
                && self.resident(&session_id).is_none()
                && !self.compactions.queued.contains_key(&session_id);
            if cold {
                self.queue_compaction(&session_id, true, cx);
                return;
            }
            if let Some(pass) = self.compactions.pass.as_mut() {
                pass.skipped += 1;
            }
        }
    }

    pub(super) fn compaction_finished(
        &mut self,
        session_id: String,
        request: Arc<CompactionRequest>,
        compacted: Compacted,
        cx: &mut HostCx,
    ) {
        if self
            .compactions
            .queued
            .get(&session_id)
            .is_some_and(|queued| Arc::ptr_eq(queued, &request))
        {
            self.compactions.queued.remove(&session_id);
        }
        if let Compacted::Done(CompactOutcome::Rejected(rejection)) = &compacted {
            match rejection {
                CompactRejection::Malformed { line, reason } => log::warn!(
                    "left {session_id}.jsonl uncompacted: line {line} is {}",
                    match reason {
                        MalformedLine::NotUtf8 => "not UTF-8",
                        MalformedLine::Truncated => "cut short",
                        MalformedLine::Blank => "blank",
                        MalformedLine::Unparseable => "not a record",
                    }
                ),
                CompactRejection::Gate => log::warn!(
                    "left {session_id}.jsonl uncompacted: the compacted records fold differently"
                ),
                CompactRejection::Io(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!("could not compact {session_id}.jsonl: {error}")
                }
                CompactRejection::Io(_) => {}
            }
        }
        if !request.pass {
            return;
        }
        if let Some(pass) = self.compactions.pass.as_mut() {
            match compacted {
                Compacted::Skipped => pass.skipped += 1,
                Compacted::Done(CompactOutcome::Rewritten { before, after }) => {
                    pass.rewritten += 1;
                    pass.bytes_before += before;
                    pass.bytes_after += after;
                }
                Compacted::Done(CompactOutcome::Unchanged) => pass.unchanged += 1,
                Compacted::Done(CompactOutcome::Rejected(_)) => pass.rejected += 1,
            }
        }
        self.continue_compaction_pass(cx);
    }
}
