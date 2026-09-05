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
    state: Option<Weak<RefCell<TestClientState>>>,
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
    pub(super) fn new_entity(
        &mut self,
        build: impl FnOnce(&mut ()) -> TestClientState,
    ) -> TestEntity {
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
    fn flush_store_writer(&self, state: &Rc<RefCell<TestClientState>>, deadline: Instant) -> bool {
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

pub(super) struct TestEntity(Rc<RefCell<TestClientState>>);

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
        update: impl FnOnce(&mut TestClientState, &mut HostCx) -> R,
    ) -> R {
        let mut host_cx = cx.host_cx();
        update(&mut self.0.borrow_mut(), &mut host_cx)
    }

    pub(super) fn update<R>(
        &self,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut TestClientState, &mut HostCx) -> R,
    ) -> R {
        self.host_update(cx, update)
    }

    pub(super) fn read_with<R>(
        &self,
        _cx: &TestAppContext,
        read: impl FnOnce(&TestClientState, &()) -> R,
    ) -> R {
        read(&self.0.borrow(), &())
    }
}

/// Single-client fixture: selection belongs to this test client, never AppState.
/// Existing lifecycle scenarios use this facade to adopt and park their targets;
/// every forwarded runtime operation captures its explicit session id.
pub(super) struct TestClientState {
    host: AppState,
    selected: Option<String>,
}
impl std::ops::Deref for TestClientState {
    type Target = AppState;
    fn deref(&self) -> &AppState {
        &self.host
    }
}
impl std::ops::DerefMut for TestClientState {
    fn deref_mut(&mut self) -> &mut AppState {
        &mut self.host
    }
}
impl TestClientState {
    pub(super) fn new(store: SessionStore) -> Self {
        Self {
            host: AppState::new(store),
            selected: None,
        }
    }
    pub(super) fn selected_session(&self) -> Option<&ActiveSession> {
        self.host.residents.live.get(self.selected.as_deref()?)
    }
    pub(super) fn selected_session_mut(&mut self) -> Option<&mut ActiveSession> {
        self.host.residents.live.get_mut(self.selected.as_deref()?)
    }
    pub(super) fn active_session_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    pub(super) fn install_selected(&mut self, session: ActiveSession) {
        let _ = self.take_selected();
        self.selected = Some(session.meta.id.clone());
        self.host
            .residents
            .live
            .insert(session.meta.id.clone(), session);
    }
    pub(super) fn take_selected(&mut self) -> Option<ActiveSession> {
        self.host.residents.live.remove(&self.selected.take()?)
    }
    pub(super) fn park_active(&mut self, cx: &mut HostCx) {
        if let Some(id) = self.selected.take() {
            self.host.park_active(&id, cx);
        }
    }
    pub(super) fn shutdown_active(&mut self, cx: &mut HostCx) {
        if let Some(id) = self.selected.take() {
            self.host.shutdown_active(&id, cx);
        }
    }
    pub(super) fn select_session(&mut self, id: &str, cx: &mut HostCx) {
        if self.selected.as_deref() == Some(id) {
            return;
        }
        self.park_active(cx);
        self.host.select_session(id, cx);
        if self.host.resident(id).is_some() {
            self.selected = Some(id.to_string());
        }
    }
    pub(super) fn fork_thread(&mut self, id: &str, cx: &mut HostCx) {
        if let Some(id) = self.host.fork_thread(id, cx) {
            self.park_active(cx);
            self.selected = Some(id);
        }
    }
    pub(super) fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut HostCx) {
        self.park_active(cx);
        self.selected = Some(self.host.start_draft(project_id, cwd, cx));
    }
    pub(super) fn add_review_comment(&mut self, comment: ReviewComment, _cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.add_review_comment(&target_id, comment, _cx)
    }
    pub(super) fn commit_draft(&mut self, cx: &mut HostCx) -> std::io::Result<()> {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.commit_draft(&target_id, cx)
    }
    pub(super) fn dispatch_next_queued(&mut self, _cx: &mut HostCx) -> Result<bool, ()> {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.dispatch_next_queued(&target_id, _cx)
    }
    pub(super) fn drop_queued(&mut self, id: u64, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.drop_queued(&target_id, id, cx)
    }
    pub(super) fn implement_plan(&mut self, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.implement_plan(&target_id, cx)
    }
    pub(super) fn interrupt(&mut self, _cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.interrupt(&target_id, _cx)
    }
    pub(super) fn native_rewind_pending(&self) -> bool {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.native_rewind_pending(&target_id)
    }
    pub(super) fn open_terminal_panel(&mut self, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.open_terminal_panel(&target_id, cx)
    }
    pub(super) fn orchestrate_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host
            .orchestrate_turn(&target_id, text, attachment_paths, cx)
    }
    pub(super) fn relay_confirmation(&self) -> Option<(String, String)> {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.relay_confirmation(&target_id)
    }
    pub(super) fn review_comments(&self) -> &[ReviewComment] {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.review_comments(&target_id)
    }
    pub(super) fn rewind_turn(&mut self, turn: usize, mode: RewindMode, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.rewind_turn(&target_id, turn, mode, cx)
    }
    pub(super) fn save_plan_to_workspace(&mut self, markdown: String, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.save_plan_to_workspace(&target_id, markdown, cx)
    }
    pub(super) fn schedule_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        fire_at_unix_secs: u64,
        cx: &mut HostCx,
    ) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host
            .schedule_turn(&target_id, text, attachment_paths, fire_at_unix_secs, cx)
    }
    pub(super) fn send_turn(
        &mut self,
        text: String,
        attachment_paths: Vec<PathBuf>,
        cx: &mut HostCx,
    ) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.send_turn(&target_id, text, attachment_paths, cx)
    }
    pub(super) fn set_active_model(
        &mut self,
        provider: ProviderKind,
        model: Option<String>,

        profile_id: Option<String>,
        cx: &mut HostCx,
    ) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host
            .set_active_model(&target_id, provider, model, profile_id, cx)
    }
    pub(super) fn steer(&mut self, text: String, attachment_paths: Vec<PathBuf>, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.steer(&target_id, text, attachment_paths, cx)
    }
    pub(super) fn steer_queued(&mut self, id: u64, cx: &mut HostCx) {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.steer_queued(&target_id, id, cx)
    }
    pub(super) fn terminal_panel_open(&self) -> bool {
        let target_id = self.selected.clone().unwrap_or_default();
        self.host.terminal_panel_open(&target_id)
    }
}
