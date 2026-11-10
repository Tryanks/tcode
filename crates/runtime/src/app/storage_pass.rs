use std::sync::atomic::{AtomicBool, Ordering};

use tcode_protocol::StorageMigration;
use tcode_services::store::{
    ChunkWait, Compaction, CompactionPlan, StepOutcome, StorageStep, StorageWork,
};

use super::*;

/// How long blob files nothing names are kept before collection, so a blob
/// written for a row about to commit is never taken.
const BLOB_GRACE: Duration = Duration::from_secs(60 * 60);
/// How long a pass that ran out of disk waits before trying again.
const NO_SPACE_RETRY: Duration = Duration::from_secs(15 * 60);
/// How often clients hear how far a running move has come.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// The background pass over the store: train the row dictionary, move an
/// older build's threads in, retire its file, compact marked threads and
/// collect unnamed blobs. Every write is one bounded step through the store
/// writer, queued only once the one before it finished, so the pass never
/// delays the writes queued behind it by more than one step.
pub(super) struct StoragePassRun {
    stop: Arc<AtomicBool>,
}

/// A step for the store writer, answered with what it did and how long it
/// held the writer.
pub(super) enum StorageJob {
    Commit(StorageStep),
    Compact(Box<CompactionPlan>),
}

pub(super) enum StorageJobDone {
    Committed(StepOutcome),
    Compacted(Box<CompactionPlan>, Option<Compaction>),
}

impl StorageJob {
    pub(super) fn run(self, store: &SessionStore) -> std::io::Result<StorageJobDone> {
        Ok(match self {
            StorageJob::Commit(step) => StorageJobDone::Committed(store.commit_storage_step(step)?),
            StorageJob::Compact(mut plan) => {
                let outcome = store.compact(&mut plan)?;
                StorageJobDone::Compacted(plan, outcome)
            }
        })
    }
}

#[derive(Default)]
struct Totals {
    moved_threads: usize,
    moved_rows: usize,
    bytes_before: u64,
    bytes_after: u64,
    stale: usize,
    compacted: usize,
    compacted_rows: usize,
    undecodable: usize,
    swept_files: usize,
    swept_bytes: u64,
    failed: usize,
    held: Vec<Duration>,
}

enum Finish {
    Done,
    Stopped,
    NoSpace,
}

impl AppState {
    /// Start the pass unless it is running.
    pub(crate) fn start_storage_pass(&mut self, cx: &mut HostCx) {
        if self.storage_pass.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.storage_pass = Some(StoragePassRun { stop: stop.clone() });
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let finish = run(&store, &host_cx, &stop).await;
            host_cx.enqueue(move |state, cx| {
                state.storage_pass = None;
                if let Finish::NoSpace = finish {
                    let mut waiting = state.storage_migration.clone();
                    if let Some(migration) = waiting.as_mut() {
                        migration.waiting_for_space = true;
                    }
                    state.replace_storage_migration(waiting, cx);
                    let host_cx = cx.clone();
                    HostCx::spawn_detached(cx, async move {
                        smol::Timer::after(NO_SPACE_RETRY).await;
                        host_cx.enqueue(|state, cx| state.start_storage_pass(cx));
                    });
                } else {
                    state.replace_storage_migration(None, cx);
                }
            });
        });
    }

    pub(super) fn stop_storage_pass(&mut self) {
        if let Some(run) = self.storage_pass.take() {
            run.stop.store(true, Ordering::Relaxed);
        }
    }

    fn replace_storage_migration(&mut self, migration: Option<StorageMigration>, cx: &mut HostCx) {
        if self.storage_migration == migration {
            return;
        }
        self.storage_migration.clone_from(&migration);
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::StorageMigration,
            event: ServerEvent::StorageMigrationReplaced(migration),
        }));
    }
}

/// Tells the host's clients how far moving an older build's threads has
/// come, at most once every [`REPORT_EVERY`].
#[derive(Default)]
struct MoveReport {
    reported: Option<Instant>,
}

impl MoveReport {
    async fn report(&mut self, store: &SessionStore, host_cx: &HostCx) {
        if self.reported.is_some_and(|at| at.elapsed() < REPORT_EVERY) {
            return;
        }
        self.reported = Some(Instant::now());
        let reader = store.clone();
        let migration = match host_cx.unblock(move || reader.move_progress()).await {
            Ok(progress) => progress.map(|(moved, total)| StorageMigration {
                moved,
                total,
                waiting_for_space: false,
            }),
            Err(error) => {
                log::warn!("could not read how far the move has come: {error}");
                return;
            }
        };
        host_cx.enqueue(move |state, cx| state.replace_storage_migration(migration, cx));
    }
}

/// Run one job through the store writer, recording how long it held it.
async fn write(
    host_cx: &HostCx,
    job: StorageJob,
    totals: &mut Totals,
) -> Result<StorageJobDone, String> {
    let (completion, done) = smol::channel::bounded(1);
    host_cx.enqueue(move |state, cx| {
        state.enqueue_store_write(StoreWrite::Storage { job, completion }, cx)
    });
    match done.recv().await {
        Ok(Ok((done, held))) => {
            totals.held.push(held);
            Ok(done)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err("the session store writer stopped".into()),
    }
}

async fn run(store: &SessionStore, host_cx: &HostCx, stop: &AtomicBool) -> Finish {
    let started = Instant::now();
    let mut totals = Totals::default();
    let finish = pass(store, host_cx, stop, &mut totals).await;
    let mut held = std::mem::take(&mut totals.held);
    held.sort();
    let p99 = held
        .get((held.len() * 99 / 100).min(held.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default();
    if held.is_empty() && totals.failed == 0 {
        log::info!("storage pass complete: no work");
        return finish;
    }
    log::info!(
        "storage pass {} in {:.1?}: moved {} thread(s), {} row(s), {} -> {} bytes ({} stale); \
         compacted {} thread(s), {} row(s) ({} undecodable); swept {} blob(s), {} bytes; \
         {} failed; {} writer step(s), held at most {:.1?}, p99 {:.1?}",
        match finish {
            Finish::Done => "complete",
            Finish::Stopped => "interrupted",
            Finish::NoSpace => "paused for disk space",
        },
        started.elapsed(),
        totals.moved_threads,
        totals.moved_rows,
        totals.bytes_before,
        totals.bytes_after,
        totals.stale,
        totals.compacted,
        totals.compacted_rows,
        totals.undecodable,
        totals.swept_files,
        totals.swept_bytes,
        totals.failed,
        held.len(),
        held.last().copied().unwrap_or_default(),
        p99,
    );
    finish
}

async fn pass(
    store: &SessionStore,
    host_cx: &HostCx,
    stop: &AtomicBool,
    totals: &mut Totals,
) -> Finish {
    let reader = store.clone();
    let work = match host_cx.unblock(move || reader.storage_work()).await {
        Ok(work) => work,
        Err(error) => {
            failed(
                host_cx,
                totals,
                format!("could not list the storage pass's work: {error}"),
            );
            return Finish::Done;
        }
    };
    let StorageWork {
        dictionary,
        moves,
        source,
        compactions,
    } = work;
    // Moving is what clients see; the steps after it are short or routine.
    let mut report = (source && !moves.is_empty()).then(MoveReport::default);
    if let Some(report) = report.as_mut() {
        report.report(store, host_cx).await;
    }
    if dictionary {
        let reader = store.clone();
        match host_cx.unblock(move || reader.prepare_dictionary()).await {
            Ok(Some(step)) => {
                if commit(host_cx, step, totals).await.is_none() {
                    return Finish::Stopped;
                }
            }
            Ok(None) => {}
            Err(error) => {
                failed(
                    host_cx,
                    totals,
                    format!("could not train the row dictionary: {error}"),
                );
            }
        }
    }
    for id in moves {
        if stop.load(Ordering::Relaxed) {
            return Finish::Stopped;
        }
        let reader = store.clone();
        let planned = id.clone();
        let mut plan = match host_cx.unblock(move || reader.plan_move(&planned)).await {
            Ok(Some(plan)) => plan,
            Ok(None) => continue,
            Err(error) => {
                failed(
                    host_cx,
                    totals,
                    format!("could not plan moving {id}: {error}"),
                );
                continue;
            }
        };
        loop {
            if stop.load(Ordering::Relaxed) {
                return Finish::Stopped;
            }
            let reader = store.clone();
            let (returned, prepared) = host_cx
                .unblock(move || {
                    let prepared = reader.prepare_move_chunk(&mut plan);
                    (plan, prepared)
                })
                .await;
            plan = returned;
            let step = match prepared {
                Ok(Ok(step)) => step,
                Ok(Err(ChunkWait::Done)) => break,
                Ok(Err(ChunkWait::NoSpace { needed, free })) => {
                    log::warn!(
                        "storage pass paused: {free} bytes free, moving {} needs {needed}",
                        plan.id()
                    );
                    return Finish::NoSpace;
                }
                Err(error) => {
                    failed(
                        host_cx,
                        totals,
                        format!("could not prepare moving {id}: {error}"),
                    );
                    break;
                }
            };
            match commit(host_cx, step, totals).await {
                None => return Finish::Stopped,
                Some(StepOutcome::Moved {
                    rows,
                    before,
                    after,
                    done,
                }) => {
                    totals.moved_rows += rows;
                    totals.bytes_before += before;
                    totals.bytes_after += after;
                    if let Some(report) = report.as_mut() {
                        report.report(store, host_cx).await;
                    }
                    if done {
                        totals.moved_threads += 1;
                        break;
                    }
                }
                Some(StepOutcome::Stale) => {
                    totals.stale += 1;
                    break;
                }
                Some(_) => break,
            }
        }
    }
    if source {
        let reader = store.clone();
        match host_cx.unblock(move || reader.retire_source()).await {
            Ok(true) => log::info!("every thread moved; the older database is retired"),
            Ok(false) => {}
            Err(error) => {
                failed(
                    host_cx,
                    totals,
                    format!("could not retire the older database: {error}"),
                );
            }
        }
    }
    if report.is_some() {
        host_cx.enqueue(|state, cx| state.replace_storage_migration(None, cx));
    }
    for id in compactions {
        if stop.load(Ordering::Relaxed) {
            return Finish::Stopped;
        }
        let reader = store.clone();
        let planned = id.clone();
        let mut plan = match host_cx
            .unblock(move || reader.plan_compaction(&planned))
            .await
        {
            Ok(plan) => Box::new(plan),
            Err(error) => {
                failed(
                    host_cx,
                    totals,
                    format!("could not read {id} to compact it: {error}"),
                );
                continue;
            }
        };
        loop {
            if stop.load(Ordering::Relaxed) {
                return Finish::Stopped;
            }
            match write(host_cx, StorageJob::Compact(plan), totals).await {
                Ok(StorageJobDone::Compacted(returned, outcome)) => {
                    plan = returned;
                    match outcome {
                        None => continue,
                        Some(Compaction::Compacted {
                            deleted, rewritten, ..
                        }) => {
                            totals.compacted += 1;
                            totals.compacted_rows += deleted + rewritten;
                        }
                        Some(Compaction::Undecodable { position }) => {
                            totals.undecodable += 1;
                            log::warn!("kept every row of {id}: event {position} does not decode");
                        }
                        Some(Compaction::Unchanged | Compaction::Deferred) => {}
                    }
                    break;
                }
                Ok(StorageJobDone::Committed(_)) => break,
                Err(error) => {
                    failed(host_cx, totals, format!("could not compact {id}: {error}"));
                    if store.is_failed() {
                        return Finish::Stopped;
                    }
                    break;
                }
            }
        }
    }
    let reader = store.clone();
    let cutoff = std::time::SystemTime::now() - BLOB_GRACE;
    match host_cx.unblock(move || reader.prepare_sweep(cutoff)).await {
        Ok(steps) => {
            for step in steps {
                if stop.load(Ordering::Relaxed) {
                    return Finish::Stopped;
                }
                if let Some(StepOutcome::Swept { files, bytes }) =
                    commit(host_cx, step, totals).await
                {
                    totals.swept_files += files;
                    totals.swept_bytes += bytes;
                }
            }
        }
        Err(error) => {
            failed(
                host_cx,
                totals,
                format!("could not list blob files to collect: {error}"),
            );
        }
    }
    Finish::Done
}

/// Commit a prepared step; `None` when it failed or the writer stopped.
async fn commit(host_cx: &HostCx, step: StorageStep, totals: &mut Totals) -> Option<StepOutcome> {
    match write(host_cx, StorageJob::Commit(step), totals).await {
        Ok(StorageJobDone::Committed(outcome)) => Some(outcome),
        Ok(StorageJobDone::Compacted(..)) => None,
        Err(error) => {
            failed(host_cx, totals, format!("a storage step failed: {error}"));
            None
        }
    }
}

/// A step of the pass that failed, reported to the host's clients with the
/// store's own words: what it left undone stays undone until the next start.
fn failed(host_cx: &HostCx, totals: &mut Totals, message: String) {
    totals.failed += 1;
    host_cx.enqueue(move |state, cx| state.report_error(RuntimeError::External(message), cx));
}
