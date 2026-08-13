use super::*;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use tcode_protocol::HostMessage;

use crate::host::HostMsg;

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
    outgoing_tx: smol::channel::Sender<HostMessage>,
    pub(super) outgoing_rx: smol::channel::Receiver<HostMessage>,
    pending: Arc<Mutex<HashMap<u64, smol::channel::Sender<HostMessage>>>>,
    changed_tx: smol::channel::Sender<()>,
    changed_rx: smol::channel::Receiver<()>,
    state: Option<Weak<RefCell<AppState>>>,
}

impl Default for TestAppContext {
    fn default() -> Self {
        let (mailbox_tx, mailbox_rx) = smol::channel::unbounded();
        let (outgoing_tx, outgoing_rx) = smol::channel::unbounded();
        let (changed_tx, changed_rx) = smol::channel::bounded(1);
        Self {
            mailbox_tx,
            mailbox_rx,
            outgoing_tx,
            outgoing_rx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            changed_tx,
            changed_rx,
            state: None,
        }
    }
}

impl TestAppContext {
    pub(super) fn new_entity(&mut self, build: impl FnOnce(&mut ()) -> AppState) -> TestEntity {
        let state = Rc::new(RefCell::new(build(&mut ())));
        self.state = Some(Rc::downgrade(&state));
        TestEntity(state)
    }

    pub(super) fn host_cx(&self) -> HostCx {
        HostCx::new(
            self.mailbox_tx.clone(),
            self.outgoing_tx.clone(),
            self.pending.clone(),
            self.changed_tx.clone(),
        )
    }

    pub(super) fn run_until_parked(&mut self) {
        let state = self
            .state
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("test state must outlive its context");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut idle_passes = 0_u32;

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
            while self.outgoing_rx.try_recv().is_ok() {
                had_work = true;
            }
            while self.changed_rx.try_recv().is_ok() {
                had_work = true;
            }

            if had_work {
                idle_passes = 0;
            } else {
                idle_passes += 1;
                if idle_passes >= 50 {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("test host failed to park within five seconds");
    }
}

pub(super) struct TestEntity(Rc<RefCell<AppState>>);

impl TestEntity {
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
