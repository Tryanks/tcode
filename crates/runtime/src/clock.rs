//! The host's time.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A wait until a moment of a [`HostClock`].
pub type HostTimer = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What now is, on the monotonic clock and on the wall, and a wait until a
/// moment of it. Everything the host does by time reads it here: a running
/// host the system's, a test one it turns itself.
#[derive(Clone, Default)]
pub struct HostClock(Source);

#[derive(Clone, Default)]
enum Source {
    #[default]
    System,
    #[cfg(any(test, feature = "test-support"))]
    Manual(std::sync::Arc<ManualClock>),
}

impl HostClock {
    pub fn now(&self) -> Instant {
        match &self.0 {
            Source::System => Instant::now(),
            #[cfg(any(test, feature = "test-support"))]
            Source::Manual(clock) => clock.state().now,
        }
    }

    pub fn wall(&self) -> SystemTime {
        match &self.0 {
            Source::System => SystemTime::now(),
            #[cfg(any(test, feature = "test-support"))]
            Source::Manual(clock) => clock.state().wall,
        }
    }

    /// The wall time in unix milliseconds.
    pub fn unix_millis(&self) -> u64 {
        self.wall()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64)
    }

    /// The wall time in unix seconds.
    pub fn unix_secs(&self) -> u64 {
        self.unix_millis() / 1000
    }

    /// Ready once this clock reaches `at`.
    pub fn timer_at(&self, at: Instant) -> HostTimer {
        match &self.0 {
            Source::System => Box::pin(async move {
                smol::Timer::at(at).await;
            }),
            #[cfg(any(test, feature = "test-support"))]
            Source::Manual(clock) => clock.timer_at(at),
        }
    }

    /// Ready once `duration` of this clock has passed.
    pub fn timer(&self, duration: Duration) -> HostTimer {
        self.timer_at(self.now() + duration)
    }
}

/// A clock that moves only when it is advanced, for a host a test drives.
#[cfg(any(test, feature = "test-support"))]
pub struct ManualClock {
    state: std::sync::Mutex<ManualState>,
    armed: (smol::channel::Sender<()>, smol::channel::Receiver<()>),
}

#[cfg(any(test, feature = "test-support"))]
struct ManualState {
    now: Instant,
    wall: SystemTime,
    timers: Vec<(Instant, smol::channel::Sender<()>)>,
}

#[cfg(any(test, feature = "test-support"))]
impl ManualClock {
    /// A clock whose wall reads `wall`. The monotonic start is opaque, so it
    /// is the system's at creation and moves only by [`Self::advance`].
    pub fn starting_at(wall: SystemTime) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            state: std::sync::Mutex::new(ManualState {
                now: Instant::now(),
                wall,
                timers: Vec::new(),
            }),
            armed: smol::channel::unbounded(),
        })
    }

    pub fn host_clock(self: &std::sync::Arc<Self>) -> HostClock {
        HostClock(Source::Manual(self.clone()))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ManualState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn timer_at(&self, at: Instant) -> HostTimer {
        let mut state = self.state();
        if at <= state.now {
            return Box::pin(std::future::ready(()));
        }
        let (fire, fired) = smol::channel::bounded(1);
        state.timers.retain(|(_, fire)| !fire.is_closed());
        state.timers.push((at, fire));
        drop(state);
        let _ = self.armed.0.try_send(());
        Box::pin(async move {
            let _ = fired.recv().await;
        })
    }

    /// Move both clocks on by `duration`, firing every timer it reaches.
    pub fn advance(&self, duration: Duration) {
        let mut state = self.state();
        state.now += duration;
        state.wall += duration;
        let now = state.now;
        state.timers.retain(|(at, fire)| {
            if *at <= now {
                let _ = fire.try_send(());
                false
            } else {
                !fire.is_closed()
            }
        });
    }

    /// The earliest moment something still waits for, once something does:
    /// blocks on work arming a timer, never on this clock or the system's.
    pub async fn next_timer(&self) -> Instant {
        loop {
            {
                let mut state = self.state();
                state.timers.retain(|(_, fire)| !fire.is_closed());
                if let Some(at) = state.timers.iter().map(|(at, _)| *at).min() {
                    return at;
                }
            }
            let _ = self.armed.1.recv().await;
        }
    }

    /// Advance to the earliest timer something waits on, once something
    /// does, firing it.
    pub async fn advance_to_next_timer(&self) {
        let at = self.next_timer().await;
        let now = self.state().now;
        self.advance(at.saturating_duration_since(now));
    }

    /// Advance to the earliest timer due within `within`, once something arms
    /// one, firing it. Timers further off stay armed, as a running host's
    /// minute-long sweeps do while a terminal's frames come and go.
    pub async fn advance_to_timer_within(&self, within: Duration) {
        loop {
            {
                let mut state = self.state();
                state.timers.retain(|(_, fire)| !fire.is_closed());
                let (now, limit) = (state.now, state.now + within);
                let due = state
                    .timers
                    .iter()
                    .map(|(at, _)| *at)
                    .filter(|at| *at <= limit)
                    .min();
                if let Some(at) = due {
                    drop(state);
                    self.advance(at.saturating_duration_since(now));
                    return;
                }
            }
            let _ = self.armed.1.recv().await;
        }
    }
}
