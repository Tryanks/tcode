use android_activity::AndroidAppWaker;
use gpui::{PlatformDispatcher, Priority, RunnableVariant};
use parking_lot::{Condvar, Mutex};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MIN_BACKGROUND_THREADS: usize = 2;

pub(crate) struct AndroidDispatcher {
    main_queue: Mutex<RunnableQueues>,
    background_queue: Arc<SharedQueue>,
    timers: Arc<TimerQueue>,
    main_thread: thread::ThreadId,
    waker: AndroidAppWaker,
    _workers: Vec<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RunnableQueues {
    realtime: VecDeque<RunnableVariant>,
    high: VecDeque<RunnableVariant>,
    medium: VecDeque<RunnableVariant>,
    low: VecDeque<RunnableVariant>,
}

impl RunnableQueues {
    fn push(&mut self, priority: Priority, runnable: RunnableVariant) {
        match priority {
            Priority::RealtimeAudio => self.realtime.push_back(runnable),
            Priority::High => self.high.push_back(runnable),
            Priority::Medium => self.medium.push_back(runnable),
            Priority::Low => self.low.push_back(runnable),
        }
    }

    fn pop(&mut self) -> Option<RunnableVariant> {
        self.realtime
            .pop_front()
            .or_else(|| self.high.pop_front())
            .or_else(|| self.medium.pop_front())
            .or_else(|| self.low.pop_front())
    }
}

#[derive(Default)]
struct SharedQueue {
    runnables: Mutex<RunnableQueues>,
    ready: Condvar,
}

impl SharedQueue {
    fn push(&self, priority: Priority, runnable: RunnableVariant) {
        self.runnables.lock().push(priority, runnable);
        self.ready.notify_one();
    }

    fn pop(&self) -> RunnableVariant {
        let mut runnables = self.runnables.lock();
        loop {
            if let Some(runnable) = runnables.pop() {
                return runnable;
            }
            self.ready.wait(&mut runnables);
        }
    }
}

struct Timer {
    due: Instant,
    seq: u64,
    runnable: RunnableVariant,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Timer {}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timer {
    /// Reversed, so the max-heap's top is the earliest deadline, and the
    /// earliest-armed timer among equal deadlines.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .due
            .cmp(&self.due)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

#[derive(Default)]
struct TimerHeap {
    timers: BinaryHeap<Timer>,
    next_seq: u64,
}

#[derive(Default)]
struct TimerQueue {
    heap: Mutex<TimerHeap>,
    changed: Condvar,
}

impl TimerQueue {
    fn push(&self, due: Instant, runnable: RunnableVariant) {
        let mut heap = self.heap.lock();
        let seq = heap.next_seq;
        heap.next_seq += 1;
        heap.timers.push(Timer { due, seq, runnable });
        // Only a new earliest deadline shortens the timer thread's wait.
        if heap.timers.peek().is_some_and(|first| first.seq == seq) {
            self.changed.notify_one();
        }
    }

    fn pop_due(&self) -> RunnableVariant {
        let mut heap = self.heap.lock();
        loop {
            match heap.timers.peek().map(|first| first.due) {
                Some(due) if due <= Instant::now() => {
                    return heap.timers.pop().expect("peeked timer").runnable;
                }
                Some(due) => {
                    self.changed.wait_until(&mut heap, due);
                }
                None => self.changed.wait(&mut heap),
            }
        }
    }
}

impl AndroidDispatcher {
    pub(crate) fn new(waker: AndroidAppWaker) -> Arc<Self> {
        let background_queue = Arc::new(SharedQueue::default());
        let thread_count = thread::available_parallelism()
            .map_or(MIN_BACKGROUND_THREADS, |count| {
                count.get().max(MIN_BACKGROUND_THREADS)
            });

        let workers = (0..thread_count)
            .map(|index| {
                let queue = background_queue.clone();
                thread::Builder::new()
                    .name(format!("gpui-worker-{index}"))
                    .spawn(move || {
                        loop {
                            let runnable = queue.pop();
                            runnable.run();
                        }
                    })
                    .expect("failed to spawn GPUI background worker")
            })
            .collect();

        let timers = Arc::new(TimerQueue::default());
        thread::Builder::new()
            .name("gpui-timer".into())
            .spawn({
                let timers = timers.clone();
                move || {
                    loop {
                        // Run outside the lock: a timer may arm another.
                        timers.pop_due().run();
                    }
                }
            })
            .expect("failed to spawn GPUI timer thread");

        Arc::new(Self {
            main_queue: Mutex::new(RunnableQueues::default()),
            background_queue,
            timers,
            main_thread: thread::current().id(),
            waker,
            _workers: workers,
        })
    }

    pub(crate) fn drain_main_queue(&self) {
        loop {
            let runnable = self.main_queue.lock().pop();
            let Some(runnable) = runnable else {
                break;
            };
            runnable.run();
        }
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        self.background_queue.push(priority, runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.main_queue.lock().push(priority, runnable);
        self.waker.wake();
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let now = Instant::now();
        // A duration past the clock's range (such as `Duration::MAX`) never
        // comes due; dropping its runnable instead would resolve the timer.
        let due = now
            .checked_add(duration)
            .unwrap_or_else(|| now + Duration::from_secs(100 * 365 * 24 * 60 * 60));
        self.timers.push(due, runnable);
    }

    fn spawn_realtime(&self, callback: Box<dyn FnOnce() + Send>) {
        thread::Builder::new()
            .name("gpui-realtime".into())
            .spawn(callback)
            .expect("failed to spawn GPUI realtime thread");
    }
}
