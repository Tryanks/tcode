use super::*;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::{Rc, Weak};

use crate::host::HostFn;
use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, HostMessage, Principal, Query,
    Subscription, decode_host_line,
};

/// What a client attached to the test host has been sent on the topics it
/// holds, kept as a client keeps it: the latest status of each thread, the
/// records of each thread's log, the index, and the answers to its requests.
#[derive(Default)]
pub(super) struct ClientView {
    statuses: HashMap<String, SessionStatus>,
    records: HashMap<String, Vec<StoredEvent>>,
    sessions: HashMap<String, SessionMeta>,
    activity: HashMap<String, SessionActivity>,
    title_generating: HashSet<String>,
    worktree_shared: HashSet<String>,
    acks: HashMap<u64, Result<CommandResponse, ProtocolError>>,
    answers: HashMap<u64, Result<QueryResponse, ProtocolError>>,
    notifications: Vec<RuntimeEvent>,
    providers: Option<ProvidersStatus>,
}

impl ClientView {
    fn apply(&mut self, message: HostMessage) {
        let envelope = match message {
            HostMessage::Ack { id, result } => {
                self.acks.insert(id, result);
                return;
            }
            HostMessage::QueryResult { id, result } => {
                self.answers.insert(id, result);
                return;
            }
            HostMessage::Event(envelope) => envelope,
        };
        match (envelope.topic, envelope.event) {
            (Topic::SessionStatus { session_id }, ServerEvent::SessionStatusReplaced(status)) => {
                self.statuses.insert(session_id, *status);
            }
            (
                Topic::SessionStatus { session_id },
                ServerEvent::SessionUsageChanged {
                    usage,
                    context_window,
                },
            ) => {
                if let Some(status) = self.statuses.get_mut(&session_id) {
                    status.usage = usage;
                    status.context_window = context_window;
                }
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionSnapshot { records, .. }) => {
                self.records.insert(session_id, records);
            }
            (Topic::SessionEvents { session_id }, ServerEvent::SessionEvent(record)) => {
                self.records.entry(session_id).or_default().push(record);
            }
            (Topic::Index, ServerEvent::IndexSnapshot(snapshot)) => {
                self.sessions = snapshot
                    .sessions
                    .into_iter()
                    .map(|meta| (meta.id.clone(), meta))
                    .collect();
                self.activity = snapshot.summary.activity;
                self.title_generating = snapshot.summary.title_generating;
                self.worktree_shared = snapshot.summary.worktree_shared;
            }
            (Topic::Index, ServerEvent::IndexUpsertSession(meta)) => {
                self.sessions.insert(meta.id.clone(), *meta);
            }
            (Topic::Index, ServerEvent::IndexRemoveSession { session_id }) => {
                self.sessions.remove(&session_id);
            }
            (Topic::Index, ServerEvent::IndexSummaryReplaced(summary)) => {
                self.activity = summary.activity;
                self.title_generating = summary.title_generating;
                self.worktree_shared = summary.worktree_shared;
            }
            (Topic::Index, ServerEvent::IndexActivitiesChanged { upserts, removed }) => {
                self.activity.extend(upserts);
                for id in removed {
                    self.activity.remove(&id);
                }
            }
            (Topic::Providers, ServerEvent::ProvidersReplaced(providers)) => {
                self.providers = Some(providers);
            }
            (
                Topic::Providers,
                ServerEvent::ProviderUsageChanged {
                    provider_usage,
                    usage_checking,
                },
            ) => {
                if let Some(providers) = &mut self.providers {
                    providers.provider_usage = provider_usage;
                    providers.usage_checking = usage_checking;
                }
            }
            (_, ServerEvent::Runtime(notification)) => self.notifications.push(notification),
            _ => {}
        }
    }

    /// The thread's status as its status topic last gave it.
    pub(super) fn status(&self, session_id: &str) -> &SessionStatus {
        self.statuses
            .get(session_id)
            .unwrap_or_else(|| panic!("no status of {session_id} reached the client"))
    }

    pub(super) fn has_status(&self, session_id: &str) -> bool {
        self.statuses.contains_key(session_id)
    }

    /// The thread's log as its events topic gave it.
    pub(super) fn records(&self, session_id: &str) -> &[StoredEvent] {
        self.records.get(session_id).map_or(&[], Vec::as_slice)
    }

    /// The thread's records as a client folds them.
    pub(super) fn timeline(&self, session_id: &str) -> Timeline {
        Timeline::fold_stored(self.records(session_id))
    }

    /// A thread of the index.
    pub(super) fn session(&self, session_id: &str) -> Option<&SessionMeta> {
        self.sessions.get(session_id)
    }

    /// The index's threads.
    pub(super) fn sessions(&self) -> impl Iterator<Item = &SessionMeta> {
        self.sessions.values()
    }

    /// The index's activity of a thread.
    pub(super) fn activity(&self, session_id: &str) -> Option<&SessionActivity> {
        self.activity.get(session_id)
    }

    /// Whether the index says a title is being made for the thread.
    pub(super) fn title_generating(&self, session_id: &str) -> bool {
        self.title_generating.contains(session_id)
    }

    /// Whether the index says another thread works in the thread's worktree.
    pub(super) fn worktree_shared(&self, session_id: &str) -> bool {
        self.worktree_shared.contains(session_id)
    }

    /// The thread of the index that mirrors the harness's own subagent `native`.
    pub(super) fn mirror(&self, native: &str) -> Option<&SessionMeta> {
        self.sessions
            .values()
            .find(|meta| meta.native_subagent.as_deref() == Some(native))
    }

    /// The providers topic as it last stood.
    pub(super) fn providers(&self) -> &ProvidersStatus {
        self.providers
            .as_ref()
            .expect("the client holds the providers topic")
    }

    /// Every runtime notification the client was sent.
    pub(super) fn notifications(&self) -> &[RuntimeEvent] {
        &self.notifications
    }
}

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
    /// Everything the host sent, as the client attached to it keeps it.
    view: ClientView,
    /// The id of the client's next request.
    next_request: u64,
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
            view: ClientView::default(),
            // Clear of the ids tests number by hand.
            next_request: 1 << 32,
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
                self.take(line);
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
            self.take(line);
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
                self.take(line);
                None
            }
            Arrival::Other(value) => Some(value),
        }
    }

    /// Drain and decode every NDJSON line emitted by the host so tests assert
    /// on the same serialized traffic consumed by production clients.
    pub(super) fn drain_outgoing(&mut self) -> Vec<HostMessage> {
        while let Ok(line) = self.outgoing_rx.try_recv() {
            self.take(line);
        }
        self.outgoing
            .drain(..)
            .map(|line| decode_host_line(&line).expect("decode outgoing host NDJSON"))
            .collect()
    }

    /// Take in a line the host sent: the client attached to it keeps what it
    /// says, and the test can still drain it.
    fn take(&mut self, line: String) {
        self.view
            .apply(decode_host_line(&line).expect("decode outgoing host NDJSON"));
        self.outgoing.push(line);
    }

    /// What the client attached to the host has been sent so far.
    pub(super) fn seen(&self) -> &ClientView {
        &self.view
    }

    /// Run the host until what the client has been sent satisfies `done`,
    /// waiting between parks for work off the host to hand something back.
    pub(super) fn run_until_seen(&mut self, mut done: impl FnMut(&ClientView) -> bool) {
        loop {
            self.run_until_parked();
            if done(&self.view) {
                return;
            }
            self.wait_for_host(std::future::pending::<()>());
        }
    }

    fn request_id(&mut self) -> u64 {
        self.next_request += 1;
        self.next_request
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
        self.deliver_as(cx, id, None, None, payload);
    }

    /// [`Self::deliver`] from a client whose device is admitted as
    /// `principal`, as the mux stamps its messages.
    fn deliver_as(
        &self,
        cx: &mut TestAppContext,
        id: u64,
        principal: Option<Principal>,
        key: Option<String>,
        payload: ClientPayload,
    ) {
        let mut host_cx = cx.host_cx();
        let mut state = self.0.borrow_mut();
        crate::pipe::handle_client_message(
            &mut state,
            &mut host_cx,
            ClientMessage {
                principal,
                key,
                id,
                payload,
            },
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        );
        cx.after_input(&mut state);
    }

    /// [`Self::command`] under the idempotency `key` a client gives a request
    /// it may send again.
    pub(super) fn command_keyed(
        &self,
        cx: &mut TestAppContext,
        key: &str,
        command: Command,
    ) -> Result<CommandResponse, ProtocolError> {
        let id = cx.request_id();
        self.deliver_as(
            cx,
            id,
            None,
            Some(key.into()),
            ClientPayload::Command(command),
        );
        cx.run_until_seen(|view| view.acks.contains_key(&id));
        cx.view.acks.remove(&id).unwrap()
    }

    /// [`Self::command`] from a member device admitted as `principal`.
    pub(super) fn command_as(
        &self,
        cx: &mut TestAppContext,
        principal: Principal,
        command: Command,
    ) -> Result<CommandResponse, ProtocolError> {
        let id = cx.request_id();
        self.deliver_as(
            cx,
            id,
            Some(principal),
            None,
            ClientPayload::Command(command),
        );
        cx.run_until_seen(|view| view.acks.contains_key(&id));
        cx.view.acks.remove(&id).unwrap()
    }

    /// [`Self::query`] from a member device admitted as `principal`.
    pub(super) fn query_as(
        &self,
        cx: &mut TestAppContext,
        principal: Principal,
        query: Query,
    ) -> Result<QueryResponse, ProtocolError> {
        let id = cx.request_id();
        self.deliver_as(cx, id, Some(principal), None, ClientPayload::Query(query));
        cx.run_until_seen(|view| view.answers.contains_key(&id));
        cx.view.answers.remove(&id).unwrap()
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

    /// Send `command` as the client does, and run the host until it answers.
    pub(super) fn command(
        &self,
        cx: &mut TestAppContext,
        command: Command,
    ) -> Result<CommandResponse, ProtocolError> {
        let id = cx.request_id();
        self.deliver(cx, id, ClientPayload::Command(command));
        cx.run_until_seen(|view| view.acks.contains_key(&id));
        cx.view.acks.remove(&id).unwrap()
    }

    /// Ask `query` as the client does, and run the host until it answers.
    pub(super) fn query(
        &self,
        cx: &mut TestAppContext,
        query: Query,
    ) -> Result<QueryResponse, ProtocolError> {
        let id = cx.request_id();
        self.deliver(cx, id, ClientPayload::Query(query));
        cx.run_until_seen(|view| view.answers.contains_key(&id));
        cx.view.answers.remove(&id).unwrap()
    }

    /// Hold `topic`, as a client showing what it carries does.
    pub(super) fn subscribe(&self, cx: &mut TestAppContext, topic: Topic) {
        let id = cx.request_id();
        self.deliver(
            cx,
            id,
            ClientPayload::Subscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            }),
        );
        cx.run_until_parked();
    }

    /// Let `topic` go.
    pub(super) fn unsubscribe(&self, cx: &mut TestAppContext, topic: Topic) {
        let id = cx.request_id();
        self.deliver(
            cx,
            id,
            ClientPayload::Unsubscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            }),
        );
        cx.run_until_parked();
    }

    /// Open thread `id` as a client showing it does, holding its status and
    /// its log, and run until both have reached the client.
    pub(super) fn open(&self, cx: &mut TestAppContext, id: &str) {
        self.subscribe(
            cx,
            Topic::SessionStatus {
                session_id: id.into(),
            },
        );
        self.subscribe(
            cx,
            Topic::SessionEvents {
                session_id: id.into(),
            },
        );
        cx.run_until_seen(|view| view.has_status(id) && view.records.contains_key(id));
    }

    /// Leave thread `id`, as a client that shows another does.
    pub(super) fn leave(&self, cx: &mut TestAppContext, id: &str) {
        for topic in [
            Topic::SessionStatus {
                session_id: id.into(),
            },
            Topic::SessionEvents {
                session_id: id.into(),
            },
        ] {
            self.unsubscribe(cx, topic);
        }
    }

    /// The user sends `text` to thread `id`.
    pub(super) fn send(&self, cx: &mut TestAppContext, id: &str, text: &str) {
        self.command(
            cx,
            Command::SendTurn {
                session_id: id.into(),
                text: text.into(),
                attachment_paths: Vec::new(),
            },
        )
        .expect("the host takes the message");
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

/// What holds a start back until it goes.
type Gate = smol::channel::Sender<()>;

/// Every provider the host starts, each played by the test on channels of
/// its own, in the order the host started them.
#[derive(Clone)]
pub(super) struct Providers {
    started: Arc<std::sync::Mutex<Vec<Played>>>,
    /// Starts in these directories fail with what the adapter says.
    refused: Arc<std::sync::Mutex<Vec<(PathBuf, String)>>>,
    /// While held, each start goes on until it is let go: the gate of every
    /// start, by its number, open once its sender is gone.
    held: Arc<std::sync::Mutex<Option<Vec<Option<Gate>>>>>,
    /// Told of each start, which happens off the host.
    starts: smol::channel::Receiver<()>,
    start: smol::channel::Sender<()>,
}

impl Default for Providers {
    fn default() -> Self {
        let (start, starts) = smol::channel::unbounded();
        Self {
            started: Default::default(),
            refused: Default::default(),
            held: Default::default(),
            starts,
            start,
        }
    }
}

/// One provider the host started: the test plays its adapter's half of the
/// canonical stream.
#[derive(Clone)]
pub(super) struct Played {
    pub(super) provider: ProviderKind,
    pub(super) options: SessionOptions,
    pub(super) commands: smol::channel::Receiver<SessionCommand>,
    pub(super) events: smol::channel::Sender<AgentEvent>,
}

impl Providers {
    pub(super) fn launcher(&self) -> ProviderLauncher {
        let (started, start) = (self.started.clone(), self.start.clone());
        let (refused, held) = (self.refused.clone(), self.held.clone());
        ProviderLauncher(Arc::new(move |provider, options: SessionOptions| {
            let hold = held.lock().unwrap().as_mut().map(|gates| {
                let (gate, hold) = smol::channel::bounded::<()>(1);
                gates.resize(started.lock().unwrap().len(), None);
                gates.push(Some(gate));
                hold
            });
            let refusal = refused
                .lock()
                .unwrap()
                .iter()
                .find(|(cwd, _)| *cwd == options.cwd)
                .map(|(_, error)| error.clone());
            if let Some(error) = refusal {
                return Box::pin(async move { Err(AgentError::Spawn(error)) });
            }
            let (commands, command_rx) = smol::channel::unbounded();
            let (event_tx, events) = smol::channel::unbounded();
            started.lock().unwrap().push(Played {
                provider,
                options,
                commands: command_rx,
                events: event_tx,
            });
            let _ = start.try_send(());
            Box::pin(async move {
                if let Some(hold) = hold {
                    let _ = hold.recv().await;
                }
                Ok(SessionHandle {
                    provider,
                    commands,
                    events,
                })
            })
        }))
    }

    /// The providers the host starts from now on are still starting, as a
    /// harness is for the seconds it takes, until [`Self::let_go`].
    pub(super) fn hold(&self) {
        *self.held.lock().unwrap() = Some(Vec::new());
    }

    /// The starts held go on, and the ones after them are not held.
    pub(super) fn let_go(&self) {
        self.held.lock().unwrap().take();
    }

    /// The start the host made `n`th, held, goes on.
    pub(super) fn let_go_of(&self, n: usize) {
        if let Some(gate) = self
            .held
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|gates| gates.get_mut(n))
        {
            gate.take();
        }
    }

    /// A provider started in `cwd` from now on fails to start, its adapter
    /// saying `error`.
    pub(super) fn refuse_in(&self, cwd: &Path, error: String) {
        self.refused
            .lock()
            .unwrap()
            .push((cwd.to_path_buf(), error));
    }

    /// How many providers the host has started.
    pub(super) fn count(&self) -> usize {
        self.started.lock().unwrap().len()
    }

    /// The provider the host started `n`th, from 0, once it has.
    pub(super) fn started(&self, cx: &mut TestAppContext, n: usize) -> Played {
        while self.count() <= n {
            cx.run_until_recv(&self.starts);
        }
        self.started.lock().unwrap()[n].clone()
    }

    /// The provider the host started in `cwd`, once it has: the one a thread
    /// working there runs.
    pub(super) fn started_in(&self, cx: &mut TestAppContext, cwd: &Path) -> Played {
        loop {
            if let Some(played) = self
                .started
                .lock()
                .unwrap()
                .iter()
                .find(|played| played.options.cwd == cwd)
            {
                return played.clone();
            }
            cx.run_until_recv(&self.starts);
        }
    }
}

impl Played {
    /// The next command the host sends the provider, with the usage reads a
    /// harness answers on its own answered on the way.
    pub(super) fn next_command(&self, cx: &mut TestAppContext) -> SessionCommand {
        cx.run_until_command(&self.commands)
    }

    /// The next message the host steers into the running turn.
    pub(super) fn next_steer(&self, cx: &mut TestAppContext) -> String {
        match self.next_command(cx) {
            SessionCommand::Steer { text, .. } => text,
            other => panic!("expected a steer, got {other:?}"),
        }
    }

    /// The next turn the host sends: its delivery id and text.
    pub(super) fn next_turn(&self, cx: &mut TestAppContext) -> (u64, String) {
        match self.next_command(cx) {
            SessionCommand::SendTurn {
                delivery_id, text, ..
            } => (delivery_id, text),
            other => panic!("expected a turn, got {other:?}"),
        }
    }

    /// The provider says `events` happened, in order. They reach the host off
    /// it; a test waits for what the last of them changes.
    pub(super) fn play(&self, events: impl IntoIterator<Item = AgentEvent>) {
        for event in events {
            self.events
                .try_send(event)
                .expect("the host holds the provider's events");
        }
    }

    /// The provider's event stream ends, as an adapter's does when its
    /// process is gone.
    pub(super) fn end_stream(&self) {
        self.events.close();
    }

    /// Run until the host has taken every event played so far: the provider
    /// lists its commands after them, which nothing else of the thread
    /// reads, and the client showing thread `id`, the thread this provider
    /// runs, is sent that list last.
    pub(super) fn fence(&self, cx: &mut TestAppContext, id: &str) {
        static FENCES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mark = format!(
            "fence-{}",
            FENCES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.play([AgentEvent::ProviderCommands {
            commands: vec![agent::ProviderCommand {
                name: mark.clone(),
                description: None,
                kind: agent::ProviderCommandKind::Command,
            }],
        }]);
        cx.run_until_seen(|view| {
            view.status(id)
                .provider_commands
                .iter()
                .any(|command| command.name == mark)
        });
    }

    /// The provider takes the next turn the host sends and starts it as
    /// `turn_id`, as an adapter does once the message is written; returns
    /// the turn's delivery id.
    pub(super) fn start_turn(&self, cx: &mut TestAppContext, turn_id: &str) -> u64 {
        let (delivery_id, _) = self.next_turn(cx);
        self.play([
            AgentEvent::TurnAccepted { delivery_id },
            AgentEvent::TurnStarted {
                turn_id: turn_id.into(),
            },
        ]);
        delivery_id
    }
}

impl Played {
    /// The model calls `tcode_control`'s `tool` with `arguments`, as its
    /// harness does over the MCP server the host gave it at launch: the
    /// tool's answer, or the error it gave.
    pub(super) fn control(
        &self,
        cx: &mut TestAppContext,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let registration = self
            .options
            .mcp_servers
            .iter()
            .find(|server| server.name == "tcode_control")
            .expect("the provider was given tcode_control")
            .clone();
        let (answer, answered) = smol::channel::bounded(1);
        let tool = tool.to_owned();
        std::thread::spawn(move || {
            let (url, token) = (&registration.url, &registration.bearer_token);
            let (_, session) = rpc(
                url,
                token,
                None,
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"played-provider","version":"1"}}}),
            );
            rpc(
                url,
                token,
                session.as_deref(),
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            );
            let (reply, _) = rpc(
                url,
                token,
                session.as_deref(),
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
            );
            let _ = answer.send_blocking(reply);
        });
        let reply = cx.run_until_recv(&answered);
        if let Some(error) = reply.get("error") {
            return Err(error["message"].as_str().unwrap_or_default().to_owned());
        }
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if reply["result"]["isError"] == true {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
        }
    }
}

/// [`played_host`], offering its providers `tcode_control` on loopback as a
/// composition root does.
pub(super) fn controlled_host(
    cx: &mut TestAppContext,
    store: &SessionStore,
) -> (TestEntity, Providers) {
    let mut server = mcp_host::Host::bind().unwrap();
    let control = control_mcp::start(&mut server);
    server.start().unwrap();
    let providers = Providers::default();
    let mut state = TestClientState::new(store.clone());
    state.set_provider_launcher(providers.launcher());
    state.attach_control_mcp(control);
    let state = cx.new_entity(state);
    state.update(cx, |state, cx| {
        state.pump_control_requests(cx);
        state.resume_undelivered_reports(cx);
        state.start_activity(cx);
    });
    (state, providers)
}

/// The user sends thread `id`, shown on the client, its first message, and
/// the provider the host starts `n`th takes it and starts a turn.
pub(super) fn first_turn(
    state: &TestEntity,
    cx: &mut TestAppContext,
    providers: &Providers,
    id: &str,
    n: usize,
) -> Played {
    state.open(cx, id);
    state.send(cx, id, "Go");
    let provider = providers.started(cx, n);
    provider.start_turn(cx, "turn");
    cx.run_until_seen(|view| view.status(id).running_turn.is_some());
    provider
}

/// A host over `store` whose providers the test plays.
pub(super) fn played_host(
    cx: &mut TestAppContext,
    store: &SessionStore,
) -> (TestEntity, Providers) {
    let providers = Providers::default();
    let mut state = TestClientState::new(store.clone());
    state.set_provider_launcher(providers.launcher());
    let state = cx.new_entity(state);
    // What a host does as it starts over a store an earlier one left.
    state.update(cx, |state, cx| {
        state.resume_undelivered_reports(cx);
        state.start_activity(cx);
    });
    (state, providers)
}

/// A thread of `provider` working in `cwd`, as a host that ran it before left
/// it on disk.
pub(super) fn stored_thread(
    store: &SessionStore,
    id: &str,
    provider: ProviderKind,
    cwd: &Path,
    edit: impl FnOnce(&mut SessionMeta),
) -> SessionMeta {
    std::fs::create_dir_all(cwd).unwrap();
    let mut meta = SessionMeta::new(provider, cwd.to_path_buf(), None);
    meta.id = id.into();
    edit(&mut meta);
    store.upsert_meta(&meta).unwrap();
    meta
}

/// One JSON-RPC exchange with an in-process MCP server, as a provider makes it: the reply
/// and the session id the server assigned.
pub(crate) fn rpc(
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

impl AppState {
    /// Deliver an event as a session's provider would.
    pub(crate) fn provider_event_for_test(
        &mut self,
        session_id: &str,
        event: AgentEvent,
        cx: &mut HostCx,
    ) {
        if let Some(session) = self.resident_mut(session_id)
            && matches!(session.runtime, Runtime::Idle)
        {
            let (commands, receiver) = smol::channel::unbounded();
            smol::spawn(async move { while receiver.recv().await.is_ok() {} }).detach();
            session.runtime = Runtime::Live(commands);
        }
        self.on_event(session_id, event, cx);
    }
}
