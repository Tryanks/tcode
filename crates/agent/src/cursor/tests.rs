//! The Cursor dialect driven as the app drives it: [`super::start`] and
//! [`super::list_models`] spawn a stand-in `cursor-agent` that relays its
//! stdio to the test, which answers as Cursor's ACP server does from
//! `tests/fixtures/cursor` (see its `PROVENANCE.md`).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value, json};
use smol::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use smol::net::{TcpListener, TcpStream};

use crate::{
    AgentEvent, ApprovalDecision, ApprovalKind, ApprovalMode, InteractionMode, ItemContent,
    ItemStatus, LaunchEnv, OptionSelection, PlanStepStatus, ResumeCursor, SessionCommand,
    SessionHandle, SessionOptions, ThreadItem, TurnStatus,
};

const RECORDED: &str = include_str!("../../tests/fixtures/cursor/recorded_signed_out.jsonl");
const SOURCE: &str = include_str!("../../tests/fixtures/cursor/source_derived.json");
const SESSION: &str = "4f1b6a52-8e0c-4c1d-9a57-2d3b1e7c9a10";

fn source(name: &str) -> Value {
    let fixtures: Value = serde_json::from_str(SOURCE).unwrap();
    let value = fixtures[name].clone();
    assert!(!value.is_null(), "no source-derived fixture {name}");
    value
}

/// A line `cursor-agent` 2026.10.01-e373342 wrote while signed out.
fn recorded(line: usize) -> Value {
    serde_json::from_str(RECORDED.lines().nth(line).unwrap()).unwrap()
}

fn recorded_initialize() -> Value {
    recorded(0)["result"].clone()
}

fn recorded_auth_required() -> Value {
    let error = recorded(2)["error"].clone();
    assert_eq!(recorded(1)["error"], error);
    error
}

/// Fails a test that would otherwise wait forever on a broken exchange.
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    smol::future::or(future, async {
        smol::Timer::after(Duration::from_secs(30)).await;
        panic!("the Cursor exchange stalled");
    })
    .await
}

fn stand_in() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cursor/cursor-agent")
}

/// The far end of the stand-in: Cursor's ACP server, as this test plays it.
struct Agent {
    listener: TcpListener,
    lines: Option<BufReader<TcpStream>>,
    out: Option<TcpStream>,
    /// The stand-in's arguments.
    argv: String,
    /// Client messages read while waiting for something else.
    pending: VecDeque<Value>,
    /// Every method the client sent.
    methods: Vec<String>,
    request_seq: u64,
}

impl Agent {
    async fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            lines: None,
            out: None,
            argv: String::new(),
            pending: VecDeque::new(),
            methods: Vec::new(),
            request_seq: 0,
        }
    }

    /// The launch environment that points the stand-in at this test.
    fn launch_env(&self) -> LaunchEnv {
        let port = self.listener.local_addr().unwrap().port();
        LaunchEnv {
            env: vec![("TCODE_STAND_IN_PORT".into(), port.to_string())],
            home: None,
        }
    }

    fn options(&self) -> SessionOptions {
        SessionOptions {
            cwd: std::env::temp_dir(),
            model: None,
            resume: None,
            fork: false,
            binary_path: Some(stand_in()),
            approval_mode: ApprovalMode::Supervised,
            option_selections: Vec::new(),
            interaction_mode: InteractionMode::Build,
            mcp_servers: Vec::new(),
            launch_env: self.launch_env(),
            extra_args: Vec::new(),
            acp: None,
        }
    }

    /// Take the stand-in's connection, then answer `initialize` as the
    /// signed-out binary did.
    async fn connect<T>(&mut self, client: &mut smol::Task<Result<T, crate::AgentError>>) {
        let (stream, _) =
            smol::future::or(async { self.listener.accept().await.unwrap() }, async {
                let error = client.await.err();
                panic!("the client ended before the stand-in connected: {error:?}")
            })
            .await;
        self.out = Some(stream.clone());
        let mut lines = BufReader::new(stream);
        let mut argv = String::new();
        lines.read_line(&mut argv).await.unwrap();
        self.argv = argv.trim_end().to_string();
        self.lines = Some(lines);
        let initialize = self.expect("initialize").await;
        assert_eq!(
            initialize["params"]["clientCapabilities"]["_meta"],
            json!({ "parameterizedModelPicker": true, "subagents": true })
        );
        self.reply(&initialize, recorded_initialize()).await;
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let lines = self.lines.as_mut().unwrap();
        assert!(
            lines.read_line(&mut line).await.unwrap() > 0,
            "the client closed the connection"
        );
        let message: Value = serde_json::from_str(&line).unwrap();
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            self.methods.push(method.to_string());
        }
        message
    }

    async fn recv(&mut self) -> Value {
        match self.pending.pop_front() {
            Some(message) => message,
            None => self.read().await,
        }
    }

    async fn expect(&mut self, method: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["method"], method, "{message}");
        message
    }

    async fn send(&mut self, message: Value) {
        self.send_all([message]).await;
    }

    /// Messages in one write, so the client reads them together.
    async fn send_all<const N: usize>(&mut self, messages: [Value; N]) {
        let mut lines = String::new();
        for message in messages {
            lines.push_str(&message.to_string());
            lines.push('\n');
        }
        let out = self.out.as_mut().unwrap();
        out.write_all(lines.as_bytes()).await.unwrap();
        out.flush().await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
            .await;
    }

    async fn fail(&mut self, request: &Value, error: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "error": error }))
            .await;
    }

    async fn update(&mut self, session: &str, update: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": session, "update": update },
        }))
        .await;
    }

    /// Send an agent→client request without waiting for its answer.
    async fn request(&mut self, method: &str, params: Value) -> String {
        self.request_seq += 1;
        let id = format!("agent-{}", self.request_seq);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        id
    }

    /// The client's answer to request `id`, leaving other messages queued.
    async fn answer(&mut self, id: &str) -> Value {
        if let Some(position) = self.pending.iter().position(|message| message["id"] == id) {
            return self.pending.remove(position).unwrap();
        }
        loop {
            let message = self.read().await;
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
            self.pending.push_back(message);
        }
    }

    async fn new_session(&mut self) -> Value {
        let new = self.expect("session/new").await;
        self.reply(&new, source("session_new")).await;
        new
    }

    /// Shut the session down, then wait until every stand-in process is
    /// gone (each holds its end of the socket until it exits). Returns every
    /// method the client sent.
    async fn finish(mut self, handle: Option<SessionHandle>) -> Vec<String> {
        if let Some(handle) = handle {
            let _ = handle.commands.send(SessionCommand::Shutdown).await;
            while handle.events.recv().await.is_ok() {}
        }
        if let Some(out) = self.out.take() {
            out.shutdown(std::net::Shutdown::Write).unwrap();
        }
        if let Some(mut lines) = self.lines.take() {
            let mut rest = String::new();
            lines.read_to_string(&mut rest).await.unwrap();
            for line in rest.lines() {
                let message: Value = serde_json::from_str(line).unwrap();
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    self.methods.push(method.to_string());
                }
            }
        }
        std::mem::take(&mut self.methods)
    }

    async fn start(&mut self, opts: SessionOptions) -> SessionHandle {
        let mut starting = smol::spawn(super::start(opts));
        self.connect(&mut starting).await;
        self.new_session().await;
        starting.await.unwrap()
    }
}

async fn send_turn(handle: &SessionHandle, text: &str) {
    handle
        .commands
        .send(SessionCommand::SendTurn {
            delivery_id: 1,
            text: text.to_string(),
            options: None,
            attachments: Vec::new(),
        })
        .await
        .unwrap();
}

/// Events up to and including the first that `last` accepts.
async fn events_until(
    handle: &SessionHandle,
    last: impl Fn(&AgentEvent) -> bool,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = handle.events.recv().await.unwrap();
        let done = last(&event);
        events.push(event);
        if done {
            return events;
        }
    }
}

fn turn_completed(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::TurnCompleted { .. })
}

fn items(events: &[AgentEvent]) -> Vec<&ThreadItem> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => Some(item),
            _ => None,
        })
        .collect()
}

/// The last state of each item, by id.
fn last_item<'a>(events: &'a [AgentEvent], id: &str) -> &'a ThreadItem {
    items(events)
        .into_iter()
        .rev()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("no item {id} in {events:#?}"))
}

fn turn_status(events: &[AgentEvent]) -> TurnStatus {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::TurnCompleted { status, usage, .. } => {
                assert_eq!(*usage, None, "Cursor reports no usage");
                Some(*status)
            }
            _ => None,
        })
        .unwrap()
}

#[test]
fn signed_out_cursor_fails_fast_with_sign_in_guidance() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let mut starting = smol::spawn(super::start(agent.options()));
        agent.connect(&mut starting).await;
        let new = agent.expect("session/new").await;
        agent.fail(&new, recorded_auth_required()).await;
        let error = starting.await.err().expect("a signed-out start fails");
        assert!(
            matches!(&error, crate::AgentError::Provider(message) if message.contains("Cursor is not signed in") && message.contains("cursor-agent login")),
            "{error}"
        );

        let mut catalog = Agent::new().await;
        let mut listing = smol::spawn(super::list_models(Some(stand_in()), catalog.launch_env()));
        catalog.connect(&mut listing).await;
        let list = catalog.expect("cursor/list_available_models").await;
        catalog.fail(&list, recorded_auth_required()).await;
        let error = listing.await.expect_err("a signed-out catalog fails");
        assert!(
            error.to_string().contains("Cursor is not signed in"),
            "{error}"
        );

        for methods in [agent.finish(None).await, catalog.finish(None).await] {
            assert!(
                !methods.iter().any(|method| method == "authenticate"),
                "`authenticate` starts a browser login: {methods:?}"
            );
        }
    }));
}

#[test]
fn catalog_ids_are_the_model_values_cursor_accepts() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let mut listing = smol::spawn(super::list_models(Some(stand_in()), agent.launch_env()));
        agent.connect(&mut listing).await;
        let list = agent.expect("cursor/list_available_models").await;
        agent.reply(&list, source("list_available_models")).await;
        let models = listing.await.unwrap();
        let ids: Vec<_> = models
            .iter()
            .map(|model| (model.id.as_str(), model.display_name.as_str()))
            .collect();
        assert_eq!(ids, [("composer-1", "Composer 1"), ("gpt-5", "GPT-5")]);
        agent.finish(None).await;
    }));
}

#[test]
fn the_composers_model_and_the_sessions_parameters_are_selected_at_start() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let mut opts = agent.options();
        opts.model = Some("gpt-5".into());
        opts.option_selections = [("acp:cfg:reasoning", "high"), ("acp:cfg:fast", "true")]
            .map(|(id, value)| OptionSelection {
                id: id.into(),
                value: json!(value),
            })
            .into();
        let mut starting = smol::spawn(super::start(opts));
        agent.connect(&mut starting).await;
        agent.new_session().await;
        let select = agent.expect("session/set_config_option").await;
        assert_eq!(
            select["params"],
            json!({ "sessionId": SESSION, "configId": "model", "value": "gpt-5" })
        );
        agent.reply(&select, source("set_model_gpt5")).await;
        // The new model has no `fast`: only its own parameter is restored.
        let restore = agent.expect("session/set_config_option").await;
        assert_eq!(
            restore["params"],
            json!({ "sessionId": SESSION, "configId": "reasoning", "value": "high" })
        );
        agent.reply(&restore, source("set_reasoning_high")).await;
        let handle = starting.await.unwrap();
        let events = events_until(&handle, |event| {
            matches!(event, AgentEvent::ProviderOptions { .. })
        })
        .await;
        let Some(AgentEvent::SessionStarted { model, .. }) = events.first() else {
            panic!("{events:#?}");
        };
        assert_eq!(model.as_deref(), Some("gpt-5"));
        let Some(AgentEvent::ProviderOptions { selections, .. }) = events.last() else {
            unreachable!();
        };
        let selections: Vec<_> = selections
            .iter()
            .map(|selection| (selection.id.as_str(), selection.value.as_str().unwrap()))
            .collect();
        assert_eq!(
            selections,
            [("acp:mode", "agent"), ("acp:cfg:reasoning", "high")],
            "the model is the composer's, and the last model's parameter is gone"
        );
        agent.finish(Some(handle)).await;

        // Already on the model: nothing to select.
        let mut agent = Agent::new().await;
        let mut opts = agent.options();
        opts.model = Some("composer-1".into());
        let handle = agent.start(opts).await;
        let methods = agent.finish(Some(handle)).await;
        assert!(
            !methods
                .iter()
                .any(|method| method == "session/set_config_option"),
            "{methods:?}"
        );
    }));
}

#[test]
fn tool_results_are_judged_by_their_raw_output_and_cursors_failure_text_fails_the_turn() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let handle = agent.start(agent.options()).await;

        send_turn(&handle, "Fix the build").await;
        let prompt = agent.expect("session/prompt").await;
        for update in source("turn_tools").as_array().unwrap().clone() {
            agent.update(SESSION, update).await;
        }
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        let events = events_until(&handle, turn_completed).await;

        match &last_item(&events, "call_shell_1").content {
            ItemContent::CommandExecution {
                command,
                output,
                exit_code,
                status,
            } => {
                assert_eq!(command, "cargo build");
                assert_eq!(output, "error[E0425]: cannot find value `x`");
                assert_eq!(*exit_code, Some(101));
                assert_eq!(*status, ItemStatus::Failed);
            }
            other => panic!("{other:?}"),
        }
        match &last_item(&events, "call_edit_1").content {
            ItemContent::FileChange { changes, status } => {
                assert_eq!(*status, ItemStatus::Completed);
                assert_eq!(changes[0].path, "src/lib.rs");
            }
            other => panic!("{other:?}"),
        }
        match &last_item(&events, "call_mcp_1").content {
            ItemContent::ToolCall { status, .. } => assert_eq!(*status, ItemStatus::Declined),
            other => panic!("{other:?}"),
        }
        match &last_item(&events, "call_grep_1").content {
            ItemContent::ToolCall { status, .. } => assert_eq!(*status, ItemStatus::Completed),
            other => panic!("{other:?}"),
        }
        // Prose that mentions an error is the model's, not a failed run.
        assert_eq!(turn_status(&events), TurnStatus::Completed);

        send_turn(&handle, "Try again").await;
        let prompt = agent.expect("session/prompt").await;
        agent
            .update(
                SESSION,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Retrying." } }),
            )
            .await;
        agent.update(SESSION, source("run_error")).await;
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        let events = events_until(&handle, turn_completed).await;
        assert_eq!(turn_status(&events), TurnStatus::Failed);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::AssistantMessage { text },
                    ..
                }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            "Retrying.\n\nError: ConnectError: [unavailable] upstream connect error"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Error { .. })),
            "the failure is already in the transcript: {events:#?}"
        );
        agent.finish(Some(handle)).await;
    }));
}

#[test]
fn questions_and_plans_are_answered_in_cursors_nested_shapes() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let handle = agent.start(agent.options()).await;
        send_turn(&handle, "Plan the storage move").await;
        let prompt = agent.expect("session/prompt").await;

        let asked = agent
            .request("cursor/ask_question", source("ask_question"))
            .await;
        let events = events_until(&handle, |event| {
            matches!(event, AgentEvent::UserInputRequested { .. })
        })
        .await;
        let Some(AgentEvent::UserInputRequested {
            request_id,
            questions,
            ..
        }) = events.last()
        else {
            unreachable!();
        };
        let asked_questions: Vec<_> = questions
            .iter()
            .map(|question| {
                let labels: Vec<_> = question
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect();
                (
                    question.id.as_str(),
                    question.question.as_str(),
                    labels,
                    question.multi_select,
                )
            })
            .collect();
        assert_eq!(
            asked_questions,
            [
                ("db", "Which database?", vec!["Postgres", "SQLite"], false),
                (
                    "extras",
                    "Which extras?",
                    vec!["Cache", "Queue", "Search"],
                    true
                ),
            ]
        );
        let mut answers = Map::new();
        answers.insert("db".into(), json!("Postgres"));
        answers.insert("extras".into(), json!(["Cache", "Search"]));
        handle
            .commands
            .send(SessionCommand::RespondUserInput {
                request_id: request_id.clone(),
                answers,
            })
            .await
            .unwrap();
        assert_eq!(
            agent.answer(&asked).await["result"],
            json!({ "outcome": {
                "outcome": "answered",
                "answers": [
                    { "questionId": "db", "selectedOptionIds": ["pg"] },
                    { "questionId": "extras", "selectedOptionIds": ["cache", "search"] },
                ],
            } })
        );

        agent.update(SESSION, source("create_plan_entries")).await;
        let planned = agent
            .request("cursor/create_plan", source("create_plan"))
            .await;
        assert_eq!(
            agent.answer(&planned).await["result"],
            json!({ "outcome": { "outcome": "accepted" } })
        );
        let events = events_until(&handle, |event| {
            matches!(event, AgentEvent::ProposedPlan { .. })
        })
        .await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::ProposedPlan { item_id, markdown })
                if item_id == "call_plan_1" && markdown == "# Storage\n\n1. Add the migration\n2. Wire the repository\n"
        ));

        let todos = agent
            .request("cursor/update_todos", source("update_todos"))
            .await;
        assert_eq!(agent.answer(&todos).await["result"], json!({}));
        let merged = agent
            .request("cursor/update_todos", source("update_todos_merge"))
            .await;
        assert_eq!(agent.answer(&merged).await["result"], json!({}));
        let task = agent.request("cursor/task", source("task")).await;
        assert_eq!(agent.answer(&task).await["result"], json!({}));
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;

        let events = events_until(&handle, turn_completed).await;
        let plans: Vec<Vec<(&str, PlanStepStatus)>> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PlanUpdated { steps, .. } => Some(
                    steps
                        .iter()
                        .map(|step| (step.step.as_str(), step.status))
                        .collect(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            plans.last().unwrap(),
            &[
                ("Add the migration", PlanStepStatus::Completed),
                ("Wire the repository", PlanStepStatus::Pending),
            ],
            "a merge updates by id and a cancelled todo leaves the plan: {plans:?}"
        );
        assert_eq!(turn_status(&events), TurnStatus::Completed);
        agent.finish(Some(handle)).await;
    }));
}

#[test]
fn an_interrupt_cancels_a_pending_question() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let handle = agent.start(agent.options()).await;
        send_turn(&handle, "Plan the storage move").await;
        let prompt = agent.expect("session/prompt").await;
        let asked = agent
            .request("cursor/ask_question", source("ask_question"))
            .await;
        events_until(&handle, |event| {
            matches!(event, AgentEvent::UserInputRequested { .. })
        })
        .await;
        handle
            .commands
            .send(SessionCommand::Interrupt)
            .await
            .unwrap();
        assert_eq!(
            agent.answer(&asked).await["result"],
            json!({ "outcome": { "outcome": "cancelled" } })
        );
        let cancel = agent.expect("session/cancel").await;
        assert_eq!(cancel["params"]["sessionId"], SESSION);
        agent
            .reply(&prompt, json!({ "stopReason": "cancelled" }))
            .await;
        let events = events_until(&handle, turn_completed).await;
        assert_eq!(turn_status(&events), TurnStatus::Interrupted);
        agent.finish(Some(handle)).await;
    }));
}

#[test]
fn permissions_are_granted_once_by_kind_and_otherwise_asked() {
    smol::block_on(bounded(async {
        for (mode, granted, asked) in [
            (ApprovalMode::ReadOnly, "permission_read", "permission_edit"),
            (
                ApprovalMode::AutoAcceptEdits,
                "permission_edit",
                "permission_shell",
            ),
            (ApprovalMode::Supervised, "", "permission_read"),
        ] {
            let mut agent = Agent::new().await;
            let mut opts = agent.options();
            opts.approval_mode = mode;
            let handle = agent.start(opts).await;
            assert_eq!(agent.argv, "acp", "{mode:?}");
            send_turn(&handle, "Go").await;
            let prompt = agent.expect("session/prompt").await;

            if !granted.is_empty() {
                let mut params = source(granted);
                params["sessionId"] = json!(SESSION);
                let id = agent.request("session/request_permission", params).await;
                assert_eq!(
                    agent.answer(&id).await["result"],
                    json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
                    "{mode:?}"
                );
            }

            let mut params = source(asked);
            params["sessionId"] = json!(SESSION);
            let id = agent.request("session/request_permission", params).await;
            let events = events_until(&handle, |event| {
                matches!(event, AgentEvent::ApprovalRequested(_))
            })
            .await;
            let approvals: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ApprovalRequested(request) => Some(request),
                    _ => None,
                })
                .collect();
            assert_eq!(
                approvals.len(),
                1,
                "{mode:?}: only {asked} is asked: {events:#?}"
            );
            let request = approvals[0];
            let expected_kind = match asked {
                "permission_edit" => matches!(request.kind, ApprovalKind::FileChange { .. }),
                "permission_shell" => matches!(request.kind, ApprovalKind::ExecCommand { .. }),
                _ => matches!(request.kind, ApprovalKind::FileRead { .. }),
            };
            assert!(expected_kind, "{mode:?}: {request:?}");
            handle
                .commands
                .send(SessionCommand::RespondApproval {
                    request_id: request.id.clone(),
                    decision: ApprovalDecision::ApproveForSession,
                })
                .await
                .unwrap();
            assert_eq!(
                agent.answer(&id).await["result"],
                json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }),
                "{mode:?}: a session-wide approval never writes Cursor's allowlist"
            );
            agent
                .reply(&prompt, json!({ "stopReason": "end_turn" }))
                .await;
            events_until(&handle, turn_completed).await;
            agent.finish(Some(handle)).await;
        }

        let mut agent = Agent::new().await;
        let mut opts = agent.options();
        opts.approval_mode = ApprovalMode::FullAccess;
        opts.extra_args = vec!["--sandbox".into(), "disabled".into()];
        let handle = agent.start(opts).await;
        assert_eq!(agent.argv, "--force --sandbox disabled acp");
        agent.finish(Some(handle)).await;
    }));
}

#[test]
fn a_loaded_session_replays_nothing_and_a_failed_load_is_an_error() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let mut opts = agent.options();
        opts.resume = Some(ResumeCursor(json!({ "session_id": SESSION })));
        let cwd = opts.cwd.clone();
        let mut starting = smol::spawn(super::start(opts));
        agent.connect(&mut starting).await;
        let load = agent.expect("session/load").await;
        assert_eq!(
            load["params"],
            json!({ "sessionId": SESSION, "cwd": cwd, "mcpServers": [] })
        );
        for update in source("load_replay").as_array().unwrap().clone() {
            agent.update(SESSION, update).await;
        }
        // A blocking request mid-replay is answered without the user.
        let asked = agent
            .request("cursor/ask_question", source("ask_question"))
            .await;
        assert_eq!(
            agent.answer(&asked).await["result"],
            json!({ "outcome": { "outcome": "cancelled" } })
        );
        // Cursor sends its slash commands as soon as the load has answered.
        agent
            .send_all([
                json!({ "jsonrpc": "2.0", "id": load["id"], "result": source("session_load") }),
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "sessionId": SESSION, "update": source("available_commands") },
                }),
            ])
            .await;
        let handle = starting.await.unwrap();
        let events = events_until(&handle, |event| {
            matches!(event, AgentEvent::ProviderCommands { .. })
        })
        .await;
        assert!(
            events.iter().all(|event| matches!(
                event,
                AgentEvent::SessionStarted { .. }
                    | AgentEvent::ProviderOptions { .. }
                    | AgentEvent::ProviderCommands { .. }
            )),
            "{events:#?}"
        );
        let Some(AgentEvent::SessionStarted { resume, .. }) = events.first() else {
            panic!("{events:#?}");
        };
        assert_eq!(resume.0, json!({ "session_id": SESSION }));
        agent.finish(Some(handle)).await;

        let mut agent = Agent::new().await;
        let mut opts = agent.options();
        opts.resume = Some(ResumeCursor(json!({ "session_id": "gone" })));
        let mut starting = smol::spawn(super::start(opts));
        agent.connect(&mut starting).await;
        let load = agent.expect("session/load").await;
        agent
            .fail(
                &load,
                json!({ "code": -32602, "message": "Invalid params", "data": { "message": "Session \"gone\" not found" } }),
            )
            .await;
        let error = starting.await.err().expect("a failed load fails the start");
        assert!(
            error.to_string().contains("could not resume session gone"),
            "{error}"
        );
        let methods = agent.finish(None).await;
        assert!(
            !methods.iter().any(|method| method == "session/new"),
            "never a fresh session in its place: {methods:?}"
        );
    }));
}

#[test]
fn subagent_sessions_become_subagent_items_with_their_own_activity() {
    smol::block_on(bounded(async {
        let mut agent = Agent::new().await;
        let handle = agent.start(agent.options()).await;
        send_turn(&handle, "Survey the schema").await;
        let prompt = agent.expect("session/prompt").await;
        for notification in source("subagent_turn").as_array().unwrap().clone() {
            agent
                .update(
                    notification["sessionId"].as_str().unwrap(),
                    notification["update"].clone(),
                )
                .await;
        }
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        let events = events_until(&handle, turn_completed).await;

        let child = "9e8d7c6b-5a49-4382-a1b0-c9d8e7f6a5b4";
        let mut subagent = Vec::new();
        let mut children = Vec::new();
        for event in &events {
            match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item) => match (&item.content, &item.parent_item_id) {
                    (
                        ItemContent::Subagent {
                            agent_type,
                            description,
                            status,
                            model,
                            ..
                        },
                        None,
                    ) => {
                        assert_eq!(item.id, "call_task_1");
                        assert_eq!(
                            (agent_type.as_str(), description.as_str()),
                            ("explore", "Survey the schema")
                        );
                        subagent.push((*status, model.clone()));
                    }
                    (content, Some(parent)) => {
                        assert_eq!(parent, "call_task_1");
                        children.push((item.id.clone(), content.clone()));
                    }
                    (content, None) => assert!(
                        matches!(content, ItemContent::AssistantMessage { text } if text == "The schema has one table."),
                        "only the parent's own text stays in the parent: {content:?}"
                    ),
                },
                AgentEvent::Delta { item_id, .. } => {
                    assert!(
                        !item_id.starts_with(child),
                        "a subagent's stream stays inside it"
                    )
                }
                _ => {}
            }
        }
        assert_eq!(
            subagent.first(),
            Some(&(ItemStatus::InProgress, None)),
            "{subagent:?}"
        );
        assert_eq!(
            subagent.last(),
            Some(&(ItemStatus::Completed, Some("composer-1".to_string()))),
            "{subagent:?}"
        );
        let texts: Vec<_> = children
            .iter()
            .filter_map(|(_, content)| match content {
                ItemContent::AssistantMessage { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Looking at the migrations.", "One table: users."]);
        assert!(
            children.iter().any(|(id, content)| id == &format!("{child}/call_shell_1")
                && matches!(content, ItemContent::CommandExecution { output, status: ItemStatus::Completed, .. } if output == "0001_init.sql\n")),
            "{children:#?}"
        );
        assert!(
            children
                .iter()
                .all(|(id, _)| id.starts_with(&format!("{child}/"))),
            "{children:#?}"
        );
        agent.finish(Some(handle)).await;
    }));
}
