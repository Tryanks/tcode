//! Runtime-owned host execution seam.
//!
//! `AppState` is still a gpui entity in this migration step. All gpui
//! mechanics are confined here so state methods can survive replacing the
//! implementation with a dedicated-thread actor mailbox.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use gpui::{BackgroundExecutor, Context, Task};

use crate::app::AppState;
use crate::event::HostEvent;

type HostFn = Box<dyn FnOnce(&mut AppState, &mut HostCx) + Send + 'static>;

pub(crate) enum HostCommand {
    Run(HostFn),
    Emit(Box<HostEvent>),
    Notify,
    Track(async_channel::Receiver<()>),
}

#[derive(Default)]
struct ImmediateEffects {
    events: Vec<HostEvent>,
    notify: bool,
}

/// A runtime task handle independent of the current host implementation.
#[must_use = "tasks must be awaited or detached"]
pub struct HostTask<T>(Task<T>);

impl<T> HostTask<T> {
    fn new(task: Task<T>) -> Self {
        Self(task)
    }

    /// Keep running the task after dropping its handle.
    pub fn detach(self) {
        self.0.detach();
    }
}

impl<T: 'static> Future for HostTask<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // `self.0` is structurally pinned with `self`; it is never moved while
        // this `HostTask` is pinned.
        let this = unsafe { self.get_unchecked_mut() };
        unsafe { Pin::new_unchecked(&mut this.0) }.poll(cx)
    }
}

/// The only execution/event context accepted by [`AppState`] methods.
///
/// Clones are background-safe handles. The adapter-created root additionally
/// buffers synchronous effects so gpui observers see them before the entity
/// update returns, matching the pre-seam behavior.
pub struct HostCx {
    executor: BackgroundExecutor,
    commands: async_channel::Sender<HostCommand>,
    immediate: Option<ImmediateEffects>,
}

impl Clone for HostCx {
    fn clone(&self) -> Self {
        Self {
            executor: self.executor.clone(),
            commands: self.commands.clone(),
            immediate: None,
        }
    }
}

impl HostCx {
    pub fn emit(&mut self, event: HostEvent) {
        if let Some(immediate) = self.immediate.as_mut() {
            immediate.events.push(event);
        } else {
            let _ = self.commands.try_send(HostCommand::Emit(Box::new(event)));
        }
    }

    pub fn notify(&mut self) {
        if let Some(immediate) = self.immediate.as_mut() {
            immediate.notify = true;
        } else {
            let _ = self.commands.try_send(HostCommand::Notify);
        }
    }

    pub fn spawn_background<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> HostTask<T> {
        HostTask::new(self.executor.spawn(fut))
    }

    pub fn enqueue(&self, f: impl FnOnce(&mut AppState, &mut HostCx) + Send + 'static) {
        let _ = self.commands.try_send(HostCommand::Run(Box::new(f)));
    }

    pub(crate) async fn enqueue_and_wait<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ()> {
        let (sender, receiver) = async_channel::bounded(1);
        self.commands
            .send(HostCommand::Run(Box::new(move |state, cx| {
                let _ = sender.try_send(f(state, cx));
            })))
            .await
            .map_err(|_| ())?;
        receiver.recv().await.map_err(|_| ())
    }

    pub(crate) fn timer(&self, duration: std::time::Duration) -> HostTask<()> {
        HostTask::new(self.executor.timer(duration))
    }

    pub fn spawn_detached(&self, fut: impl Future<Output = ()> + Send + 'static) {
        let (done, completed) = async_channel::bounded(1);
        let _ = self.commands.try_send(HostCommand::Track(completed));
        self.spawn_background(async move {
            fut.await;
            let _ = done.try_send(());
        })
        .detach();
    }

    pub(crate) fn adapt<R>(
        state: &mut AppState,
        cx: &mut Context<AppState>,
        commands: async_channel::Sender<HostCommand>,
        receiver: Option<async_channel::Receiver<HostCommand>>,
        f: impl FnOnce(&mut AppState, &mut HostCx) -> R,
    ) -> R {
        if let Some(receiver) = receiver {
            cx.spawn(async move |entity, async_cx| {
                while let Ok(command) = receiver.recv().await {
                    if entity
                        .update(async_cx, |state, gpui_cx| {
                            let commands = state.host_commands();
                            let mut host_cx = HostCx::root(gpui_cx, commands);
                            match command {
                                HostCommand::Run(f) => f(state, &mut host_cx),
                                HostCommand::Emit(event) => host_cx.emit(*event),
                                HostCommand::Notify => host_cx.notify(),
                                HostCommand::Track(completed) => {
                                    gpui_cx
                                        .spawn(async move |_, _| {
                                            let _ = completed.recv().await;
                                        })
                                        .detach();
                                }
                            }
                            host_cx.flush(gpui_cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }

        let mut host_cx = Self::root(cx, commands);
        let result = f(state, &mut host_cx);
        host_cx.flush(cx);
        result
    }

    fn root(cx: &Context<AppState>, commands: async_channel::Sender<HostCommand>) -> Self {
        Self {
            executor: cx.background_executor().clone(),
            commands,
            immediate: Some(ImmediateEffects::default()),
        }
    }

    fn flush(mut self, cx: &mut Context<AppState>) {
        let immediate = self.immediate.take().expect("root HostCx has effects");
        for event in immediate.events {
            cx.emit(event);
        }
        if immediate.notify {
            cx.notify();
        }
    }
}
