//! Cursor's dialect fed Cursor's messages as `tests/fixtures/cursor` holds
//! them (see its `PROVENANCE.md`): the recorded signed-out answers, and shapes
//! taken from the shipped JavaScript with contents made up.

use std::path::PathBuf;

use agent_client_protocol::schema::v1 as acp;
use serde_json::{Map, Value, json};

use super::{Cursor, Questions};
use crate::acp_session::{Dialect as _, State};
use crate::{
    AgentEvent, ItemContent, ItemStatus, LaunchEnv, PlanStepStatus, SessionOptions, ThreadItem,
    TurnStatus,
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

fn options() -> SessionOptions {
    SessionOptions {
        cwd: std::env::temp_dir(),
        model: None,
        resume: None,
        fork: None,
        binary_path: Some(PathBuf::from("/opt/cursor/bin/cursor-agent")),
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env: LaunchEnv::default(),
        extra_args: Vec::new(),
        acp: None,
        instructions: None,
        processes: Default::default(),
        clock: Default::default(),
        worktrees: Vec::new(),
    }
}

/// One live turn: each `session/update` of `session` routed as the ACP
/// session routes it (one the schema reads to `session_update`, any other
/// to `notification`), then the prompt answering `end_turn`.
fn turn(
    cursor: &Cursor,
    state: &mut State,
    updates: impl IntoIterator<Item = (String, Value)>,
) -> Vec<AgentEvent> {
    let mut events = state.begin_agent_turn("turn-1");
    for (session, update) in updates {
        let params = json!({ "sessionId": session, "update": update });
        events.extend(
            match serde_json::from_value::<acp::SessionNotification>(params.clone()) {
                Ok(notification) => cursor.session_update(state, notification),
                Err(_) => cursor.notification(state, "session/update", params),
            },
        );
    }
    let end = cursor.turn_end(
        state,
        Ok(serde_json::from_value(json!({ "stopReason": "end_turn" })).unwrap()),
    );
    events.extend(state.end_turn("turn-1", end));
    events
}

fn own(updates: Value) -> impl Iterator<Item = (String, Value)> {
    updates
        .as_array()
        .unwrap()
        .clone()
        .into_iter()
        .map(|update| (SESSION.to_string(), update))
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

/// `cursor-agent` 2026.10.01-e373342 refusing `session/new` while signed out.
#[test]
fn a_signed_out_start_says_how_to_sign_in() {
    let refused: Value = serde_json::from_str(RECORDED.lines().nth(2).unwrap()).unwrap();
    let error: acp::Error = serde_json::from_value(refused["error"].clone()).unwrap();
    let failure = super::start_failure(&error, "could not start a session");
    assert!(
        matches!(&failure, crate::AgentError::Provider(message)
            if message.contains("Cursor is not signed in") && message.contains("cursor-agent login")),
        "{failure}"
    );
}

/// `cursor-agent acp` runs Cursor's ACP server; `--force` runs its tools
/// unasked, and the client `_meta` turns on the parameterized model picker
/// and subagent sessions.
#[test]
fn a_session_launches_cursor_s_acp_server() {
    let opts = options();
    let launch = Cursor::new(&opts).launch(&opts).unwrap();
    assert_eq!(
        launch.program,
        PathBuf::from("/opt/cursor/bin/cursor-agent")
    );
    assert_eq!(launch.args, ["--force", "acp"]);
    assert_eq!(
        launch.client_meta.map(Value::Object),
        Some(json!({ "parameterizedModelPicker": true, "subagents": true }))
    );
}

#[test]
fn catalog_ids_are_the_model_values_cursor_accepts() {
    let models = super::catalog(&source("list_available_models"));
    let ids: Vec<_> = models
        .iter()
        .map(|model| (model.id.as_str(), model.display_name.as_str()))
        .collect();
    assert_eq!(ids, [("composer-1", "Composer 1"), ("gpt-5", "GPT-5")]);
}

#[test]
fn tool_results_are_judged_by_their_raw_output_and_cursors_failure_text_fails_the_turn() {
    let opts = options();
    let cursor = Cursor::new(&opts);
    let mut state = State::new(opts.cwd.clone());
    let events = turn(&cursor, &mut state, own(source("turn_tools")));

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
        ItemContent::FileChange {
            changes, status, ..
        } => {
            assert_eq!(*status, ItemStatus::Completed);
            assert_eq!(changes[0].path, "src/lib.rs");
        }
        other => panic!("{other:?}"),
    }
    // An MCP call is the server's tool, its input the tool's arguments.
    match &last_item(&events, "call_mcp_1").content {
        ItemContent::ToolCall {
            name,
            input,
            status,
            ..
        } => {
            assert_eq!(name, "github/create_issue");
            assert_eq!(input, &json!({"title": "Build broke"}));
            assert_eq!(*status, ItemStatus::Declined);
        }
        other => panic!("{other:?}"),
    }
    match &last_item(&events, "call_grep_1").content {
        ItemContent::ToolCall { status, .. } => assert_eq!(*status, ItemStatus::Completed),
        other => panic!("{other:?}"),
    }
    // Prose that mentions an error is the model's, not a failed run.
    assert_eq!(turn_status(&events), TurnStatus::Completed);

    let events = turn(
        &cursor,
        &mut state,
        own(json!([
            { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "Retrying." } },
            source("run_error"),
        ])),
    );
    assert_eq!(turn_status(&events), TurnStatus::Failed);
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text, .. },
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
}

#[test]
fn questions_and_todos_are_answered_in_cursors_nested_shapes() {
    let questions = Questions::parse(&source("ask_question")).unwrap();
    let asked: Vec<_> = questions
        .canonical()
        .into_iter()
        .map(|question| {
            let labels: Vec<_> = question
                .options
                .into_iter()
                .map(|option| option.label)
                .collect();
            (
                question.id,
                question.question,
                labels,
                question.multi_select,
            )
        })
        .collect();
    assert_eq!(
        asked,
        [
            (
                "db".to_string(),
                "Which database?".to_string(),
                vec!["Postgres".to_string(), "SQLite".to_string()],
                false
            ),
            (
                "extras".to_string(),
                "Which extras?".to_string(),
                vec![
                    "Cache".to_string(),
                    "Queue".to_string(),
                    "Search".to_string()
                ],
                true
            ),
        ]
    );
    let mut answers = Map::new();
    answers.insert("db".into(), json!("Postgres"));
    answers.insert("extras".into(), json!(["Cache", "Search"]));
    assert_eq!(
        questions.reply(Some(answers)),
        (
            json!({ "outcome": {
                "outcome": "answered",
                "answers": [
                    { "questionId": "db", "selectedOptionIds": ["pg"] },
                    { "questionId": "extras", "selectedOptionIds": ["cache", "search"] },
                ],
            } }),
            Vec::new()
        )
    );
    // An interrupt or a shutdown answers a pending question cancelled.
    assert_eq!(
        questions.reply(None),
        (json!({ "outcome": { "outcome": "cancelled" } }), Vec::new())
    );

    let cursor = Cursor::new(&options());
    cursor.update_todos(&source("update_todos"));
    let plan: Vec<_> = cursor
        .update_todos(&source("update_todos_merge"))
        .into_iter()
        .map(|step| (step.step, step.status))
        .collect();
    assert_eq!(
        plan,
        [
            ("Add the migration".to_string(), PlanStepStatus::Completed),
            ("Wire the repository".to_string(), PlanStepStatus::Pending),
        ],
        "a merge updates by id and a cancelled todo leaves the plan"
    );
}

#[test]
fn subagent_sessions_become_subagent_items_with_their_own_activity() {
    let opts = options();
    let cursor = Cursor::new(&opts);
    let mut state = State::new(opts.cwd.clone());
    let notifications = source("subagent_turn").as_array().unwrap().clone();
    let events = turn(
        &cursor,
        &mut state,
        notifications.into_iter().map(|notification| {
            (
                notification["sessionId"].as_str().unwrap().to_string(),
                notification["update"].clone(),
            )
        }),
    );

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
                    matches!(content, ItemContent::AssistantMessage { text, .. } if text == "The schema has one table."),
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
            ItemContent::AssistantMessage { text, .. } => Some(text.as_str()),
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
}
