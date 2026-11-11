//! The host's time, as a session reads it for a wait on the harness.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

/// What now is, and a wait until a moment of it. The host gives its own,
/// so a session waits on the time the rest of the host waits on.
pub trait Timekeeper: Send + Sync {
    fn now(&self) -> Instant;
    fn timer_at(&self, at: Instant) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// The [`Timekeeper`] a session was started with: the system's unless the
/// host gave another.
#[derive(Clone)]
pub struct Clock(Arc<dyn Timekeeper>);

impl Clock {
    pub fn new(timekeeper: impl Timekeeper + 'static) -> Self {
        Self(Arc::new(timekeeper))
    }

    pub fn now(&self) -> Instant {
        self.0.now()
    }

    pub fn timer_at(&self, at: Instant) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        self.0.timer_at(at)
    }
}

struct System;

impl Timekeeper for System {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn timer_at(&self, at: Instant) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            smol::Timer::at(at).await;
        })
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(System)
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}
