use super::*;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::{Rc, Weak};

use crate::host::HostFn;
use tcode_protocol::{ClientMessage, ClientPayload, Command, HostMessage, decode_host_line};

/// What reaches the host from work running off it.
enum Arrival<T> {
    Message(HostFn),
    Line(String),
    Other(T),
}

/// The wall time a test's host clock starts at.
pub(super) fn test_start() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000)
}

/// [`test_start`] in unix milliseconds.
pub(super) fn test_start_millis() -> u64 {
    test_start()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A host clock for a test, whose wall starts at [`test_start`].
pub(super) fn test_clock() -> std::sync::Arc<crate::clock::ManualClock> {
    crate::clock::ManualClock::starting_at(test_start())
}

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
/// completions must re-enter through the host mailbox, and
/// `run_until_parked` is the only code that mutates the owned `AppState`.
pub(super) struct TestAppContext {
    mailbox_tx: smol::channel::Sender<HostFn>,
    mailbox_rx: smol::channel::Receiver<HostFn>,
    outgoing_tx: smol::channel::Sender<String>,
    pub(super) outgoing_rx: smol::channel::Receiver<String>,
    outgoing: Vec<String>,
    domain_diff: Option<DomainDiff>,
    state: Option<Weak<RefCell<TestClientState>>>,
    /// The host's time, which moves only when the test advances it.
    pub(super) clock: std::sync::Arc<crate::clock::ManualClock>,
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
            clock: test_clock(),
        }
    }
}

impl TestAppContext {
    /// Start the host this context drives. A host started again over the same
    /// store takes a context of its own: it is another process, and work the
    /// earlier host left running would otherwise land in it.
    pub(super) fn new_entity(&mut self, state: TestClientState) -> TestEntity {
        assert!(
            self.state.is_none(),
            "a TestAppContext drives one host; start another with a new context"
        );
        let state = Rc::new(RefCell::new(state));
        self.domain_diff = Some(DomainDiff::new(&state.borrow()));
        self.state = Some(Rc::downgrade(&state));
        TestEntity(state)
    }

    /// The host's wall time in unix milliseconds.
    pub(super) fn now_millis(&self) -> u64 {
        self.clock.host_clock().unix_millis()
    }

    /// The host's wall time in unix seconds.
    pub(super) fn now_secs(&self) -> u64 {
        self.clock.host_clock().unix_secs()
    }

    pub(super) fn host_cx(&self) -> HostCx {
        HostCx::new(
            self.mailbox_tx.clone(),
            self.outgoing_tx.clone(),
            self.clock.host_clock(),
        )
    }

    /// Run what is already queued for the host, and what that queues in turn,
    /// until the mailbox is empty. Work running off the host (thread-pool
    /// hops, timers) reports only when it finishes; a test that needs its
    /// result waits for that state with [`Self::run_until`].
    pub(super) fn run_until_parked(&mut self) {
        let state = self.state();
        let mut store_flushed = false;

        loop {
            let mut had_work = false;
            while let Ok(message) = self.mailbox_rx.try_recv() {
                had_work = true;
                let mut host_cx = self.host_cx();
                let mut state = state.borrow_mut();
                message(&mut state, &mut host_cx);
                self.after_input(&mut state);
            }
            while let Ok(line) = self.outgoing_rx.try_recv() {
                self.outgoing.push(line);
                had_work = true;
            }
            if had_work {
                store_flushed = false;
                continue;
            }
            if !store_flushed {
                // Barrier the store-writer task so persisted state is
                // visible before callers assert on it. Its failure path can
                // enqueue more work, so keep draining.
                store_flushed = true;
                if self.flush_store_writer(&state) {
                    continue;
                }
            }
            break;
        }
        // What a test changed through `TestEntity::update` reaches clients too.
        self.after_input(&mut state.borrow_mut());
    }

    fn after_input(&mut self, state: &mut TestClientState) {
        let mut host_cx = self.host_cx();
        crate::pipe::after_input(
            state,
            self.domain_diff
                .as_mut()
                .expect("test domain diff must be initialized"),
            &mut host_cx,
        );
    }

    /// Park repeatedly until `done` observes the state a background chain
    /// must reach, waiting between parks for that work to hand something
    /// back to the host.
    pub(super) fn run_until(&mut self, mut done: impl FnMut(&TestClientState) -> bool) {
        loop {
            self.run_until_parked();
            if done(&self.state().borrow()) {
                return;
            }
            self.wait_for_host(std::future::pending::<()>());
        }
    }

    /// Park repeatedly until `pick` finds what the test waits for among the
    /// messages the host sends; return it with every message drained on the
    /// way.
    pub(super) fn run_until_sent<T>(
        &mut self,
        mut pick: impl FnMut(&HostMessage) -> Option<T>,
    ) -> (T, Vec<HostMessage>) {
        let mut sent = Vec::new();
        loop {
            self.run_until_parked();
            let seen = sent.len();
            sent.extend(self.drain_outgoing());
            if let Some(found) = sent[seen..].iter().find_map(&mut pick) {
                return (found, sent);
            }
            self.wait_for_host(std::future::pending::<()>());
        }
    }

    /// Run until the provider behind `commands` is sent a command a test
    /// scripts and return it, answering usage reads on the way as
    /// [`next_command`] does.
    pub(super) fn run_until_command(
        &mut self,
        commands: &smol::channel::Receiver<SessionCommand>,
    ) -> SessionCommand {
        loop {
            match self.run_until_recv(commands) {
                SessionCommand::ReadAccountUsage { reply } => answer_usage_read(reply),
                command => return command,
            }
        }
    }

    /// Run the host until `receiver` hears something, and return it with
    /// the host parked again: work off the host (a provider's launch, a
    /// control call's reply) sends on such channels directly.
    pub(super) fn run_until_recv<T>(&mut self, receiver: &smol::channel::Receiver<T>) -> T {
        loop {
            self.run_until_parked();
            if let Ok(value) = receiver.try_recv() {
                return value;
            }
            match self.wait_for_host(receiver.recv()) {
                None => {}
                Some(Ok(value)) => {
                    self.run_until_parked();
                    return value;
                }
                Some(Err(_)) => panic!("the channel the test waits on closed"),
            }
        }
    }

    /// Wait for background work to queue a message on the mailbox and run
    /// that one message alone, keeping what it emitted for
    /// [`TestAppContext::drain_outgoing`].
    pub(super) fn run_next(&mut self) {
        let message = self
            .mailbox_rx
            .recv_blocking()
            .expect("the context holds the mailbox");
        self.run_message(message);
    }

    fn state(&self) -> Rc<RefCell<TestClientState>> {
        self.state
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("test state must outlive its context")
    }

    fn run_message(&mut self, message: HostFn) {
        let state = self.state();
        let mut host_cx = self.host_cx();
        message(&mut state.borrow_mut(), &mut host_cx);
        self.after_input(&mut state.borrow_mut());
        while let Ok(line) = self.outgoing_rx.try_recv() {
            self.outgoing.push(line);
        }
    }

    /// Block until work running off the host hands it a message or sends a
    /// line, and take that in; or until `other` is ready, and return it.
    /// This context holds the mailbox's and the output's senders, so a chain
    /// that never arrives is ended by the test runner's timeout.
    fn wait_for_host<T>(&mut self, other: impl std::future::Future<Output = T>) -> Option<T> {
        let mailbox = self.mailbox_rx.clone();
        let outgoing = self.outgoing_rx.clone();
        let arrival = smol::block_on(smol::future::or(
            smol::future::or(
                async {
                    Arrival::Message(mailbox.recv().await.expect("the context holds the mailbox"))
                },
                async {
                    Arrival::Line(outgoing.recv().await.expect("the context holds the output"))
                },
            ),
            async { Arrival::Other(other.await) },
        ));
        match arrival {
            Arrival::Message(message) => {
                self.run_message(message);
                None
            }
            Arrival::Line(line) => {
                self.outgoing.push(line);
                None
            }
            Arrival::Other(value) => Some(value),
        }
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
    fn flush_store_writer(&self, state: &Rc<RefCell<TestClientState>>) -> bool {
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
        // A failed write answers the barrier too; tests that make writes
        // fail read the failure from the host.
        let _answered = rx
            .recv_blocking()
            .expect("the store writer ended before it flushed");
        true
    }
}

pub(super) struct TestEntity(Rc<RefCell<TestClientState>>);

impl TestEntity {
    pub(super) fn dispatch_command(&self, cx: &mut TestAppContext, id: u64, command: Command) {
        self.deliver(cx, id, ClientPayload::Command(command));
    }

    pub(super) fn deliver(&self, cx: &mut TestAppContext, id: u64, payload: ClientPayload) {
        let mut host_cx = cx.host_cx();
        let mut state = self.0.borrow_mut();
        crate::pipe::handle_client_message(
            &mut state,
            &mut host_cx,
            ClientMessage {
                principal: None,
                key: None,
                id,
                payload,
            },
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        );
        cx.after_input(&mut state);
    }

    pub(super) fn update<R>(
        &self,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut TestClientState, &mut HostCx) -> R,
    ) -> R {
        let mut host_cx = cx.host_cx();
        update(&mut self.0.borrow_mut(), &mut host_cx)
    }

    pub(super) fn read<R>(&self, read: impl FnOnce(&TestClientState) -> R) -> R {
        read(&self.0.borrow())
    }

    /// Open the parked thread `id` again, as a client selecting it does, and
    /// run until the timeline that reopening rebuilds from its log off the
    /// host has landed. The parked timeline is emptied first so the rebuild
    /// can be seen landing; the rebuild replaces it whole, so this changes
    /// nothing it holds.
    pub(super) fn reopen(&self, cx: &mut TestAppContext, id: &str) {
        self.update(cx, |state, cx| {
            state
                .host
                .residents
                .parked
                .get_mut(id)
                .expect("the thread is parked")
                .timeline = Timeline::default();
            state.select_session(id, cx);
        });
        cx.run_until(|state| {
            state
                .resident(id)
                .is_some_and(|thread| !thread.timeline.entries.is_empty())
        });
    }
}

/// Single-client fixture: selection belongs to this test client, never AppState.
/// Existing lifecycle scenarios use this facade to adopt and park their targets;
/// every forwarded runtime operation captures its explicit session id.
pub(super) struct TestClientState {
    pub(super) host: AppState,
    pub(super) selected: Option<String>,
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
            host: AppState::new(store).unwrap(),
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
        if let Ok(id) = self.host.fork_thread(id, cx) {
            self.park_active(cx);
            self.selected = Some(id);
        }
    }
    pub(super) fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut HostCx) {
        self.park_active(cx);
        self.selected = Some(self.host.start_draft(project_id, cwd, cx));
    }
}

/// One JSON-RPC exchange with an in-process MCP server, as a provider makes it: the reply
/// and the session id the server assigned.
pub(super) fn rpc(
    url: &str,
    token: &str,
    session: Option<&str>,
    body: serde_json::Value,
) -> (serde_json::Value, Option<String>) {
    let mut request = ureq::post(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json, text/event-stream")
        .set("Content-Type", "application/json");
    if let Some(session) = session {
        request = request
            .set("Mcp-Session-Id", session)
            .set("MCP-Protocol-Version", "2025-11-25");
    }
    let response = request
        .send_bytes(&serde_json::to_vec(&body).unwrap())
        .unwrap();
    let session = response.header("Mcp-Session-Id").map(str::to_owned);
    let text = response.into_string().unwrap();
    let json = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|data| !data.trim().is_empty())
        .unwrap_or(&text);
    (
        if json.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(json).unwrap()
        },
        session,
    )
}

/// The next command a scripted provider receives that a test scripts: the
/// usage reads a finished turn asks of a running harness are answered on the
/// way, as a harness answers them without the conversation moving.
pub(crate) fn next_command(
    commands: &smol::channel::Receiver<SessionCommand>,
) -> Result<SessionCommand, smol::channel::TryRecvError> {
    loop {
        match commands.try_recv() {
            Ok(SessionCommand::ReadAccountUsage { reply }) => answer_usage_read(reply),
            other => return other,
        }
    }
}

/// [`next_command`], waiting for it.
pub(crate) fn next_command_blocking(
    commands: &smol::channel::Receiver<SessionCommand>,
) -> Result<SessionCommand, smol::channel::RecvError> {
    loop {
        match commands.recv_blocking() {
            Ok(SessionCommand::ReadAccountUsage { reply }) => answer_usage_read(reply),
            other => return other,
        }
    }
}

fn answer_usage_read(reply: smol::channel::Sender<Result<serde_json::Value, String>>) {
    let _ = reply.try_send(Err("the scripted provider has no account".into()));
}
