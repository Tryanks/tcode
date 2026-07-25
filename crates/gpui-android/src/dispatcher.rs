use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    mem,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
        mpsc,
    },
    thread::{self, JoinHandle, ThreadId},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use gpui::{PlatformDispatcher, Priority, RunnableVariant};

use crate::AndroidHost;

const MIN_BACKGROUND_THREADS: usize = 2;
const MAX_BACKGROUND_THREADS: usize = 4;
const MAX_TIMER_DELAY: Duration = Duration::from_millis(i32::MAX as u64);

struct ScheduledRunnable {
    deadline: Instant,
    sequence: u64,
    runnable: RunnableVariant,
}

impl PartialEq for ScheduledRunnable {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.sequence == other.sequence
    }
}

impl Eq for ScheduledRunnable {}

impl PartialOrd for ScheduledRunnable {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledRunnable {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

struct PriorityWorkQueue<T> {
    state: Mutex<PriorityWorkQueueState<T>>,
    available: Condvar,
}

struct PriorityWorkQueueState<T> {
    high: VecDeque<T>,
    medium: VecDeque<T>,
    low: VecDeque<T>,
    closed: bool,
}

impl<T> PriorityWorkQueue<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(PriorityWorkQueueState {
                high: VecDeque::new(),
                medium: VecDeque::new(),
                low: VecDeque::new(),
                closed: false,
            }),
            available: Condvar::new(),
        }
    }

    fn push(&self, priority: Priority, item: T) -> Result<(), T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(item);
        }
        match priority {
            Priority::RealtimeAudio => {
                panic!("RealtimeAudio priority must use spawn_realtime")
            }
            Priority::High => state.high.push_back(item),
            Priority::Medium => state.medium.push_back(item),
            Priority::Low => state.low.push_back(item),
        }
        drop(state);
        self.available.notify_one();
        Ok(())
    }

    fn try_pop(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pop_highest_priority(&mut state)
    }

    fn pop(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(item) = pop_highest_priority(&mut state) {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn is_empty(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.high.is_empty() && state.medium.is_empty() && state.low.is_empty()
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = true;
        self.available.notify_all();
    }
}

fn pop_highest_priority<T>(state: &mut PriorityWorkQueueState<T>) -> Option<T> {
    state
        .high
        .pop_front()
        .or_else(|| state.medium.pop_front())
        .or_else(|| state.low.pop_front())
}

/// GPUI dispatcher backed by Android's one UI Looper plus Rust worker threads.
pub struct AndroidDispatcher {
    host: Arc<dyn AndroidHost>,
    main_thread_id: ThreadId,
    main_queue: Arc<PriorityWorkQueue<RunnableVariant>>,
    main_wake_pending: AtomicBool,
    background_queue: Arc<PriorityWorkQueue<RunnableVariant>>,
    timer_sender: mpsc::Sender<ScheduledRunnable>,
    timer_sequence: AtomicU64,
    _background_threads: Vec<JoinHandle<()>>,
    _timer_thread: JoinHandle<()>,
}

impl AndroidDispatcher {
    /// Creates worker queues while recording the current thread as Android's UI thread.
    pub fn new(host: Arc<dyn AndroidHost>) -> Result<Self> {
        let main_queue = Arc::new(PriorityWorkQueue::new());
        let background_queue = Arc::new(PriorityWorkQueue::new());

        let thread_count = thread::available_parallelism()
            .map(|count| count.get().saturating_sub(1))
            .unwrap_or(MIN_BACKGROUND_THREADS)
            .clamp(MIN_BACKGROUND_THREADS, MAX_BACKGROUND_THREADS);

        let mut background_threads = Vec::with_capacity(thread_count);
        for index in 0..thread_count {
            let queue = background_queue.clone();
            let worker = thread::Builder::new()
                .name(format!("gpui-worker-{index}"))
                .spawn(move || {
                    while let Some(runnable) = queue.pop() {
                        execute_runnable(runnable);
                    }
                })
                .with_context(|| format!("failed to spawn GPUI worker {index}"))?;
            background_threads.push(worker);
        }

        let (timer_sender, timer_receiver) = mpsc::channel();
        let timer_thread = thread::Builder::new()
            .name("gpui-timer".into())
            .spawn(move || run_timer_thread(timer_receiver))
            .context("failed to spawn GPUI timer thread")?;

        Ok(Self {
            host,
            main_thread_id: thread::current().id(),
            main_queue,
            main_wake_pending: AtomicBool::new(false),
            background_queue,
            timer_sender,
            timer_sequence: AtomicU64::new(0),
            _background_threads: background_threads,
            _timer_thread: timer_thread,
        })
    }

    /// Runs all queued foreground work. The Android main Looper calls this after a wake.
    pub fn drain_main_thread(&self) {
        assert!(
            self.is_main_thread(),
            "Android main-thread queue drained from a non-main thread"
        );

        loop {
            loop {
                let runnable = self.main_queue.try_pop();
                let Some(runnable) = runnable else {
                    break;
                };
                execute_runnable(runnable);
            }

            self.main_wake_pending.store(false, AtomicOrdering::Release);
            if self.main_queue.is_empty() {
                break;
            }

            // A producer raced with the flag reset. Drain its item here; its posted
            // Looper wake may later find an empty queue, which is harmless.
            self.main_wake_pending.store(true, AtomicOrdering::Release);
        }
    }

    fn enqueue_main(&self, runnable: RunnableVariant, priority: Priority) {
        if let Err(runnable) = self.main_queue.push(priority, runnable) {
            log::error!("Android main-thread queue disconnected during shutdown");
            // A foreground runnable may own !Send state and cannot be dropped here.
            mem::forget(runnable);
            return;
        }

        if !self.main_wake_pending.swap(true, AtomicOrdering::AcqRel) {
            self.host.wake_main_thread();
        }
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        if priority == Priority::RealtimeAudio {
            panic!("RealtimeAudio priority must use spawn_realtime");
        }
        if let Err(runnable) = self.background_queue.push(priority, runnable) {
            log::error!("Android background queue disconnected during shutdown");
            mem::forget(runnable);
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.enqueue_main(runnable, priority);
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let duration = duration.min(MAX_TIMER_DELAY);
        let scheduled = ScheduledRunnable {
            deadline: Instant::now() + duration,
            sequence: self.timer_sequence.fetch_add(1, AtomicOrdering::Relaxed),
            runnable,
        };
        if let Err(error) = self.timer_sender.send(scheduled) {
            log::error!("Android timer queue disconnected during shutdown");
            mem::forget(error.0.runnable);
        }
    }

    fn spawn_realtime(&self, function: Box<dyn FnOnce() + Send>) {
        // Android realtime scheduling needs audio-specific Java/AAudio policy; a
        // dedicated thread preserves isolation until that policy is supplied.
        if let Err(error) = thread::Builder::new()
            .name("gpui-realtime".into())
            .spawn(function)
        {
            log::error!("failed to spawn Android realtime thread: {error}");
        }
    }
}

impl Drop for AndroidDispatcher {
    fn drop(&mut self) {
        self.main_queue.close();
        // The final dispatcher Arc can be released by a worker; leak queued
        // foreground futures rather than dropping their !Send state there.
        while let Some(runnable) = self.main_queue.try_pop() {
            mem::forget(runnable);
        }
        self.background_queue.close();
    }
}

fn execute_runnable(runnable: RunnableVariant) {
    let location = runnable.metadata().location;
    let spawned = runnable.metadata().spawned;
    gpui::profiler::update_running_task(spawned, location);
    runnable.run();
    gpui::profiler::save_task_timing();
}

fn run_timer_thread(receiver: mpsc::Receiver<ScheduledRunnable>) {
    let mut pending = BinaryHeap::new();
    loop {
        let next = pending.peek().map(|entry: &ScheduledRunnable| {
            entry.deadline.saturating_duration_since(Instant::now())
        });

        let received = match next {
            Some(timeout) => receiver.recv_timeout(timeout),
            None => match receiver.recv() {
                Ok(item) => {
                    pending.push(item);
                    continue;
                }
                Err(_) => break,
            },
        };

        match received {
            Ok(item) => pending.push(item),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(item) = pending.pop() {
                    execute_runnable(item.runnable);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Dropping scheduled runnables cancels awaiters; during shutdown they must
    // remain pending instead.
    for item in pending {
        mem::forget(item.runnable);
    }
}
