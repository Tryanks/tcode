//! The tasks a running machine starts, each of which may hold the machine,
//! its endpoint or a connection on it.
//!
//! [`TraverseHost::shutdown`](super::TraverseHost::shutdown) ends them all
//! and waits until each is gone, so that the machine's own handle is the
//! last one on the endpoint's UDP sockets and they close on the caller's
//! thread before it returns. netwatch closes a socket dropped on a runtime
//! thread later, on the blocking pool; Settings starts the machine again on
//! the same port at once to apply a new name or Traverse instance, and that
//! bind found the port still taken.
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use tokio::task::AbortHandle;

#[derive(Clone)]
pub(super) struct Tasks(Arc<Inner>);

struct Inner {
    /// The tasks still running, by key; `None` once the machine is shutting
    /// down, after which nothing new starts.
    running: Mutex<Option<HashMap<u64, AbortHandle>>>,
    next: AtomicU64,
    /// Every task holds a clone, dropped after the task's own future.
    alive: Mutex<Option<async_channel::Sender<()>>>,
    /// Closed once the last clone of `alive` is gone.
    gone: async_channel::Receiver<()>,
}

impl Tasks {
    pub(super) fn new() -> Self {
        let (alive, gone) = async_channel::bounded(1);
        Self(Arc::new(Inner {
            running: Mutex::new(Some(HashMap::new())),
            next: AtomicU64::new(0),
            alive: Mutex::new(Some(alive)),
            gone,
        }))
    }

    /// Run `task` on the runtime until it ends or the machine shuts down. A
    /// task asked for once shutdown has begun is dropped unstarted.
    pub(super) fn spawn(
        &self,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Option<AbortHandle> {
        let alive = self.0.alive.lock().unwrap().clone()?;
        let key = self.0.next.fetch_add(1, Ordering::Relaxed);
        let handle = crate::runtime::runtime()
            .spawn(Tracked {
                task: Box::pin(task),
                _ended: Ended {
                    key,
                    tasks: Arc::downgrade(&self.0),
                    _alive: alive,
                },
            })
            .abort_handle();
        match self.0.running.lock().unwrap().as_mut() {
            // A task that ended before this point has already looked for
            // its entry; one ending from here on finds it.
            Some(running) if !handle.is_finished() => {
                running.insert(key, handle.clone());
            }
            Some(_) => {}
            // Shutdown began meanwhile; it waits for this one all the same.
            None => handle.abort(),
        }
        Some(handle)
    }

    /// End every task and wait until all of them are gone.
    pub(super) async fn end(&self) {
        let running = self.0.running.lock().unwrap().take();
        self.0.alive.lock().unwrap().take();
        for task in running.into_iter().flat_map(HashMap::into_values) {
            task.abort();
        }
        let _ = self.0.gone.recv().await;
    }
}

/// A task's future, which is dropped before the token saying it is gone.
struct Tracked {
    task: Pin<Box<dyn Future<Output = ()> + Send>>,
    _ended: Ended,
}

impl Future for Tracked {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.task.as_mut().poll(cx)
    }
}

struct Ended {
    key: u64,
    tasks: Weak<Inner>,
    _alive: async_channel::Sender<()>,
}

impl Drop for Ended {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.upgrade()
            && let Some(running) = tasks.running.lock().unwrap().as_mut()
        {
            running.remove(&self.key);
        }
    }
}
