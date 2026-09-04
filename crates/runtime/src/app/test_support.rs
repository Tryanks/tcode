use super::*;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::{Rc, Weak};

use crate::host::HostMsg;
use tcode_protocol::{ClientMessage, ClientPayload, Command, HostMessage, decode_host_line};

pub(super) struct TestStore(SessionStore);

impl TestStore {
    pub(super) fn new(prefix: &str) -> Self {
        let root = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        Self(SessionStore::open_at(root).unwrap())
    }
}

impl Deref for TestStore {
    type Target = SessionStore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.root());
    }
}

/// Plain smol/mailbox replacement for the former gpui test context.
///
/// Tests still exercise the exact production [`HostCx`] seam: background
/// completions must re-enter through `HostMsg::Enqueued`, and
/// `run_until_parked` is the only code that mutates the owned `AppState`.
pub(super) struct TestAppContext {
    mailbox_tx: smol::channel::Sender<HostMsg>,
    mailbox_rx: smol::channel::Receiver<HostMsg>,
    outgoing_tx: smol::channel::Sender<String>,
    pub(super) outgoing_rx: smol::channel::Receiver<String>,
    outgoing: Vec<String>,
    domain_diff: Option<DomainDiff>,
    state: Option<Weak<RefCell<AppState>>>,
}

impl Default for TestAppContext {
    fn default() -> Self {
        let (mailbox_tx, mailbox_rx) = smol::channel::unbounded();
        let (outgoing_tx, outgoing_rx) = smol::channel::unbounded();
        Self {
            mailbox_tx,
            mailbox_rx,
            outgoing_tx,
            outgoing_rx,
            outgoing: Vec::new(),
            domain_diff: None,
            state: None,
        }
    }
}

impl TestAppContext {
    pub(super) fn new_entity(&mut self, build: impl FnOnce(&mut ()) -> AppState) -> TestEntity {
        let state = Rc::new(RefCell::new(build(&mut ())));
        self.domain_diff = Some(DomainDiff::new(&state.borrow()));
        self.state = Some(Rc::downgrade(&state));
        TestEntity(state)
    }

    pub(super) fn host_cx(&self) -> HostCx {
        HostCx::new(self.mailbox_tx.clone(), self.outgoing_tx.clone())
    }

    pub(super) fn run_until_parked(&mut self) {
        let state = self
            .state
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("test state must outlive its context");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut idle_passes = 0_u32;
        let mut saw_work = false;
        let mut store_flushed = false;

        while Instant::now() < deadline {
            let mut had_work = false;
            while let Ok(message) = self.mailbox_rx.try_recv() {
                had_work = true;
                match message {
                    HostMsg::Enqueued(completion) => {
                        let mut host_cx = self.host_cx();
                        let mut state = state.borrow_mut();
                        completion(&mut state, &mut host_cx);
                        state.sync_terminal_handles();
                    }
                }
            }
            while let Ok(line) = self.outgoing_rx.try_recv() {
                self.outgoing.push(line);
                had_work = true;
            }
            if had_work {
                idle_passes = 0;
                saw_work = true;
                store_flushed = false;
            } else {
                idle_passes += 1;
                // Loaded CI runners (notably Windows) can take well past 50ms
                // to schedule a background completion; calls that processed
                // work get a wider idle window before declaring parked.
                let threshold = if saw_work && !store_flushed { 250 } else { 50 };
                if idle_passes >= threshold {
                    if !store_flushed {
                        // Barrier the store-writer task so persisted state is
                        // visible before callers assert on it. Its failure
                        // path can enqueue more work, so keep draining.
                        store_flushed = true;
                        if self.flush_store_writer(&state, deadline) {
                            idle_passes = 0;
                            continue;
                        }
                    }
                    let mut host_cx = self.host_cx();
                    state.borrow().sync_terminal_handles();
                    self.domain_diff
                        .as_mut()
                        .expect("test domain diff must be initialized")
                        .emit_changes(&state.borrow(), &mut host_cx);
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("test host failed to park within five seconds");
    }

    /// Drain and decode every NDJSON line emitted by the host so tests assert
    /// on the same serialized traffic consumed by production clients.
    pub(super) fn drain_outgoing(&mut self) -> Vec<HostMessage> {
        while let Ok(line) = self.outgoing_rx.try_recv() {
            self.outgoing.push(line);
        }
        self.outgoing
            .drain(..)
            .map(|line| decode_host_line(&line).expect("decode outgoing host NDJSON"))
            .collect()
    }

    /// Push a `StoreWrite::Flush` barrier through the store-writer task and
    /// wait for its echo. Returns false when no writer is running.
    fn flush_store_writer(&self, state: &Rc<RefCell<AppState>>, deadline: Instant) -> bool {
        let (tx, rx) = smol::channel::bounded(1);
        {
            let state = state.borrow();
            // The writer task spawns lazily on the first enqueued write; a
            // still-present receiver means it never started — nothing to flush.
            if state.store_write_receiver.is_some()
                || state.store_writes.try_send(StoreWrite::Flush(tx)).is_err()
            {
                return false;
            }
        }
        while Instant::now() < deadline {
            if rx.try_recv().is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("store writer failed to flush within the parked deadline");
    }
}

pub(super) struct TestEntity(Rc<RefCell<AppState>>);

impl TestEntity {
    pub(super) fn dispatch_command(&self, cx: &mut TestAppContext, id: u64, command: Command) {
        let mut host_cx = cx.host_cx();
        crate::pipe::handle_client_message_for_test(
            &mut self.0.borrow_mut(),
            &mut host_cx,
            ClientMessage {
                id,
                payload: ClientPayload::Command(command),
            },
        );
    }

    pub(super) fn host_update<R>(
        &self,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut AppState, &mut HostCx) -> R,
    ) -> R {
        let mut host_cx = cx.host_cx();
        update(&mut self.0.borrow_mut(), &mut host_cx)
    }

    pub(super) fn update<R>(
        &self,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut AppState, &mut HostCx) -> R,
    ) -> R {
        self.host_update(cx, update)
    }

    pub(super) fn read_with<R>(
        &self,
        _cx: &TestAppContext,
        read: impl FnOnce(&AppState, &()) -> R,
    ) -> R {
        read(&self.0.borrow(), &())
    }
}
