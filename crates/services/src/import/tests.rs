use std::path::PathBuf;

use agent::{ItemContent, ItemStatus};
use serde_json::json;

use super::{ConvertedEntry, ConvertedThread, codex};

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
        context_len: None,
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
/// gives a call it dropped.
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
"#;

/// A rollout shows what Codex recorded: a local shell call shows the
/// command in its `action`, a call shows the output answered to its
/// `call_id`, and a call Codex dropped or never answered is interrupted.
#[test]
fn a_codex_rollout_imports_as_codex_recorded_it() {
    let rollout = Transcript::new(CODEX_ROLLOUT);
    let thread = codex::convert(&rollout.0, "codex:example").unwrap();
    assert_eq!(thread.first_user.as_deref(), Some("List the files"));
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
        ]
    );
}
