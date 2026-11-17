use super::*;
use crate::McpRegistration;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/opencode");

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
}

/// A recorded `/api/event` frame sequence, one event per line.
fn frames(name: &str) -> Vec<Value> {
    fixture_text(name)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The session's mapper fed a recorded stream: each event it emits, as
/// wire JSON, followed by what it asks the actor to do for that frame.
fn replay(name: &str, mcp_servers: &[&str]) -> Vec<Value> {
    let mut mapper = Mapper::new(
        "ses_root".into(),
        mcp_servers
            .iter()
            .map(|server| (*server).to_owned())
            .collect(),
        HashMap::new(),
    );
    frames(name)
        .iter()
        .flat_map(|frame| feed(&mut mapper, frame))
        .collect()
}

fn feed(mapper: &mut Mapper, frame: &Value) -> Vec<Value> {
    let mapped = mapper.on_event(frame).unwrap();
    let mut out = wire(&mapped.events);
    out.extend(mapped.actions.into_iter().map(|action| match action {
            Action::PollShell {
                item_id,
                shell_id,
                directory,
            } => json!({"action": "poll_shell", "item_id": item_id, "shell_id": shell_id, "directory": directory}),
            Action::StopShell { item_id } => {
                json!({"action": "stop_shell", "item_id": item_id})
            }
            Action::DrainShell { item_id } => {
                json!({"action": "drain_shell", "item_id": item_id})
            }
            Action::FetchDiff => json!({"action": "fetch_diff"}),
            Action::RemoveShell {
                shell_id,
                directory,
            } => json!({"action": "remove_shell", "shell_id": shell_id, "directory": directory}),
            Action::InterruptSession { session_id } => {
                json!({"action": "interrupt_session", "session_id": session_id})
            }
        }));
    out
}

fn wire(events: &[AgentEvent]) -> Vec<Value> {
    events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

/// The events a turn's bookkeeping adds around its items, and the
/// actor's side effects, left out.
fn items_only(out: Vec<Value>) -> Vec<Value> {
    out.into_iter()
        .filter(|out| {
            !matches!(
                out["type"].as_str(),
                None | Some("token_usage" | "turn_started" | "served_model")
            )
        })
        .collect()
}

#[test]
fn a_text_turn_streams_the_reply_and_reports_its_usage() {
    assert_eq!(
        replay("text_turn.jsonl", &[]),
        vec![
            json!({"type": "turn_started", "turn_id": "evt_198aac9f500200000000000325"}),
            json!({"type": "served_model", "model": "opencode/deepseek-v4.1-flash-free", "reason": null}),
            json!({"type": "item_started", "id": "msg_198aaca0a00100000000000328:text:0", "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": "msg_198aaca0a00100000000000328:text:0", "kind": "assistant_text", "text": "PONG"}),
            json!({"type": "item_completed", "id": "msg_198aaca0a00100000000000328:text:0", "content": {"kind": "assistant_message", "text": "PONG"}}),
            json!({"type": "token_usage", "freshness": "current", "turn_processed_tokens": null, "input_tokens": 6335, "cached_input_tokens": 3328, "output_tokens": 4, "used_tokens": 9667, "context_window": null, "total_processed_tokens": null, "cost_usd": null, "duration_ms": null}),
            json!({"type": "turn_completed", "turn_id": "evt_198aac9f500200000000000325", "status": "completed", "usage": {"freshness": "current", "turn_processed_tokens": 9667, "input_tokens": 6335, "cached_input_tokens": 3328, "output_tokens": 4, "used_tokens": 9667, "context_window": null, "total_processed_tokens": null, "cost_usd": 0.0, "duration_ms": null}}),
        ]
    );
}

/// `read` gives a page of `N: text` lines under its `Read file` header,
/// and says where to continue when the file goes on (v2.2.7's
/// `toModelContent`, `packages/core/src/tool/plugin/read.ts`; content
/// invented). A directory's page is a listing of its entries.
// The fixture's paths are as the harness prints them on a POSIX machine.
#[cfg(unix)]
#[test]
fn a_read_is_the_lines_it_read_from_where_it_began() {
    let page = "Read file src/lib.rs, lines 40-41\n40: fn main() {\n41: }\n[Output truncated. Continue reading with offset: 42]";
    assert_eq!(
        tool_family(
            "read",
            &json!({"path": "src/lib.rs", "offset": 40}),
            Some(page.into()),
            None
        ),
        (
            Some(ToolFamily::Read {
                path: "src/lib.rs".into(),
                first_line: Some(40),
                note: Some("[Output truncated. Continue reading with offset: 42]".into()),
            }),
            Some("fn main() {\n}".into()),
        )
    );
    // A page laid out any other way is the call's output as written.
    let past_the_end = "Read file src/lib.rs, lines 90-90 (offset 95 is past the end; the file has 90 lines)\n90: }";
    assert_eq!(
        tool_family(
            "read",
            &json!({"path": "src/lib.rs", "offset": 95}),
            Some(past_the_end.into()),
            None
        ),
        (
            Some(ToolFamily::Read {
                path: "src/lib.rs".into(),
                first_line: None,
                note: None,
            }),
            Some(past_the_end.into()),
        )
    );
    let listing = "Read directory src, entries 1-2\nlib.rs\nbin/";
    assert_eq!(
        tool_family("read", &json!({"path": "src"}), Some(listing.into()), None),
        (
            Some(ToolFamily::Search {
                scope: SearchScope::Entries,
                pattern: None,
                path: Some("src".into()),
                glob: None,
                hits: Some(vec![hit("src/lib.rs"), hit("src/bin/")]),
            }),
            Some(listing.into())
        )
    );
}

/// grep names each file by its absolute path and its matches under it
/// as `  Line N: text`; glob lists the absolute path of each file
/// (v2.2.7's `toModelContent` in `grep.ts` and `glob.ts`; content
/// invented).
// The fixture's paths are as the harness prints them on a POSIX machine.
#[cfg(unix)]
#[test]
fn a_search_is_what_it_found() {
    let grep = "Found 2 matches\n/work/src/a.rs:\n  Line 3: fn new() {}\n  Line 9: fn new_at(x: u8) {}\n\n(Results are truncated: showing first 2 results. Consider using a more specific path or pattern.)";
    let (family, _) = tool_family(
        "grep",
        &json!({"pattern": "fn new", "path": "src", "include": "*.rs"}),
        Some(grep.into()),
        None,
    );
    assert_eq!(
        family,
        Some(ToolFamily::Search {
            scope: SearchScope::Content,
            pattern: Some("fn new".into()),
            path: Some("src".into()),
            glob: Some("*.rs".into()),
            hits: Some(vec![
                SearchHit {
                    path: "/work/src/a.rs".into(),
                    line: Some(3),
                    text: Some("fn new() {}".into()),
                },
                SearchHit {
                    path: "/work/src/a.rs".into(),
                    line: Some(9),
                    text: Some("fn new_at(x: u8) {}".into()),
                },
            ]),
        })
    );
    let (family, _) = tool_family(
        "glob",
        &json!({"pattern": "**/*.rs"}),
        Some("/work/src/a.rs\n/work/src/b.rs".into()),
        None,
    );
    assert_eq!(
        family,
        Some(ToolFamily::Search {
            scope: SearchScope::Files,
            pattern: Some("**/*.rs".into()),
            path: None,
            glob: None,
            hits: Some(vec![hit("/work/src/a.rs"), hit("/work/src/b.rs")]),
        })
    );
}

/// question reports each question's answers as a list in its metadata,
/// in the questions' order (v2.2.7's `question.ts`; content invented).
#[test]
fn a_question_is_what_it_asked_and_the_answers_its_metadata_lists() {
    let (family, _) = tool_family(
            "question",
            &json!({"questions": [{"question": "Which colours?", "header": "Colours", "multiple": true, "options": [{"label": "Red, green", "description": "Warm and cool"}, {"label": "Blue", "description": "Cool"}]}]}),
            Some("User has answered your questions: \"Which colours?\"=\"Red, green, Blue\". You can now continue with the user's answers in mind.".into()),
            Some(&json!({"answers": [["Red, green", "Blue"]]})),
        );
    let Some(ToolFamily::Question { questions }) = family else {
        panic!("{family:?}");
    };
    assert_eq!(questions[0].question, "Which colours?");
    assert_eq!(questions[0].answers, ["Red, green", "Blue"]);
}

/// websearch heads each page `## [title](url)` (v2.2.7's `websearch.ts`;
/// content invented).
#[test]
fn a_web_search_is_the_pages_it_found() {
    let text = "## [CORS](https://example.com/cors)\nPublished: 2026-01-02T00:00:00.000Z\n\nA page about it.\n\n## [Preflight [MDN]](https://example.org/preflight)";
    let (family, _) = tool_family(
        "websearch",
        &json!({"query": "cors preflight"}),
        Some(text.into()),
        Some(&json!({"provider": "exa"})),
    );
    assert_eq!(
        family,
        Some(ToolFamily::WebSearch {
            query: "cors preflight".into(),
            links: Some(vec![
                WebLink {
                    title: "CORS".into(),
                    url: "https://example.com/cors".into(),
                },
                WebLink {
                    title: "Preflight [MDN]".into(),
                    url: "https://example.org/preflight".into(),
                },
            ]),
        })
    );
}

/// skill is asked for by `id` and names the skill it loaded in its
/// metadata (v2.2.7's `skill.ts`; content invented).
#[test]
fn a_skill_is_the_skill_it_loaded() {
    let (family, _) = tool_family(
        "skill",
        &json!({"id": "release"}),
        Some("<skill_content name=\"release\"># Skill: release</skill_content>".into()),
        Some(&json!({"name": "release", "directory": "/work/.opencode/skills/release"})),
    );
    assert_eq!(
        family,
        Some(ToolFamily::Skill {
            name: "release".into()
        })
    );
}

/// The call's output is polled from the shell its progress names, until
/// the call ends with its output and exit code.
#[test]
fn a_shell_call_polls_its_shell_and_reports_its_exit_code() {
    let id = "msg_198ac3fdc00100000000000232:chatcmpl-tool-0000000000000009";
    assert_eq!(
        items_only(replay("shell_turn.jsonl", &[]))[..5],
        [
            json!({"type": "item_started", "id": id, "content": {"kind": "command_execution", "command": "", "output": "", "exit_code": null, "status": "in_progress"}}),
            json!({"type": "item_updated", "id": id, "content": {"kind": "command_execution", "command": "echo one; sleep 6; echo two", "output": "", "exit_code": null, "status": "in_progress"}}),
            json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "echo one; sleep 6; echo two", "output": "one\ntwo\n", "exit_code": 0, "status": "completed"}}),
            json!({"type": "item_started", "id": "msg_198ac5ee500100000000000245:text:0", "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": "msg_198ac5ee500100000000000245:text:0", "kind": "assistant_text", "text": "DONE"}),
        ]
    );
    let shell: Vec<Value> = replay("shell_turn.jsonl", &[])
        .into_iter()
        .filter(|out| out["action"].is_string() || out["type"] == "item_completed")
        .take(3)
        .collect();
    assert_eq!(
        shell,
        [
            json!({"action": "poll_shell", "item_id": id, "shell_id": "sh_198ac475f00100000000000238", "directory": "/work/project"}),
            json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "echo one; sleep 6; echo two", "output": "one\ntwo\n", "exit_code": 0, "status": "completed"}}),
            json!({"action": "stop_shell", "item_id": id}),
        ]
    );
}

/// A command run in the background returns at once with
/// `metadata.status: "running"` and ends with its shell's `shell.exited`,
/// after the turn that started it (v2.3.0's `shell.ts`, and `shell.exited`
/// in its `/openapi.json`; content invented). It settles once the poll
/// of its output has ended.
#[test]
fn a_command_in_the_background_ends_with_its_shells_exit_once_its_poll_has_ended() {
    let id = "msg_19c0000a100100000000000a01:call_0000000000000021";
    let running = json!({"type": "item_updated", "id": id, "content": {"kind": "command_execution", "command": "make build", "output": "", "exit_code": null, "status": "in_progress"}});
    let mut mapper = Mapper::new("ses_root".into(), Vec::new(), HashMap::new());
    let out: Vec<Value> = frames("background_shell_turn.jsonl")
        .iter()
        .flat_map(|frame| feed(&mut mapper, frame))
        .collect();
    assert_eq!(
        out[2..],
        [
            running.clone(),
            json!({"action": "poll_shell", "item_id": id, "shell_id": "sh_19c0000b100100000000000b01", "directory": "/work/project"}),
            running,
            json!({"type": "item_backgrounded", "item_id": id}),
            json!({"type": "turn_completed", "turn_id": "evt_19c000000200100000000000002", "status": "completed", "usage": null}),
            json!({"action": "drain_shell", "item_id": id}),
        ]
    );
    let mut settled = wire(&mapper.shell_output(id, "built\n".into()));
    settled.extend(wire(&mapper.shell_poll_ended(id)));
    assert_eq!(
        settled,
        [
            json!({"type": "delta", "item_id": id, "kind": "command_output", "text": "built\n"}),
            json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "make build", "output": "built\n", "exit_code": 2, "status": "completed"}}),
        ]
    );
}

/// Each file tool carries its own diff, the todo list is the plan, and
/// every step that changed files has the turn's diff read again.
#[test]
fn file_tools_carry_their_own_diffs_todos_set_the_plan_and_changed_steps_read_the_turn_diff() {
    let write = "msg_198b17ffc00100000000000031:chatcmpl-tool-0000000000000002";
    let edit = "msg_198b183de00100000000000040:chatcmpl-tool-0000000000000003";
    let fetch_diff = json!({"action": "fetch_diff"});
    let events: Vec<Value> = replay("files_turn.jsonl", &[])
        .into_iter()
        .filter(|out| {
            !matches!(
                out["type"].as_str(),
                Some("token_usage" | "turn_started" | "served_model" | "turn_completed")
            ) && out["action"] != "stop_shell"
        })
        .collect();
    assert_eq!(
        events,
        vec![
            json!({"type": "item_started", "id": "msg_198b1772900100000000000017:text:0", "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": "msg_198b1772900100000000000017:text:0", "kind": "assistant_text", "text": "I'll do these in order.\n\n"}),
            json!({"type": "item_completed", "id": "msg_198b1772900100000000000017:text:0", "content": {"kind": "assistant_message", "text": "I'll do these in order.\n\n"}}),
            json!({"type": "plan_updated", "turn_id": "evt_198b1771a00100000000000013", "explanation": null, "steps": [{"step": "Write code", "status": "inProgress"}, {"step": "Test code", "status": "pending"}]}),
            json!({"type": "item_started", "id": write, "content": {"kind": "file_change", "changes": [], "status": "in_progress"}}),
            json!({"type": "item_updated", "id": write, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "modify", "diff": null}], "status": "in_progress"}}),
            json!({"type": "item_completed", "id": write, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "create", "diff": "+blue"}], "status": "completed", "output": "Created file successfully: notes2.txt"}}),
            fetch_diff.clone(),
            json!({"type": "item_started", "id": edit, "content": {"kind": "file_change", "changes": [], "status": "in_progress"}}),
            json!({"type": "item_updated", "id": edit, "content": {"kind": "file_change", "changes": [{"path": "/work/project/notes2.txt", "kind": "modify", "diff": null}], "status": "in_progress"}}),
            json!({"type": "item_completed", "id": edit, "content": {"kind": "file_change", "changes": [{"path": "notes2.txt", "kind": "modify", "diff": "Index: notes2.txt\n===================================================================\n--- notes2.txt\n+++ notes2.txt\n@@ -1,1 +1,1 @@\n-blue\n+green\n"}], "status": "completed", "output": "Edited notes2.txt (1 replacement)"}}),
            fetch_diff,
            json!({"type": "item_started", "id": "msg_198b1897700100000000000049:text:0", "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": "msg_198b1897700100000000000049:text:0", "kind": "assistant_text", "text": "DONE"}),
            json!({"type": "item_completed", "id": "msg_198b1897700100000000000049:text:0", "content": {"kind": "assistant_message", "text": "DONE"}}),
        ]
    );
}

#[test]
fn a_subagents_activity_nests_under_the_call_that_spawned_it() {
    let spawn = "msg_198acc1db00100000000000290:chatcmpl-tool-0000000000000011";
    let child = "msg_198accaf200100000000000303:text:0";
    let reply = "msg_198accd8900100000000000315:text:0";
    let out = replay("subagent_turn.jsonl", &[]);
    assert_eq!(
        out.iter()
            .filter(|out| out["type"] == "turn_completed")
            .count(),
        1,
        "the child's execution is not a turn of its own"
    );
    assert_eq!(
        items_only(out)[..9],
        [
            json!({"type": "item_started", "id": spawn, "content": {"kind": "subagent", "agent_type": "", "description": "", "status": "in_progress", "summary": null}}),
            json!({"type": "item_updated", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "in_progress", "summary": null}}),
            json!({"type": "item_updated", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "in_progress", "summary": null, "model": "opencode/deepseek-v4.1-flash-free"}}),
            json!({"type": "item_started", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": child, "kind": "assistant_text", "text": "KIWI"}),
            json!({"type": "item_completed", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": "KIWI"}}),
            json!({"type": "item_completed", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Reply with KIWI", "status": "completed", "summary": "KIWI", "model": "opencode/deepseek-v4.1-flash-free"}}),
            json!({"type": "item_started", "id": reply, "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": reply, "kind": "assistant_text", "text": "The subagent said: KIWI"}),
        ]
    );
}

/// From 2.3.0 every subagent runs in the background: its call returns
/// `metadata.status: "running"`, and what the child replied reaches the
/// parent as a `synthetic` inbox item naming the child (v2.3.0's
/// `subagent.ts` and `subagent-completion.ts`, and
/// `Session.Inbox.SyntheticPayload` in its `/openapi.json`; content
/// invented). The call ends with that report, and the message is shown as
/// OpenCode wrote it where the model receives it.
#[test]
fn a_subagent_ends_with_its_report_and_the_model_is_shown_what_opencode_wrote_it() {
    let spawn = "msg_19c0000c100100000000000c01:call_0000000000000022";
    let child = "msg_19c0000d100100000000000d01:text:0";
    let running = json!({"type": "item_updated", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Name a fruit", "status": "in_progress", "summary": null}});
    assert_eq!(
        replay("subagent_report_turn.jsonl", &[])[2..],
        [
            running.clone(),
            running,
            json!({"type": "item_backgrounded", "item_id": spawn}),
            json!({"type": "turn_completed", "turn_id": "evt_19c000000200100000000000002", "status": "completed", "usage": null}),
            json!({"type": "item_started", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "item_completed", "id": child, "parent_item_id": spawn, "content": {"kind": "assistant_message", "text": "KIWI"}}),
            json!({"type": "item_completed", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Name a fruit", "status": "completed", "summary": "KIWI"}}),
            json!({"type": "turn_started", "turn_id": "evt_19c000001700100000000000017"}),
            json!({"type": "item_completed", "id": "msg_19c0000e100100000000000e01", "content": {"kind": "other", "provider_kind": "synthetic", "summary": "<subagent sessionID=\"ses_child\" state=\"completed\" description=\"Name a fruit\">\nKIWI\n</subagent>"}}),
            json!({"type": "turn_completed", "turn_id": "evt_19c000001700100000000000017", "status": "completed", "usage": null}),
        ]
    );
}

fn listed(mapper: &Mapper) -> Vec<(String, String, String, BackgroundKind)> {
    mapper
        .background()
        .into_iter()
        .map(|work| (work.id, work.item_id, work.name, work.kind))
        .collect()
}

/// A command in the background is listed under its shell's id until the
/// shell exits. A stop removes the shell where it runs, as OpenCode's own
/// app does; OpenCode then reports it exited `killed` (v2.3.0's
/// `shell.ts` `remove`), and the call ends interrupted.
#[test]
fn a_background_shell_stops_by_removal_and_ends_when_opencode_reports_it_killed() {
    let id = "msg_19c0000a100100000000000a01:call_0000000000000021";
    let shell = "sh_19c0000b100100000000000b01";
    let mut mapper = Mapper::new("ses_root".into(), Vec::new(), HashMap::new());
    let frames = frames("background_shell_turn.jsonl");
    let (exited, running) = frames.split_last().unwrap();
    assert_eq!(exited["type"], "shell.exited");
    for frame in running {
        mapper.on_event(frame).unwrap();
    }
    assert_eq!(
        listed(&mapper),
        [(
            shell.to_owned(),
            id.to_owned(),
            "make build".to_owned(),
            BackgroundKind::Command
        )]
    );
    assert_eq!(
        mapper.stop_background(Some(shell)),
        [Action::RemoveShell {
            shell_id: shell.into(),
            directory: Some("/work/project".into()),
        }]
    );
    let killed = json!({"id": "evt_19c000001000100000000000010", "created": 1_794_000_000_070_u64, "type": "shell.exited", "location": {"directory": "/work/project"}, "data": {"id": shell, "status": "killed"}});
    let mut out = feed(&mut mapper, &killed);
    assert!(listed(&mapper).is_empty());
    out.extend(wire(&mapper.shell_poll_ended(id)));
    assert_eq!(
        out,
        [
            json!({"action": "drain_shell", "item_id": id}),
            json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "make build", "output": "", "exit_code": null, "status": "interrupted"}}),
        ]
    );
}

/// A subagent is listed under its child session until it reports. A stop
/// interrupts that session, as OpenCode's own app does; OpenCode then
/// reports the subagent `cancelled` (v2.3.0's `execution.ts` and
/// `subagent-completion.ts`), and the call ends interrupted.
#[test]
fn a_subagent_is_stopped_by_interrupting_its_session_and_ends_with_its_cancelled_report() {
    let spawn = "msg_19c0000c100100000000000c01:call_0000000000000022";
    let mut mapper = Mapper::new("ses_root".into(), Vec::new(), HashMap::new());
    for frame in frames("subagent_report_turn.jsonl")
        .iter()
        .take_while(|frame| frame["type"] != "session.execution.succeeded")
    {
        mapper.on_event(frame).unwrap();
    }
    assert_eq!(
        listed(&mapper),
        [(
            "ses_child".to_owned(),
            spawn.to_owned(),
            "Name a fruit".to_owned(),
            BackgroundKind::Agent
        )]
    );
    assert_eq!(
        mapper.stop_background(None),
        [Action::InterruptSession {
            session_id: "ses_child".into(),
        }]
    );
    let report = json!({"id": "evt_19c000001600100000000000016", "created": 1_794_000_000_112_u64, "type": "session.inbox.enqueued", "data": {"inboxID": "msg_19c0000e100100000000000e01", "sessionID": "ses_root", "item": {"type": "synthetic", "payload": {"text": "<subagent sessionID=\"ses_child\" state=\"cancelled\" description=\"Name a fruit\">\nSubagent cancelled\n</subagent>", "description": "Name a fruit", "metadata": {"source": "subagent", "childID": "ses_child", "agent": "General", "state": "cancelled"}}, "delivery": "steer"}}, "durable": {"aggregateID": "ses_root", "seq": 7, "version": 1}});
    assert_eq!(
        feed(&mut mapper, &report),
        [
            json!({"type": "item_completed", "id": spawn, "content": {"kind": "subagent", "agent_type": "general", "description": "Name a fruit", "status": "interrupted", "summary": "Subagent cancelled"}}),
        ]
    );
    assert!(listed(&mapper).is_empty());
}

#[test]
fn an_image_read_carries_the_image_it_read() {
    let read = "msg_198ad176700100000000000062:chatcmpl-tool-0000000000000004";
    assert_eq!(
        items_only(replay("image_turn.jsonl", &[]))[..3],
        [
            json!({"type": "item_started", "id": read, "content": {"kind": "tool_call", "name": "read", "input": {}, "output": null, "status": "in_progress"}}),
            json!({"type": "item_updated", "id": read, "content": {"kind": "tool_call", "name": "read", "input": {"path": "blue.png"}, "output": null, "status": "in_progress", "family": {"type": "read", "path": "blue.png"}}}),
            json!({"type": "item_completed", "id": read, "content": {"kind": "tool_call", "image_reads": [{"media_type": "image/png", "data_base64": "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEElEQVR4nGNgYPiPAw0pCQCpcD/BFMrqcwAAAABJRU5ErkJggg=="}], "name": "read", "input": {"path": "blue.png"}, "output": "Image read successfully", "status": "completed", "family": {"type": "read", "path": "blue.png"}}}),
        ]
    );
}

/// The session's MCP servers are what `/api/mcp` lists; the recorded
/// call is of the `tcode_probe` server's `echo_upper`.
#[test]
fn an_mcp_call_names_its_server_and_tool_and_carries_its_text_and_images() {
    let call = "msg_198d2e71d00100000000000123:chatcmpl-tool-0000000000000007";
    let completed: Vec<Value> = replay("mcp_turn.jsonl", &["tcode_probe"])
        .into_iter()
        .filter(|out| out["type"] == "item_completed" && out["id"] == call)
        .collect();
    assert_eq!(
        completed,
        [
            json!({"type": "item_completed", "id": call, "content": {"kind": "tool_call", "image_reads": [{"media_type": "image/png", "data_base64": "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGP4z8AARwzEcQCukw/x0F8jngAAAABJRU5ErkJggg=="}], "name": "tcode_probe/echo_upper", "input": {"text": "hello"}, "output": "HELLO", "status": "completed"}})
        ]
    );
}

/// The approval card shows the command of the call that asks.
#[test]
fn a_permission_request_shows_the_command_it_would_run() {
    let mut mapper = Mapper::new("ses_root".into(), Vec::new(), HashMap::new());
    let mut asked = None;
    for frame in frames("permission_turn.jsonl") {
        if frame["type"] == "permission.asked" {
            asked = Some(mapper.approval_kind(&frame["data"]));
            break;
        }
        mapper.on_event(&frame).unwrap();
    }
    assert_eq!(
        serde_json::to_value(asked.unwrap()).unwrap(),
        json!({"kind": "exec_command", "command": "echo hi", "cwd": null})
    );
}

/// A question tool's form is a question per field, and the answer goes
/// back as OpenCode recorded it. The field names no `required`, which
/// OpenCode 2.3.0's `form.ts` reads as answerable empty.
#[test]
fn a_question_arrives_as_a_form_and_the_answer_is_what_opencode_recorded() {
    let frames = frames("question_turn.jsonl");
    let event = |kind: &str| &frames.iter().find(|frame| frame["type"] == kind).unwrap()["data"];
    let fields = event("form.created")["form"]["fields"].as_array().unwrap();
    assert_eq!(
        serde_json::to_value(fields.iter().filter_map(form_question).collect::<Vec<_>>()).unwrap(),
        json!([{
            "id": "q0", "header": "Preferred color", "question": "Which color do you prefer?",
            "options": [{"label": "Red", "description": "Prefer red"}, {"label": "Blue", "description": "Prefer blue"}],
            "multi_select": false, "optional": true,
        }])
    );
    let kinds: Vec<(String, String)> = fields
        .iter()
        .map(|field| {
            (
                field["key"].as_str().unwrap().to_owned(),
                field["type"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        Value::Object(form_answer(
            &kinds,
            &Map::from_iter([("q0".to_owned(), json!("Blue"))])
        )),
        event("form.replied")["answer"]
    );
}

#[test]
fn an_interrupted_call_and_turn_end_interrupted() {
    let id = "msg_198ae3e2d00100000000000084:chatcmpl-tool-0000000000000005";
    let out: Vec<Value> = replay("interrupt_turn.jsonl", &[])
        .into_iter()
        .filter(|out| out["type"].is_string())
        .collect();
    let running = out
        .iter()
        .position(|out| out["type"] == "item_updated" && out["content"]["command"] == "sleep 30")
        .unwrap();
    assert_eq!(
        out[running + 1..],
        [
            json!({"type": "item_completed", "id": id, "content": {"kind": "command_execution", "command": "sleep 30", "output": "", "exit_code": null, "status": "interrupted"}}),
            json!({"type": "turn_completed", "turn_id": "evt_198ae3e2700100000000000081", "status": "interrupted", "usage": null}),
        ]
    );
}

#[test]
fn a_compaction_reports_its_progress_in_a_turn_of_its_own() {
    assert_eq!(
        replay("compact_turn.jsonl", &[]),
        vec![
            json!({"type": "turn_started", "turn_id": "evt_198b05ab800200000000000003"}),
            json!({"type": "context_compacted", "in_progress": true, "trigger": "manual", "pre_tokens": null, "post_tokens": null, "dropped_tokens": null, "duration_ms": null}),
            json!({"type": "context_compacted", "in_progress": false, "trigger": "manual", "pre_tokens": null, "post_tokens": null, "dropped_tokens": null, "duration_ms": null}),
            json!({"type": "turn_completed", "turn_id": "evt_198b05ab800200000000000003", "status": "completed", "usage": null}),
        ]
    );
}

/// OpenCode 2.2.6 delivers a queued move in an execution of its own,
/// which is no turn; the prompt after it runs as an ordinary one.
#[test]
fn an_execution_that_only_moves_the_session_is_no_turn() {
    let text = "msg_1c19cb82100100000000000162:text:0";
    let turn_id = "evt_1c19cb81700100000000000159";
    assert_eq!(
        replay("move_turn.jsonl", &[]),
        vec![
            json!({"type": "turn_started", "turn_id": turn_id}),
            json!({"type": "served_model", "model": "opencode/iceman-free", "reason": null}),
            json!({"type": "item_started", "id": text, "content": {"kind": "assistant_message", "text": ""}}),
            json!({"type": "delta", "item_id": text, "kind": "assistant_text", "text": "PONG"}),
            json!({"type": "item_completed", "id": text, "content": {"kind": "assistant_message", "text": "PONG"}}),
            json!({"type": "token_usage", "freshness": "current", "turn_processed_tokens": null, "input_tokens": 4325, "cached_input_tokens": 4096, "output_tokens": 46, "used_tokens": 8467, "context_window": null, "total_processed_tokens": null, "cost_usd": null, "duration_ms": null}),
            json!({"type": "turn_completed", "turn_id": turn_id, "status": "completed", "usage": {"freshness": "current", "turn_processed_tokens": 8467, "input_tokens": 4325, "cached_input_tokens": 4096, "output_tokens": 46, "used_tokens": 8467, "context_window": null, "total_processed_tokens": null, "cost_usd": 0.0, "duration_ms": null}}),
        ]
    );
}

/// What `MoveTo` posts to `…/move`: queued behind what the session runs,
/// as the recorded move's inbox item was delivered.
#[test]
fn a_move_waits_in_the_inbox_behind_the_running_execution() {
    let enqueued = &frames("move_turn.jsonl")[0]["data"]["item"];
    let body = move_body(Path::new("/work/feature"));
    assert_eq!(body["delivery"], enqueued["delivery"]);
    assert_eq!(
        body["directory"],
        enqueued["payload"]["location"]["directory"]
    );
}

#[test]
fn tcode_mcp_servers_join_the_inline_config_as_tools_of_their_own() {
    let launch_env = LaunchEnv {
            env: vec![(
                "OPENCODE_CONFIG_CONTENT".into(),
                r#"{"experimental":{"todo":true},"mcp":{"servers":{"docs":{"type":"local","command":["docs-mcp"]}}}}"#.into(),
            )],
            home: None,
        };
    let preview = McpRegistration {
        name: McpRegistration::SERVER_NAME_PREVIEW.into(),
        url: "http://127.0.0.1:53211/mcp".into(),
        bearer_token: "token".into(),
    };
    let config: Value =
        serde_json::from_str(&config_content(&launch_env, &[&preview]).unwrap().unwrap()).unwrap();
    assert_eq!(
        config,
        json!({"experimental": {"todo": true}, "mcp": {"servers": {
            "docs": {"type": "local", "command": ["docs-mcp"]},
            "tcode_preview": {"type": "remote", "url": "http://127.0.0.1:53211/mcp", "headers": {"Authorization": "Bearer token"}, "codemode": false},
        }}})
    );
}

#[test]
fn the_model_catalog_offers_each_model_with_its_variants_in_opencodes_order() {
    let data =
        |name: &str| serde_json::from_str::<Value>(&fixture_text(name)).unwrap()["data"].take();
    let models = map_models(&data("model_list.json"), &data("model_default.json"));
    let effort = |variants: &[(&str, &str)]| json!([{"kind": "select", "id": "reasoningEffort", "label": "Thinking", "options": variants.iter().map(|(value, label)| json!({"value": value, "label": label, "description": null, "unavailable": null, "speed": null, "summary": null})).collect::<Vec<_>>(), "default_value": null, "role": "effort", "apply": "live", "recommended": null, "permissive": null}]);
    assert_eq!(
        serde_json::to_value(&models).unwrap(),
        json!([
            {"id": "openai/gpt-6.1-terra", "display_name": "GPT-6.1 Terra", "is_default": true, "options": effort(&[("none", "None"), ("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "Xhigh"), ("max", "Max")])},
            {"id": "opencode/big-pickle", "display_name": "Big Pickle", "is_default": false, "options": []},
        ])
    );
}
