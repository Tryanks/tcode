use std::path::PathBuf;

use agent::{ItemContent, ItemStatus};
use serde_json::json;

use super::{ConvertedEntry, ConvertedThread, claude, codex};

/// A transcript written to a file of its own, removed with the value.
struct Transcript(PathBuf);

impl Transcript {
    fn new(lines: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tcode-import-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&path, lines).unwrap();
        Self(path)
    }
}

impl Drop for Transcript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A history directory the scan cannot read is reported with the reason,
/// beside what it could read, rather than looking like no history.
#[cfg(unix)]
#[test]
fn a_history_directory_the_scan_cannot_read_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("tcode-scan-{}", uuid::Uuid::new_v4()));
    let locked = root.join("projects/-work-locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let roots = super::ExternalRoots {
        claude_projects: Some(root.join("projects")),
        claude_desktop_meta: None,
        codex_session_roots: vec![root.join("codex/sessions")],
    };
    let history = super::scan_recent_dirs(&roots, &[]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(history.dirs.is_empty());
    let [unreadable] = &history.unreadable[..] else {
        panic!("{:?}", history.unreadable);
    };
    assert!(
        unreadable.starts_with(&locked.display().to_string()),
        "{unreadable}"
    );
}

/// What the thread shows, in order: each item's content, `None` for a
/// compaction.
fn shown(thread: ConvertedThread) -> Vec<Option<ItemContent>> {
    thread
        .entries
        .into_iter()
        .map(|entry| match entry {
            ConvertedEntry::Item { item, .. } => Some(item.content),
            ConvertedEntry::Compacted { .. } => None,
        })
        .collect()
}

fn user(text: &str) -> Option<ItemContent> {
    Some(ItemContent::UserMessage {
        text: text.into(),
        attachments: Vec::new(),
    })
}

fn tool(
    name: &str,
    input: serde_json::Value,
    output: Option<&str>,
    status: ItemStatus,
) -> Option<ItemContent> {
    Some(ItemContent::ToolCall {
        image_reads: Vec::new(),
        name: name.into(),
        input,
        output: output.map(str::to_owned),
        status,
    })
}

/// A rollout in Codex 0.167.0's shape, content invented. Each line is
/// `{timestamp, type, payload}` with the kinds Codex's own description of
/// its rollouts names (`session_meta`, `response_item`, `event_msg`); each
/// `response_item` payload is a variant of `ResponseItem` as
/// `codex app-server generate-ts` prints it for that release, and
/// `context_compacted` is an `EventMsg` variant of it. "aborted before
/// dispatch" is the text its tool dispatcher (`core/src/tools/parallel.rs`)
/// gives a call it dropped. `thread_rolled_back` is the `EventMsg` variant
/// (`ThreadRolledBackEvent { num_turns }`, `protocol/src/protocol.rs` at
/// `rust-v0.167.0`) that release keeps replaying from existing rollouts.
const CODEX_ROLLOUT: &str = r#"{"timestamp":"2026-01-02T03:04:05.000Z","type":"session_meta","payload":{"id":"00000000-0000-4000-8000-000000000001","timestamp":"2026-01-02T03:04:05.000Z","cwd":"/work/example","originator":"codex_cli_rs","cli_version":"0.167.0"}}
{"timestamp":"2026-01-02T03:04:07.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"List the files"}]}}
{"timestamp":"2026-01-02T03:04:08.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Listing first."}],"encrypted_content":null}}
{"timestamp":"2026-01-02T03:04:09.000Z","type":"response_item","payload":{"type":"local_shell_call","call_id":"call_local","status":"completed","action":{"type":"exec","command":["ls","-la"],"timeout_ms":null,"working_directory":null,"env":null,"user":null}}}
{"timestamp":"2026-01-02T03:04:10.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"cat\",\"README.md\"]}","call_id":"call_read"}}
{"timestamp":"2026-01-02T03:04:11.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_read","output":"Example readme"}}
{"timestamp":"2026-01-02T03:04:12.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"make\"]}","call_id":"call_dropped"}}
{"timestamp":"2026-01-02T03:04:13.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_dropped","output":"aborted before dispatch"}}
{"timestamp":"2026-01-02T03:04:14.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"make\",\"test\"]}","call_id":"call_open"}}
{"timestamp":"2026-01-02T03:04:15.000Z","type":"event_msg","payload":{"type":"context_compacted"}}
{"timestamp":"2026-01-02T03:04:16.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}
{"timestamp":"2026-01-02T03:04:17.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Delete the build"}]}}
{"timestamp":"2026-01-02T03:04:18.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"rm\",\"-r\",\"build\"]}","call_id":"call_rolled_back"}}
{"timestamp":"2026-01-02T03:04:19.000Z","type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}
{"timestamp":"2026-01-02T03:04:20.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Keep the build"}]}}
"#;

/// A rollout shows what Codex recorded: a local shell call shows the
/// command in its `action`, a call shows the output answered to its
/// `call_id`, a call Codex dropped or never answered is interrupted, and a
/// turn the user rolled back is gone.
#[test]
fn a_codex_rollout_imports_as_codex_recorded_it() {
    let rollout = Transcript::new(CODEX_ROLLOUT);
    let thread = codex::convert(&rollout.0, "codex:example").unwrap();
    assert_eq!(
        shown(thread),
        [
            user("List the files"),
            Some(ItemContent::Reasoning {
                text: "Listing first.".into(),
            }),
            Some(ItemContent::CommandExecution {
                command: "ls -la".into(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::Completed,
            }),
            tool(
                "shell",
                json!({"command": ["cat", "README.md"]}),
                Some("Example readme"),
                ItemStatus::Completed,
            ),
            tool(
                "shell",
                json!({"command": ["make"]}),
                Some("aborted before dispatch"),
                ItemStatus::Interrupted,
            ),
            tool(
                "shell",
                json!({"command": ["make", "test"]}),
                None,
                ItemStatus::Interrupted,
            ),
            None,
            Some(ItemContent::AssistantMessage {
                text: "Done.".into(),
                summarized: false,
            }),
            user("Keep the build"),
        ]
    );
}

/// A transcript in Claude Code 2.1.322's shape, content invented. Each line
/// is a message spread with `cwd`, `userType`, `entrypoint`, `version`,
/// `gitBranch`, `sessionId`, `timestamp`, `parentUuid` and `isSidechain`,
/// as that CLI writes a transcript line; `message` holds an Anthropic
/// Messages API message (text, `thinking`, `tool_use` and `tool_result`
/// blocks), and a compaction is a `system` line of subtype
/// `compact_boundary`. A slash command is the user text that CLI builds for
/// it (`<command-message>`, `<command-name>`, `<command-args>`), a prompt
/// command's expansion and a local command's caveat are meta lines, and a
/// local command's output is a `<local-command-stdout>` user text. Titles
/// are `ai-title` and `custom-title` records (`{type, aiTitle | customTitle,
/// sessionId}`) that CLI appends as they change.
const CLAUDE_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user","content":"Context the CLI adds for itself."},"isMeta":true,"uuid":"00000000-0000-4000-8000-000000000011","parentUuid":null,"isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:05.000Z"}
{"type":"user","message":{"role":"user","content":"Read the readme"},"uuid":"00000000-0000-4000-8000-000000000012","parentUuid":"00000000-0000-4000-8000-000000000011","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:06.000Z"}
{"type":"assistant","message":{"id":"msg_example_1","type":"message","role":"assistant","model":"claude-example","content":[{"type":"thinking","thinking":"Reading it.","signature":"c2lnbmF0dXJl"},{"type":"tool_use","id":"toolu_read","name":"Read","input":{"file_path":"/work/example/README.md"}}]},"uuid":"00000000-0000-4000-8000-000000000013","parentUuid":"00000000-0000-4000-8000-000000000012","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:07.000Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"Example readme"}]},"uuid":"00000000-0000-4000-8000-000000000014","parentUuid":"00000000-0000-4000-8000-000000000013","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:08.000Z"}
{"type":"assistant","message":{"id":"msg_example_2","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_make","name":"Bash","input":{"command":"make"}}]},"uuid":"00000000-0000-4000-8000-000000000015","parentUuid":"00000000-0000-4000-8000-000000000014","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:09.000Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_make","content":"make: no targets","is_error":true}]},"uuid":"00000000-0000-4000-8000-000000000016","parentUuid":"00000000-0000-4000-8000-000000000015","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.000Z"}
{"type":"ai-title","aiTitle":"Read and build the example","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"custom-title","customTitle":"Readme review","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"ai-title","aiTitle":"Review the example","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","uuid":"00000000-0000-4000-8000-000000000017","parentUuid":null,"isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:11.000Z"}
{"type":"assistant","message":{"id":"msg_example_3","type":"message","role":"assistant","model":"claude-example","content":[{"type":"text","text":"Done."}]},"uuid":"00000000-0000-4000-8000-000000000018","parentUuid":"00000000-0000-4000-8000-000000000017","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:12.000Z"}
{"type":"user","message":{"role":"user","content":"<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>src</command-args>"},"uuid":"00000000-0000-4000-8000-000000000019","parentUuid":"00000000-0000-4000-8000-000000000018","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:13.000Z"}
{"type":"user","message":{"role":"user","content":"Review the code in src."},"isMeta":true,"uuid":"00000000-0000-4000-8000-000000000020","parentUuid":"00000000-0000-4000-8000-000000000019","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:13.000Z"}
{"type":"user","message":{"role":"user","content":"<local-command-caveat>The command below was run directly in Claude Code.</local-command-caveat>"},"isMeta":true,"uuid":"00000000-0000-4000-8000-000000000021","parentUuid":"00000000-0000-4000-8000-000000000020","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:14.000Z"}
{"type":"user","message":{"role":"user","content":"<command-message>cost</command-message>\n<command-name>/cost</command-name>"},"uuid":"00000000-0000-4000-8000-000000000022","parentUuid":"00000000-0000-4000-8000-000000000021","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:14.000Z"}
{"type":"user","message":{"role":"user","content":"<local-command-stdout>Total cost: $0.00</local-command-stdout>"},"uuid":"00000000-0000-4000-8000-000000000023","parentUuid":"00000000-0000-4000-8000-000000000022","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:14.000Z"}
{"type":"assistant","message":{"id":"msg_example_4","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_test","name":"Bash","input":{"command":"make test"}}]},"uuid":"00000000-0000-4000-8000-000000000024","parentUuid":"00000000-0000-4000-8000-000000000023","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:15.000Z"}
"#;

/// A transcript shows what Claude Code recorded: a meta message is passed
/// over, a tool call shows the result answered to its id and fails when the
/// result is an error, a call never answered is interrupted, a compaction is
/// marked where it happened, and a slash
/// command shows as the command line the user typed, a local one with its
/// output. The thread is named as that CLI names it: a custom title before
/// a generated one.
#[test]
fn a_claude_transcript_imports_as_claude_recorded_it() {
    let transcript = Transcript::new(CLAUDE_TRANSCRIPT);
    let thread = claude::convert(&transcript.0, "claude:example").unwrap();
    assert_eq!(thread.title.as_deref(), Some("Readme review"));
    assert_eq!(
        shown(thread),
        [
            user("Read the readme"),
            Some(ItemContent::Reasoning {
                text: "Reading it.".into(),
            }),
            tool(
                "Read",
                json!({"file_path": "/work/example/README.md"}),
                Some("Example readme"),
                ItemStatus::Completed,
            ),
            tool(
                "Bash",
                json!({"command": "make"}),
                Some("make: no targets"),
                ItemStatus::Failed,
            ),
            None,
            Some(ItemContent::AssistantMessage {
                text: "Done.".into(),
                summarized: false,
            }),
            user("/review src"),
            user("/cost"),
            Some(ItemContent::Other {
                provider_kind: "local-command-stdout".into(),
                summary: "Total cost: $0.00".into(),
            }),
            tool(
                "Bash",
                json!({"command": "make test"}),
                None,
                ItemStatus::Interrupted,
            ),
        ]
    );
}
