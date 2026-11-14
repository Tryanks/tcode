use std::path::PathBuf;

use agent::{ItemContent, ItemStatus, PlanStepStatus};
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

/// A Codex rollout that does not start with the session_meta Codex reads its
/// thread from is reported as unreadable rather than left out unsaid.
#[test]
fn a_codex_rollout_without_its_session_meta_is_reported() {
    let home = std::env::temp_dir().join(format!("tcode-codex-{}", uuid::Uuid::new_v4()));
    let day = home.join("sessions").join("2026").join("01").join("02");
    std::fs::create_dir_all(&day).unwrap();
    let rollout =
        day.join("rollout-2026-01-02T03-04-05-00000000-0000-4000-8000-000000000041.jsonl");
    std::fs::write(&rollout, "{\"timestamp\":\"2026-01-02T03:04:05.000Z\",\n").unwrap();
    let history = super::scan_recent_dirs(
        &super::ExternalRoots {
            claude_projects: Vec::new(),
            claude_desktop_meta: None,
            codex_session_roots: vec![(None, home.join("sessions"))],
        },
        &[],
    );
    let _ = std::fs::remove_dir_all(&home);
    assert!(history.dirs.is_empty());
    let [unreadable] = &history.unreadable[..] else {
        panic!("{:?}", history.unreadable);
    };
    assert!(
        unreadable.starts_with(&rollout.display().to_string()),
        "{unreadable}"
    );
}

/// What the thread's timeline shows, in order: each item's content, `None`
/// for a compaction. A plan shows in the plan panel, not here.
fn shown(thread: ConvertedThread) -> Vec<Option<ItemContent>> {
    thread
        .entries
        .into_iter()
        .filter_map(|entry| match entry {
            ConvertedEntry::Item { item, .. } => Some(Some(item.content)),
            ConvertedEntry::Compacted { .. } => Some(None),
            ConvertedEntry::Plan { .. } => None,
        })
        .collect()
}

/// The plans the thread set, in order: each one's steps.
fn plans(thread: &ConvertedThread) -> Vec<Vec<(String, PlanStepStatus)>> {
    thread
        .entries
        .iter()
        .filter_map(|entry| match entry {
            ConvertedEntry::Plan { steps, .. } => Some(
                steps
                    .iter()
                    .map(|step| (step.step.clone(), step.status))
                    .collect(),
            ),
            _ => None,
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
        family: None,
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
/// `exec_command` and `apply_patch` are that release's tools
/// (`core/src/tools/handlers/shell_spec.rs`, `apply_patch_spec.rs`); a
/// command's output is worded by `ExecCommandToolOutput::response_header`
/// (`core/src/tools/context.rs`) or `format_exec_output_for_model`
/// (`core/src/tools/mod.rs`), a stopped command's by `abort_message`; a
/// legacy rollout keeps `patch_apply_end` (`PatchApplyEndEvent`,
/// `protocol/src/protocol.rs`), whose `changes` are `FileChange`s.
const CODEX_ROLLOUT: &str = r#"{"timestamp":"2026-01-02T03:04:05.000Z","type":"session_meta","payload":{"id":"00000000-0000-4000-8000-000000000001","timestamp":"2026-01-02T03:04:05.000Z","cwd":"/work/example","originator":"codex_cli_rs","cli_version":"0.167.0"}}
{"timestamp":"2026-01-02T03:04:07.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"List the files"}]}}
{"timestamp":"2026-01-02T03:04:08.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Listing first."}],"encrypted_content":null}}
{"timestamp":"2026-01-02T03:04:09.000Z","type":"response_item","payload":{"type":"local_shell_call","call_id":"call_local","status":"completed","action":{"type":"exec","command":["ls","-la"],"timeout_ms":null,"working_directory":null,"env":null,"user":null}}}
{"timestamp":"2026-01-02T03:04:10.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cat README.md\",\"workdir\":\"/work/example\"}","call_id":"call_read"}}
{"timestamp":"2026-01-02T03:04:11.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_read","output":"Chunk ID: 0a1b2c\nWall time: 0.0123 seconds\nProcess exited with code 0\nOriginal token count: 4\nOutput:\nExample readme\n"}}
{"timestamp":"2026-01-02T03:04:11.200Z","type":"response_item","payload":{"type":"function_call","name":"view_image","arguments":"{\"path\":\"/work/example/shot.png\"}","call_id":"call_view"}}
{"timestamp":"2026-01-02T03:04:11.300Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_view","output":[{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==","detail":"high"}]}}
{"timestamp":"2026-01-02T03:04:11.320Z","type":"response_item","payload":{"type":"function_call","name":"js","namespace":"mcp__cua_repl","arguments":"{\"code\":\"await app.click(3);\",\"title\":\"Press OK\"}","call_id":"call_script"}}
{"timestamp":"2026-01-02T03:04:11.330Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_script","output":[{"type":"input_text","text":"Wall time: 0.4200 seconds\nOutput:"},{"type":"input_text","text":"clicked OK"}]}}
{"timestamp":"2026-01-02T03:04:11.340Z","type":"response_item","payload":{"type":"tool_search_call","call_id":"call_tools","status":"completed","execution":"client","arguments":{"query":"calendar"}}}
{"timestamp":"2026-01-02T03:04:11.350Z","type":"response_item","payload":{"type":"tool_search_output","call_id":"call_tools","status":"completed","execution":"client","tools":[{"name":"mcp__calendar","tools":[{"name":"list_events"}]}]}}
{"timestamp":"2026-01-02T03:04:11.360Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"python3 -i\",\"yield_time_ms\":1000}","call_id":"call_repl"}}
{"timestamp":"2026-01-02T03:04:11.365Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_repl","output":"Chunk ID: 1a2b3c\nWall time: 1.0012 seconds\nProcess running with session ID 7\nOriginal token count: 2\nOutput:\n>>> "}}
{"timestamp":"2026-01-02T03:04:11.370Z","type":"response_item","payload":{"type":"function_call","name":"write_stdin","arguments":"{\"session_id\":7,\"chars\":\"print(1)\\n\"}","call_id":"call_stdin_1"}}
{"timestamp":"2026-01-02T03:04:11.375Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_stdin_1","output":"Chunk ID: 4d5e6f\nWall time: 0.5004 seconds\nProcess running with session ID 7\nOriginal token count: 3\nOutput:\n1\n>>> "}}
{"timestamp":"2026-01-02T03:04:11.380Z","type":"response_item","payload":{"type":"function_call","name":"write_stdin","arguments":"{\"session_id\":7,\"chars\":\"exit()\\n\"}","call_id":"call_stdin_2"}}
{"timestamp":"2026-01-02T03:04:11.385Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_stdin_2","output":"Chunk ID: 7a8b9c\nWall time: 0.2001 seconds\nProcess exited with code 0\nOriginal token count: 0\nOutput:\n"}}
{"timestamp":"2026-01-02T03:04:11.400Z","type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"explanation\":\"Lint next\",\"plan\":[{\"step\":\"Read the readme\",\"status\":\"completed\"},{\"step\":\"Lint\",\"status\":\"in_progress\"}]}","call_id":"call_plan"}}
{"timestamp":"2026-01-02T03:04:11.450Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_plan","output":"Plan updated"}}
{"timestamp":"2026-01-02T03:04:11.500Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"make lint\"}","call_id":"call_lint"}}
{"timestamp":"2026-01-02T03:04:11.600Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_lint","output":"Wall time: 1.2000 seconds\nProcess exited with code 2\nOutput:\nmake: *** No rule to make target 'lint'.  Stop.\n"}}
{"timestamp":"2026-01-02T03:04:11.700Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sleep 60\"}","call_id":"call_stopped"}}
{"timestamp":"2026-01-02T03:04:11.800Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_stopped","output":"Wall time: 4.0 seconds\naborted by user"}}
{"timestamp":"2026-01-02T03:04:11.900Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** Update File: README.md\n@@\n-Example readme\n+Example readme, revised\n*** End Patch\n"}}
{"timestamp":"2026-01-02T03:04:11.950Z","type":"event_msg","payload":{"type":"patch_apply_end","call_id":"call_patch","turn_id":"turn-1","stdout":"Success. Updated the following files:\nM README.md\n","stderr":"","success":true,"changes":{"/work/example/README.md":{"type":"update","unified_diff":"@@ -1 +1 @@\n-Example readme\n+Example readme, revised\n","move_path":null}},"status":"completed"}}
{"timestamp":"2026-01-02T03:04:11.990Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_patch","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nM README.md\n"}}
{"timestamp":"2026-01-02T03:04:12.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"make\"}","call_id":"call_dropped"}}
{"timestamp":"2026-01-02T03:04:13.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_dropped","output":"aborted before dispatch"}}
{"timestamp":"2026-01-02T03:04:14.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"make test\"}","call_id":"call_open"}}
{"timestamp":"2026-01-02T03:04:15.000Z","type":"event_msg","payload":{"type":"context_compacted"}}
{"timestamp":"2026-01-02T03:04:16.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}
{"timestamp":"2026-01-02T03:04:17.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Delete the build"}]}}
{"timestamp":"2026-01-02T03:04:18.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"rm -r build\"}","call_id":"call_rolled_back"}}
{"timestamp":"2026-01-02T03:04:19.000Z","type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}
{"timestamp":"2026-01-02T03:04:20.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Keep the build"}]}}
"#;

/// A cold rollout Codex compressed is listed and imports as its plain file
/// did; while the plain file is still there, Codex reads that one and the
/// compressed copy is not listed again. Codex compresses a rollout to one
/// zstd stream of its bytes, named after it with `.zst` (`compress_rollout_if_cold_blocking`
/// and `should_skip_compressed_sibling`, `rollout/src/compression.rs` at
/// `rust-v0.167.0`); the name is `rollout_file_name.rs`'s.
#[test]
fn a_rollout_codex_compressed_is_listed_and_imports_as_it_was_written() {
    let home = std::env::temp_dir().join(format!("tcode-codex-{}", uuid::Uuid::new_v4()));
    let day = home.join("sessions").join("2026").join("01").join("02");
    std::fs::create_dir_all(&day).unwrap();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    // The directory as a JSON string holds it, without the quotes.
    let in_json = serde_json::to_string(cwd.to_str().unwrap()).unwrap();
    let rollout = CODEX_ROLLOUT.replace("/work/example", in_json.trim_matches('"'));
    let name = "rollout-2026-01-02T03-04-05-00000000-0000-4000-8000-000000000001.jsonl";
    let compressed = day.join(format!("{name}.zst"));
    std::fs::write(
        &compressed,
        zstd::stream::encode_all(rollout.as_bytes(), 3).unwrap(),
    )
    .unwrap();
    let roots = super::ExternalRoots {
        claude_projects: Vec::new(),
        claude_desktop_meta: None,
        codex_session_roots: vec![
            (None, home.join("sessions")),
            (None, home.join("archived_sessions")),
        ],
    };
    let listed = |roots: &super::ExternalRoots| {
        let history = super::scan_recent_dirs(roots, &[]);
        assert!(history.unreadable.is_empty(), "{:?}", history.unreadable);
        history
            .dirs
            .into_iter()
            .flat_map(|dir| dir.threads)
            .map(|thread| thread.file)
            .collect::<Vec<_>>()
    };
    assert_eq!(listed(&roots), std::slice::from_ref(&compressed));
    let plain = Transcript::new(&rollout);
    let from_plain = shown(codex::convert(&plain.0, "codex:example").unwrap());
    assert_eq!(
        shown(codex::convert(&compressed, "codex:example").unwrap()),
        from_plain
    );

    std::fs::write(day.join(name), &rollout).unwrap();
    assert_eq!(listed(&roots), [day.join(name)]);
    let _ = std::fs::remove_dir_all(&home);
}

/// A thread found in a profile's config directory is listed as that
/// profile's, so it imports under the account it ran with; one found in the
/// built-in directory names no profile.
#[test]
fn a_scanned_thread_belongs_to_the_profile_whose_directory_holds_it() {
    let home = std::env::temp_dir().join(format!("tcode-profiles-{}", uuid::Uuid::new_v4()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let in_json = serde_json::to_string(cwd.to_str().unwrap()).unwrap();
    let at_cwd = |text: &str| {
        let first = text.lines().next().unwrap();
        first.replace("/work/example", in_json.trim_matches('"'))
    };
    let claude_projects = home.join("claude-work").join("projects");
    std::fs::create_dir_all(claude_projects.join("-project")).unwrap();
    std::fs::write(
        claude_projects
            .join("-project")
            .join("00000000-0000-4000-8000-000000000010.jsonl"),
        at_cwd(CLAUDE_TRANSCRIPT),
    )
    .unwrap();
    let codex_day = |root: &str| home.join(root).join("sessions/2026/01/02");
    let rollout = "rollout-2026-01-02T03-04-05-00000000-0000-4000-8000-000000000001.jsonl";
    std::fs::create_dir_all(codex_day("codex-personal")).unwrap();
    std::fs::write(
        codex_day("codex-personal").join(rollout),
        at_cwd(CODEX_ROLLOUT),
    )
    .unwrap();
    let builtin = at_cwd(CODEX_ROLLOUT).replace(
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    );
    std::fs::create_dir_all(codex_day("codex")).unwrap();
    std::fs::write(
        codex_day("codex").join(rollout.replace("0001.jsonl", "0002.jsonl")),
        builtin,
    )
    .unwrap();

    let history = super::scan_recent_dirs(
        &super::ExternalRoots {
            claude_projects: vec![(Some("work".into()), claude_projects)],
            claude_desktop_meta: None,
            codex_session_roots: vec![
                (
                    Some("personal".into()),
                    home.join("codex-personal/sessions"),
                ),
                (None, home.join("codex/sessions")),
            ],
        },
        &[],
    );
    let _ = std::fs::remove_dir_all(&home);
    assert!(history.unreadable.is_empty(), "{:?}", history.unreadable);
    let mut profiles = history
        .dirs
        .into_iter()
        .flat_map(|dir| dir.threads)
        .map(|thread| (thread.external_id, thread.profile_id))
        .collect::<Vec<_>>();
    profiles.sort();
    assert_eq!(
        profiles,
        [
            (
                "claude:00000000-0000-4000-8000-000000000010".to_owned(),
                Some("work".to_owned())
            ),
            (
                "codex:00000000-0000-4000-8000-000000000001".to_owned(),
                Some("personal".to_owned())
            ),
            (
                "codex:00000000-0000-4000-8000-000000000002".to_owned(),
                None
            ),
        ]
    );
}

/// A thread `thread/revert` rewound is listed once, by the rollout Codex now
/// reads for it, and shows the history Codex assembles for it: what it kept
/// of the rollout it continues, then what followed the revert. Shapes from
/// `rust-v0.167.0`: a reverted rollout is named `rollout-<time>-<thread
/// id>_<rollout id>.jsonl` (`rollout_file_name.rs`) and its `session_meta`
/// carries `history_base` {`thread_id`: the rollout id of the rollout it
/// continues, `end_ordinal_exclusive`, `end_byte_offset`}
/// (`HistoryPosition`, `protocol/src/protocol.rs`); a paginated rollout's
/// records carry their `ordinal` (`decode_rollout_line`, `rollout/src/lib.rs`).
/// The continued rollout is archived and compressed here, which Codex looks
/// through too (`find_rollout_path_by_rollout_id`). Content invented.
#[test]
fn a_reverted_codex_thread_shows_the_history_it_kept_and_what_followed() {
    let home = std::env::temp_dir().join(format!("tcode-codex-{}", uuid::Uuid::new_v4()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let thread = "00000000-0000-7000-8000-000000000031";
    let meta = |ordinal: u64, time: &str, base: &str| {
        format!(
            r#"{{"timestamp":"{time}","ordinal":{ordinal},"type":"session_meta","payload":{{"id":"{thread}","timestamp":"{time}","cwd":{},"originator":"codex_cli_rs","cli_version":"0.167.0","history_mode":"paginated"{base}}}}}"#,
            serde_json::to_string(cwd.to_str().unwrap()).unwrap()
        )
    };
    let message = |ordinal: u64, time: &str, role: &str, kind: &str, text: &str| {
        format!(
            r#"{{"timestamp":"{time}","ordinal":{ordinal},"type":"response_item","payload":{{"type":"message","role":"{role}","content":[{{"type":"{kind}","text":"{text}"}}]}}}}"#
        )
    };
    let kept = [
        meta(0, "2026-01-02T03:04:05.000Z", ""),
        message(
            1,
            "2026-01-02T03:04:06.000Z",
            "user",
            "input_text",
            "Write the plan",
        ),
        message(
            2,
            "2026-01-02T03:04:07.000Z",
            "assistant",
            "output_text",
            "Planned.",
        ),
    ]
    .map(|line| line + "\n")
    .concat();
    let original = kept.clone()
        + &[
            message(
                3,
                "2026-01-02T03:04:08.000Z",
                "user",
                "input_text",
                "Now build it",
            ),
            message(
                4,
                "2026-01-02T03:04:09.000Z",
                "assistant",
                "output_text",
                "Built.",
            ),
        ]
        .map(|line| line + "\n")
        .concat();
    let archived = home.join("archived_sessions");
    std::fs::create_dir_all(&archived).unwrap();
    std::fs::write(
        archived.join(format!("rollout-2026-01-02T03-04-05-{thread}.jsonl.zst")),
        zstd::stream::encode_all(original.as_bytes(), 3).unwrap(),
    )
    .unwrap();
    let base = format!(
        r#","history_base":{{"thread_id":"{thread}","end_ordinal_exclusive":3,"end_byte_offset":{}}}"#,
        kept.len()
    );
    let day = home.join("sessions").join("2026").join("01").join("03");
    std::fs::create_dir_all(&day).unwrap();
    let reverted = day.join(format!(
        "rollout-2026-01-03T04-05-06-{thread}_00000000-0000-7000-8000-000000000032.jsonl"
    ));
    std::fs::write(
        &reverted,
        [
            meta(3, "2026-01-03T04:05:06.000Z", &base),
            message(
                4,
                "2026-01-03T04:05:07.000Z",
                "user",
                "input_text",
                "Build it smaller",
            ),
            message(
                5,
                "2026-01-03T04:05:08.000Z",
                "assistant",
                "output_text",
                "Built smaller.",
            ),
        ]
        .map(|line| line + "\n")
        .concat(),
    )
    .unwrap();

    let history = super::scan_recent_dirs(
        &super::ExternalRoots {
            claude_projects: Vec::new(),
            claude_desktop_meta: None,
            codex_session_roots: vec![(None, home.join("sessions")), (None, archived)],
        },
        &[],
    );
    let listed: Vec<_> = history
        .dirs
        .into_iter()
        .flat_map(|dir| dir.threads)
        .map(|thread| thread.file)
        .collect();
    assert_eq!(listed, std::slice::from_ref(&reverted));
    let assistant = |text: &str| {
        Some(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    };
    assert_eq!(
        shown(codex::convert(&reverted, "codex:example").unwrap()),
        [
            user("Write the plan"),
            assistant("Planned."),
            user("Build it smaller"),
            assistant("Built smaller."),
        ]
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// A fork attached to its parent and made while the parent's turn ran is
/// listed as Codex lists threads, which is not at all, and a thread that
/// continues it shows what Codex shows of that history: the attached fork's
/// own conversation, without the parent's history it continues or the
/// records it opens with, the interrupted-turn guidance and the parent
/// turn's abort, and then its own. Shapes from `rust-v0.167.0`: a fork's
/// `session_meta` carries `forked_from_id`, `attached_thread`,
/// `forked_from_ordinal_exclusive` and `history_base` (`SessionMeta`,
/// `protocol/src/protocol.rs`); a mid-turn fork opens with
/// `thread_settings_applied`, the `<turn_aborted>` guidance as a user
/// message (`core/src/context/turn_aborted.rs`) and `turn_aborted`
/// (`append_interrupted_boundary`, `core/src/thread_manager.rs`); Codex's
/// thread list passes over an `attached_thread` (`build_thread_item`,
/// `rollout/src/list.rs`), and its history page stops at an attached fork
/// made at its parent's bookmark, wherever that is in the lineage, and
/// starts after its opening (`attached_history_start`, thread-store
/// `local/rollout_lineage.rs`). Settings fields Tcode does not read are left
/// out; content invented.
#[test]
fn an_attached_mid_turn_fork_is_not_listed_and_a_fork_of_it_shows_its_own_conversation() {
    let home = std::env::temp_dir().join(format!("tcode-codex-{}", uuid::Uuid::new_v4()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_json = serde_json::to_string(cwd.to_str().unwrap()).unwrap();
    let day = home.join("sessions").join("2026").join("01").join("02");
    std::fs::create_dir_all(&day).unwrap();
    let parent = "00000000-0000-7000-8000-000000000041";
    let fork = "00000000-0000-7000-8000-000000000042";
    let fork_of_fork = "00000000-0000-7000-8000-000000000043";
    let record = |ordinal: u64, time: &str, kind: &str, payload: &str| {
        format!(
            r#"{{"timestamp":"2026-01-02T03:{time}.000Z","ordinal":{ordinal},"type":"{kind}","payload":{payload}}}"#
        )
    };
    let meta = |ordinal: u64, time: &str, id: &str, fork_of: &str| {
        record(
            ordinal,
            time,
            "session_meta",
            &format!(
                r#"{{"id":"{id}","timestamp":"2026-01-02T03:{time}.000Z","cwd":{cwd_json},"originator":"codex_cli_rs","cli_version":"0.167.0","history_mode":"paginated"{fork_of}}}"#
            ),
        )
    };
    let message = |ordinal: u64, time: &str, role: &str, kind: &str, text: &str| {
        record(
            ordinal,
            time,
            "response_item",
            &format!(
                r#"{{"type":"message","role":"{role}","content":[{{"type":"{kind}","text":"{text}"}}]}}"#
            ),
        )
    };
    let taken = [
        meta(0, "04:05", parent, ""),
        message(1, "04:06", "user", "input_text", "Plan the change"),
        message(2, "04:07", "assistant", "output_text", "Planning it."),
    ]
    .map(|line| line + "\n")
    .concat();
    let parent_rollout = day.join(format!("rollout-2026-01-02T03-04-05-{parent}.jsonl"));
    std::fs::write(
        &parent_rollout,
        taken.clone() + &message(3, "04:08", "assistant", "output_text", "Still planning.") + "\n",
    )
    .unwrap();
    let fork_of = format!(
        r#","forked_from_id":"{parent}","attached_thread":true,"forked_from_ordinal_exclusive":3,"history_base":{{"thread_id":"{parent}","end_ordinal_exclusive":3,"end_byte_offset":{}}}"#,
        taken.len()
    );
    let forked = [

            meta(3, "05:00", fork, &fork_of),
            record(
                4,
                "05:00",
                "event_msg",
                r#"{"type":"thread_settings_applied","thread_settings":{"model":"gpt-5.5","model_provider_id":"openai","cwd":"/work/example"}}"#,
            ),
            message(
                5,
                "05:00",
                "user",
                "input_text",
                "<turn_aborted>\\nThe user interrupted the previous turn on purpose.\\n</turn_aborted>",
            ),
            record(
                6,
                "05:00",
                "event_msg",
                r#"{"type":"turn_aborted","turn_id":"turn-1","reason":"interrupted"}"#,
            ),
            message(7, "05:01", "user", "input_text", "Only the tests"),
            message(8, "05:02", "assistant", "output_text", "Tests only."),
        ]
        .map(|line| line + "\n")
        .concat();
    std::fs::write(
        day.join(format!("rollout-2026-01-02T03-05-00-{fork}.jsonl")),
        &forked,
    )
    .unwrap();
    let fork_of_fork_of = format!(
        r#","forked_from_id":"{fork}","forked_from_ordinal_exclusive":9,"history_base":{{"thread_id":"{fork}","end_ordinal_exclusive":9,"end_byte_offset":{}}}"#,
        forked.len()
    );
    let continued = day.join(format!("rollout-2026-01-02T03-06-00-{fork_of_fork}.jsonl"));
    std::fs::write(
        &continued,
        [
            meta(9, "06:00", fork_of_fork, &fork_of_fork_of),
            message(10, "06:01", "user", "input_text", "And the docs"),
            message(11, "06:02", "assistant", "output_text", "Docs too."),
        ]
        .map(|line| line + "\n")
        .concat(),
    )
    .unwrap();

    let history = super::scan_recent_dirs(
        &super::ExternalRoots {
            claude_projects: Vec::new(),
            claude_desktop_meta: None,
            codex_session_roots: vec![(None, home.join("sessions"))],
        },
        &[],
    );
    let mut listed: Vec<_> = history
        .dirs
        .into_iter()
        .flat_map(|dir| dir.threads)
        .map(|thread| thread.file)
        .collect();
    listed.sort();
    assert_eq!(listed, [parent_rollout, continued.clone()]);
    let assistant = |text: &str| {
        Some(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    };
    assert_eq!(
        shown(codex::convert(&continued, "codex:example").unwrap()),
        [
            user("Only the tests"),
            assistant("Tests only."),
            user("And the docs"),
            assistant("Docs too."),
        ]
    );
    let _ = std::fs::remove_dir_all(&home);
}

fn command(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    status: ItemStatus,
) -> Option<ItemContent> {
    Some(ItemContent::CommandExecution {
        command: command.into(),
        input: Vec::new(),
        output: output.into(),
        exit_code,
        status,
        changes: Vec::new(),
    })
}

fn readme_revised(status: ItemStatus) -> Option<ItemContent> {
    Some(ItemContent::FileChange {
        changes: vec![agent::FileChange {
            path: "/work/example/README.md".into(),
            kind: agent::FileChangeKind::Modify,
            diff: Some("@@ -1 +1 @@\n-Example readme\n+Example readme, revised\n".into()),
        }],
        status,
        output: Some("Success. Updated the following files:\nM README.md\n".into()),
    })
}

/// A rollout shows what Codex recorded: a local shell call shows the
/// command in its `action`, a command shows what it printed and the code
/// it exited with, failing when that is not 0 as Codex judges it, an image
/// Codex was shown is that image as Codex reports it live, an MCP call is
/// what its server returned, without the `Wall time` header Codex writes
/// before it for the model (`McpToolOutput::response_payload`,
/// `core/src/tools/context.rs`), an
/// command still running when its call returned goes on through the
/// `write_stdin` calls to its session (`Process running with session ID N`,
/// `shell_spec.rs`), whose output is its own and whose `chars` are the
/// lines typed into it, until one sees it exit, an
/// `update_plan` is the plan it sets (`UpdatePlanArgs`,
/// `protocol/src/plan_tool.rs`), as live, a patch
/// shows the changes Codex applied, a call Codex dropped, stopped or never
/// answered is interrupted, and a turn the user rolled back is gone.
#[test]
fn a_codex_rollout_imports_as_codex_recorded_it() {
    let rollout = Transcript::new(CODEX_ROLLOUT);
    let thread = codex::convert(&rollout.0, "codex:example").unwrap();
    assert_eq!(
        plans(&thread),
        [vec![
            ("Read the readme".to_owned(), PlanStepStatus::Completed),
            ("Lint".to_owned(), PlanStepStatus::InProgress),
        ]]
    );
    assert_eq!(
        shown(thread),
        [
            user("List the files"),
            Some(ItemContent::Reasoning {
                text: "Listing first.".into(),
            }),
            command("ls -la", "", None, ItemStatus::Completed),
            command(
                "cat README.md",
                "Example readme\n",
                Some(0),
                ItemStatus::Completed
            ),
            Some(ItemContent::ImageRead {
                path: "/work/example/shot.png".into(),
                image: Some(agent::Attachment {
                    media_type: "image/png".into(),
                    data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==".into(),
                    source_path: None,
                }),
            }),
            tool(
                "cua_repl/js",
                json!({"code": "await app.click(3);", "title": "Press OK"}),
                Some("clicked OK"),
                ItemStatus::Completed,
            ),
            Some(ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "tool_search".into(),
                input: json!({"query": "calendar"}),
                output: Some("calendar/list_events".into()),
                status: ItemStatus::Completed,
                family: Some(Box::new(agent::ToolFamily::LoadTools {
                    query: "calendar".into(),
                    tools: Some(vec!["calendar/list_events".into()]),
                })),
            }),
            Some(ItemContent::CommandExecution {
                command: "python3 -i".into(),
                input: vec!["print(1)".into(), "exit()".into()],
                output: ">>> 1\n>>> ".into(),
                exit_code: Some(0),
                status: ItemStatus::Completed,
                changes: Vec::new(),
            }),
            command(
                "make lint",
                "make: *** No rule to make target 'lint'.  Stop.\n",
                Some(2),
                ItemStatus::Failed,
            ),
            command("sleep 60", "", None, ItemStatus::Interrupted),
            readme_revised(ItemStatus::Completed),
            command("make", "", None, ItemStatus::Interrupted),
            command("make test", "", None, ItemStatus::Interrupted),
            None,
            Some(ItemContent::AssistantMessage {
                text: "Done.".into(),
                summarized: false,
            }),
            user("Keep the build"),
        ]
    );
}

/// A rollout of a thread whose history Codex 0.167.0 keeps paginated
/// (`session_meta.history_mode`, `ThreadHistoryMode` in
/// `protocol/src/protocol.rs`), content invented. Such a rollout also keeps
/// every turn item Codex completed as an `item_completed` event
/// (`persisted_item_completed_event`, `rollout/src/policy.rs`): a
/// `CommandExecution` or `FileChange` (`protocol/src/items.rs`) whose `id` is
/// the id of the call it ran for, or a command of its own, such as one a
/// code-mode `exec` script ran.
const CODEX_PAGINATED_ROLLOUT: &str = r#"{"timestamp":"2026-01-02T03:04:05.000Z","type":"session_meta","payload":{"id":"00000000-0000-4000-8000-000000000002","timestamp":"2026-01-02T03:04:05.000Z","cwd":"/work/example","originator":"codex_cli_rs","cli_version":"0.167.0","history_mode":"paginated"}}
{"timestamp":"2026-01-02T03:04:06.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Test it"}]}}
{"timestamp":"2026-01-02T03:04:07.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"make test\"}","call_id":"call_test"}}
{"timestamp":"2026-01-02T03:04:08.000Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"00000000-0000-4000-8000-000000000002","turn_id":"turn-1","item":{"type":"CommandExecution","id":"call_test","command":["/bin/zsh","-lc","make test"],"cwd":"/work/example","parsed_cmd":[],"source":"agent","status":"failed","aggregated_output":"1 failed\n","exit_code":1,"duration":{"secs":0,"nanos":500000000}},"started_at_ms":1767323047000,"completed_at_ms":1767323048000}}
{"timestamp":"2026-01-02T03:04:08.100Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_test","output":"Wall time: 0.5000 seconds\nProcess exited with code 1\nOutput:\n1 failed\n"}}
{"timestamp":"2026-01-02T03:04:09.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_script","name":"exec","input":"await tools.exec_command({cmd: 'git status --short'})"}}
{"timestamp":"2026-01-02T03:04:10.000Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"00000000-0000-4000-8000-000000000002","turn_id":"turn-1","item":{"type":"CommandExecution","id":"call_inner","command":["/bin/zsh","-lc","git status --short"],"cwd":"/work/example","parsed_cmd":[],"source":"agent","status":"completed","aggregated_output":" M README.md\n","exit_code":0,"duration":{"secs":0,"nanos":10000000}},"started_at_ms":1767323049000,"completed_at_ms":1767323050000}}
{"timestamp":"2026-01-02T03:04:10.100Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_script","output":[{"type":"input_text","text":" M README.md\n"}]}}
{"timestamp":"2026-01-02T03:04:11.000Z","type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** Update File: README.md\n@@\n-Example readme\n+Example readme, revised\n*** End Patch\n"}}
{"timestamp":"2026-01-02T03:04:12.000Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"00000000-0000-4000-8000-000000000002","turn_id":"turn-1","item":{"type":"FileChange","id":"call_patch","changes":{"/work/example/README.md":{"type":"update","unified_diff":"@@ -1 +1 @@\n-Example readme\n+Example readme, revised\n","move_path":null}},"status":"completed","stdout":"Success. Updated the following files:\nM README.md\n","stderr":""},"started_at_ms":1767323051000,"completed_at_ms":1767323052000}}
{"timestamp":"2026-01-02T03:04:12.100Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_patch","output":"Exit code: 0\nWall time: 0.1 seconds\nOutput:\nSuccess. Updated the following files:\nM README.md\n"}}
"#;

/// A paginated rollout shows each command and patch as Codex's own item
/// records it, in the place of the call it ran for, and a command a script
/// ran where the script ran it.
#[test]
fn a_paginated_codex_rollout_shows_the_items_codex_recorded() {
    let rollout = Transcript::new(CODEX_PAGINATED_ROLLOUT);
    let thread = codex::convert(&rollout.0, "codex:example").unwrap();
    assert_eq!(
        shown(thread),
        [
            user("Test it"),
            command("make test", "1 failed\n", Some(1), ItemStatus::Failed),
            tool(
                "exec",
                json!("await tools.exec_command({cmd: 'git status --short'})"),
                Some(" M README.md\n"),
                ItemStatus::Completed,
            ),
            command(
                "git status --short",
                " M README.md\n",
                Some(0),
                ItemStatus::Completed
            ),
            readme_revised(ItemStatus::Completed),
        ]
    );
}

/// A transcript in Claude Code 2.1.322's shape, content invented. Each line
/// is a message spread with `cwd`, `userType`, `entrypoint`, `version`,
/// `gitBranch`, `sessionId`, `timestamp`, `parentUuid` and `isSidechain`,
/// as that CLI writes a transcript line; `message` holds an Anthropic
/// Messages API message (text, `thinking`, `tool_use` and `tool_result`
/// blocks), and a compaction is a `system` line of subtype
/// `compact_boundary` that starts a new chain (`parentUuid` null) and names
/// the message before it as its `logicalParentUuid`. A slash command is the user text that CLI builds for
/// it (`<command-message>`, `<command-name>`, `<command-args>`), a prompt
/// command's expansion and a local command's caveat are meta lines, and a
/// local command's output is a `<local-command-stdout>` user text. Titles
/// are `ai-title` and `custom-title` records (`{type, aiTitle | customTitle,
/// sessionId}`) that CLI appends as they change.
const CLAUDE_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user","content":"Context the CLI adds for itself."},"isMeta":true,"uuid":"00000000-0000-4000-8000-000000000011","parentUuid":null,"isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:05.000Z"}
{"type":"user","message":{"role":"user","content":"Read the readme"},"uuid":"00000000-0000-4000-8000-000000000012","parentUuid":"00000000-0000-4000-8000-000000000011","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:06.000Z"}
{"type":"assistant","message":{"id":"msg_example_1","type":"message","role":"assistant","model":"claude-example","content":[{"type":"thinking","thinking":"Reading it.","signature":"c2lnbmF0dXJl"},{"type":"tool_use","id":"toolu_read","name":"Read","input":{"file_path":"/work/example/README.md"}}]},"uuid":"00000000-0000-4000-8000-000000000013","parentUuid":"00000000-0000-4000-8000-000000000012","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:07.000Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_read","content":"1\tExample readme"}]},"toolUseResult":{"type":"text","file":{"filePath":"/work/example/README.md","content":"Example readme","numLines":1,"startLine":1,"totalLines":1}},"uuid":"00000000-0000-4000-8000-000000000014","parentUuid":"00000000-0000-4000-8000-000000000013","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:08.000Z"}
{"type":"assistant","message":{"id":"msg_example_2","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_make","name":"Bash","input":{"command":"make"}}]},"uuid":"00000000-0000-4000-8000-000000000015","parentUuid":"00000000-0000-4000-8000-000000000014","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:09.000Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_make","content":"Exit code 2\nmake: *** No targets.  Stop.","is_error":true}]},"uuid":"00000000-0000-4000-8000-000000000016","parentUuid":"00000000-0000-4000-8000-000000000015","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.000Z"}
{"type":"assistant","message":{"id":"msg_example_5","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_edit","name":"Edit","input":{"file_path":"/work/example/README.md","old_string":"Example readme","new_string":"Example readme, revised","replace_all":false}}]},"uuid":"00000000-0000-4000-8000-000000000025","parentUuid":"00000000-0000-4000-8000-000000000016","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.500Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_edit","content":"The file /work/example/README.md has been updated successfully."}]},"toolUseResult":{"filePath":"/work/example/README.md","oldString":"Example readme","newString":"Example readme, revised","originalFile":"Example readme\n","structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-Example readme","+Example readme, revised"]}],"userModified":false,"replaceAll":false},"uuid":"00000000-0000-4000-8000-000000000026","parentUuid":"00000000-0000-4000-8000-000000000025","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.700Z"}
{"type":"assistant","message":{"id":"msg_example_6","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_plan","name":"ExitPlanMode","input":{"plan":"Revise the readme:\n\n1. Reword it.","planFilePath":"/work/example/.claude/plan.md"}}]},"uuid":"00000000-0000-4000-8000-000000000027","parentUuid":"00000000-0000-4000-8000-000000000026","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.800Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_plan","content":"User has approved your plan."}]},"uuid":"00000000-0000-4000-8000-000000000028","parentUuid":"00000000-0000-4000-8000-000000000027","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.900Z"}
{"type":"assistant","message":{"id":"msg_example_7","type":"message","role":"assistant","model":"claude-example","content":[{"type":"tool_use","id":"toolu_todo","name":"TodoWrite","input":{"todos":[{"content":"Reword the readme","status":"completed","activeForm":"Rewording the readme"},{"content":"Build it","status":"in_progress","activeForm":"Building it"}]}}]},"uuid":"00000000-0000-4000-8000-000000000029","parentUuid":"00000000-0000-4000-8000-000000000028","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.950Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_todo","content":"Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable"}]},"toolUseResult":{"oldTodos":[],"newTodos":[{"content":"Reword the readme","status":"completed","activeForm":"Rewording the readme"},{"content":"Build it","status":"in_progress","activeForm":"Building it"}]},"uuid":"00000000-0000-4000-8000-000000000030","parentUuid":"00000000-0000-4000-8000-000000000029","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:10.960Z"}
{"type":"ai-title","aiTitle":"Read and build the example","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"custom-title","customTitle":"Readme review","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"ai-title","aiTitle":"Review the example","sessionId":"00000000-0000-4000-8000-000000000010"}
{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","uuid":"00000000-0000-4000-8000-000000000017","parentUuid":null,"logicalParentUuid":"00000000-0000-4000-8000-000000000030","isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:11.000Z"}
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
/// output. A TodoWrite is the plan it sets, as a live session shows it.
/// The thread is named as that CLI names it: a custom title before a
/// generated one.
#[test]
fn a_claude_transcript_imports_as_claude_recorded_it() {
    let transcript = Transcript::new(CLAUDE_TRANSCRIPT);
    let thread = claude::convert(&transcript.0, "claude:example").unwrap();
    assert_eq!(thread.title.as_deref(), Some("Readme review"));
    assert_eq!(
        plans(&thread),
        [vec![
            ("Reword the readme".to_owned(), PlanStepStatus::Completed),
            ("Build it".to_owned(), PlanStepStatus::InProgress),
        ]]
    );
    assert_eq!(
        shown(thread),
        [
            user("Read the readme"),
            Some(ItemContent::Reasoning {
                text: "Reading it.".into(),
            }),
            Some(ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "Read".into(),
                input: json!({"file_path": "/work/example/README.md"}),
                output: Some("Example readme".into()),
                status: ItemStatus::Completed,
                family: Some(Box::new(agent::ToolFamily::Read {
                    path: "/work/example/README.md".into(),
                    first_line: Some(1),
                    note: None,
                })),
            }),
            Some(ItemContent::CommandExecution {
                command: "make".into(),
                input: Vec::new(),
                output: "make: *** No targets.  Stop.".into(),
                exit_code: Some(2),
                status: ItemStatus::Failed,
                changes: Vec::new(),
            }),
            Some(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "/work/example/README.md".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some("@@ -1,1 +1,1 @@\n-Example readme\n+Example readme, revised".into()),
                }],
                status: ItemStatus::Completed,
                output: Some(
                    "The file /work/example/README.md has been updated successfully.".into()
                ),
            }),
            Some(ItemContent::Plan {
                markdown: "Revise the readme:\n\n1. Reword it.".into(),
            }),
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
            Some(ItemContent::CommandExecution {
                command: "make test".into(),
                input: Vec::new(),
                output: String::new(),
                exit_code: None,
                status: ItemStatus::Interrupted,
                changes: Vec::new(),
            }),
        ]
    );
}

/// A Claude Code transcript line in the shape of [`CLAUDE_TRANSCRIPT`]'s,
/// numbered `n`, under parent `parent`.
fn claude_line(n: u32, parent: Option<u32>, role: &str, content: &str) -> String {
    let uuid = |n: u32| format!("00000000-0000-4000-8000-{n:012}");
    let parent = parent.map_or("null".to_owned(), |parent| format!("\"{}\"", uuid(parent)));
    let message = if role == "user" {
        format!(r#"{{"role":"user","content":"{content}"}}"#)
    } else {
        format!(
            r#"{{"id":"msg_{n}","type":"message","role":"assistant","model":"claude-example","content":[{{"type":"text","text":"{content}"}}]}}"#
        )
    };
    format!(
        r#"{{"type":"{role}","message":{message},"uuid":"{}","parentUuid":{parent},"isSidechain":false,"cwd":"/work/example","userType":"external","entrypoint":"cli","version":"2.1.322","gitBranch":"main","sessionId":"00000000-0000-4000-8000-000000000010","timestamp":"2026-01-02T03:04:{n:02}.000Z"}}"#,
        uuid(n)
    )
}

/// A conversation rewound in Claude Code shows the branch Claude Code
/// resumes, not the messages the rewind left behind. A message sent after a
/// rewind takes the restore point as its `parentUuid`, and the transcript's
/// last message is the leaf; a rewind with nothing sent after it is the
/// `last-prompt` record it writes (`{type, leafUuid, explicit: true,
/// rewound: true, sessionId}`), which names the restore point. Both as the
/// 2.1.322 bundle writes and reads them; content invented.
#[test]
fn a_rewound_claude_conversation_shows_the_branch_claude_resumes() {
    let asked = [
        claude_line(1, None, "user", "Start"),
        claude_line(2, Some(1), "assistant", "First answer."),
        claude_line(3, Some(2), "user", "Try the risky way"),
        claude_line(4, Some(3), "assistant", "Risky."),
    ];
    let assistant = |text: &str| {
        Some(ItemContent::AssistantMessage {
            text: text.into(),
            summarized: false,
        })
    };
    let resent = Transcript::new(
        &[
            &asked[..],
            &[
                claude_line(5, Some(2), "user", "Try the safe way"),
                claude_line(6, Some(5), "assistant", "Safe."),
            ],
        ]
        .concat()
        .join("\n"),
    );
    assert_eq!(
        shown(claude::convert(&resent.0, "claude:example").unwrap()),
        [
            user("Start"),
            assistant("First answer."),
            user("Try the safe way"),
            assistant("Safe."),
        ]
    );
    let rewound = Transcript::new(
        &[
            &asked[..],
            &[r#"{"type":"last-prompt","leafUuid":"00000000-0000-4000-8000-000000000002","explicit":true,"rewound":true,"sessionId":"00000000-0000-4000-8000-000000000010"}"#.to_owned()],
        ]
        .concat()
        .join("\n"),
    );
    assert_eq!(
        shown(claude::convert(&rewound.0, "claude:example").unwrap()),
        [user("Start"), assistant("First answer.")]
    );
}
