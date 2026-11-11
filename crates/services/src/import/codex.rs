use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _};
use std::path::{Path, PathBuf};

use agent::{ItemContent, ItemStatus, ThreadItem, ToolFamily, ToolOutput};
use serde_json::Value;

use super::{ConvertedEntry, ConvertedThread, file_modified_ms, timestamp_ms};

/// The suffix Codex gives a rollout it compressed: the whole `.jsonl` as one
/// zstd stream, beside where the plain file was (`rollout/src/compression.rs`
/// at `rust-v0.167.0`).
const COMPRESSED: &str = ".zst";

/// A rollout's records, read through its compression when Codex compressed it.
pub(super) fn open_rollout(path: &Path) -> std::io::Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(COMPRESSED))
    {
        Ok(Box::new(BufReader::new(zstd::stream::read::Decoder::new(
            file,
        )?)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

/// The rollout's file names as Codex writes them (`rollout_file_name.rs`):
/// `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread id>.jsonl`, with `_<rollout id>`
/// after the thread id for a rollout `thread/revert` wrote, and
/// [`COMPRESSED`] after it once compressed. The timestamp, thread id and
/// rollout id, which is the thread id when the name has no other.
pub(super) fn rollout_name(path: &Path) -> Option<(&str, &str, &str)> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(COMPRESSED).unwrap_or(name);
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let timestamp = core.get(..19)?;
    let ids = core.get(19..)?.strip_prefix('-')?;
    let (thread, rollout) = ids.split_once('_').unwrap_or((ids, ids));
    Some((timestamp, thread, rollout))
}

/// Whether Codex reads `path`: a rollout's compressed copy is passed over
/// while its plain file is still there (`should_skip_compressed_sibling`).
pub(super) fn is_read_rollout(path: &Path) -> bool {
    if rollout_name(path).is_none() {
        return false;
    }
    match path.to_str().and_then(|path| path.strip_suffix(COMPRESSED)) {
        Some(plain) => !Path::new(plain).exists(),
        None => true,
    }
}

/// The records a rollout continues from, oldest first, as Codex assembles a
/// thread's history (`resolve_rollout_lineage`, thread-store
/// `local/rollout_lineage.rs` at `rust-v0.167.0`). A rollout that
/// `thread/revert` or a fork wrote names in its `session_meta.history_base`
/// (`HistoryPosition`, `protocol/src/protocol.rs`) the rollout it continues
/// and the byte offset just past the last record it takes from it; that
/// rollout may continue another in turn.
fn inherited(path: &Path, meta: &Value, seen: &mut HashSet<String>) -> Result<Vec<String>, String> {
    let Some(base) = meta.pointer("/payload/history_base") else {
        return Ok(Vec::new());
    };
    let malformed = || format!("{} has an unreadable history_base", path.display());
    let rollout = base
        .get("thread_id")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let end = base
        .get("end_byte_offset")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    if !seen.insert(rollout.to_owned()) {
        return Err(format!(
            "the history of {} returns to rollout {rollout}",
            path.display()
        ));
    }
    let base_path = find_rollout(path, rollout)?;
    let read = |err: std::io::Error| format!("failed to read {}: {err}", base_path.display());
    let mut bytes = Vec::new();
    open_rollout(&base_path)
        .map_err(read)?
        .take(end)
        .read_to_end(&mut bytes)
        .map_err(read)?;
    if u64::try_from(bytes.len()).ok() != Some(end) {
        return Err(format!(
            "{} ends before the history {} continues from",
            base_path.display(),
            path.display()
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|err| format!("failed to read {}: {err}", base_path.display()))?;
    let own: Vec<String> = text.lines().map(str::to_owned).collect();
    let base_meta = own
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or(Value::Null);
    let mut lines = inherited(&base_path, &base_meta, seen)?;
    lines.extend(own);
    Ok(lines)
}

/// The rollout whose file name carries `rollout` as its rollout id, in the
/// sessions and then the archived sessions of the Codex home `path` is in
/// (`find_rollout_path_by_rollout_id`, `rollout/src/list.rs`).
fn find_rollout(path: &Path, rollout: &str) -> Result<PathBuf, String> {
    let home = path
        .ancestors()
        .find(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "sessions" || name == "archived_sessions")
        })
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} is not in a Codex home", path.display()))?;
    let mut unreadable = Vec::new();
    for sessions in ["sessions", "archived_sessions"] {
        let found = super::collect_files(
            &home.join(sessions),
            &|candidate| {
                is_read_rollout(candidate)
                    && rollout_name(candidate).is_some_and(|(_, _, id)| id == rollout)
            },
            &mut unreadable,
        );
        if let Some(found) = found.into_iter().next() {
            return Ok(found);
        }
    }
    let mut error = format!(
        "rollout {rollout}, whose history {} continues, is not in {}",
        path.display(),
        home.display()
    );
    for path in unreadable {
        error.push_str("; ");
        error.push_str(&path);
    }
    Err(error)
}

pub(super) fn convert(path: &Path, external_id: &str) -> Result<ConvertedThread, String> {
    let fallback_ts = file_modified_ms(path);
    let read = |err: std::io::Error| format!("failed to read {}: {err}", path.display());
    let mut lines = open_rollout(path).map_err(read)?.lines();
    let first = lines
        .next()
        .ok_or_else(|| format!("{} is empty", path.display()))?
        .map_err(read)?;
    let meta: Value = serde_json::from_str(&first).map_err(|err| {
        format!(
            "invalid Codex session metadata in {}: {err}",
            path.display()
        )
    })?;
    if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Err(format!(
            "{} does not start with session_meta",
            path.display()
        ));
    }

    let inherited = inherited(path, &meta, &mut HashSet::new())?;
    let first_meta_ts = inherited
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or(meta)
        .get("timestamp")
        .and_then(timestamp_ms)
        .unwrap_or(fallback_ts);
    let created_ms = first_meta_ts;
    let mut updated_ms = first_meta_ts;
    let mut entries = Vec::new();
    let mut pending = HashMap::<String, usize>::new();
    let mut next_id = 0_u64;

    for line in inherited
        .into_iter()
        .map(Ok)
        .chain(lines.map(|line| line.map_err(read)))
    {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(timestamp_ms)
            .unwrap_or(updated_ms);
        updated_ms = ts;
        match value.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                let Some(kind) = payload.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "message" => {
                        let role = payload.get("role").and_then(Value::as_str);
                        if role == Some("developer") {
                            continue;
                        }
                        let block_kind = if role == Some("user") {
                            "input_text"
                        } else {
                            "output_text"
                        };
                        for text in message_texts(payload, block_kind) {
                            if role == Some("user") && is_harness_text(&text) {
                                continue;
                            }
                            let content = if role == Some("user") {
                                ItemContent::UserMessage {
                                    text,
                                    attachments: Vec::new(),
                                }
                            } else if role == Some("assistant") {
                                ItemContent::AssistantMessage {
                                    text,
                                    summarized: false,
                                }
                            } else {
                                continue;
                            };
                            push_item(&mut entries, ts, &mut next_id, content, None);
                        }
                    }
                    "reasoning" => {
                        let text = payload
                            .get("summary")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            push_item(
                                &mut entries,
                                ts,
                                &mut next_id,
                                ItemContent::Reasoning { text },
                                None,
                            );
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let arguments = payload
                            .get("arguments")
                            .or_else(|| payload.get("input"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let input = arguments
                            .as_str()
                            .and_then(|text| serde_json::from_str(text).ok())
                            .unwrap_or(arguments);
                        let name = tool_name(payload);
                        if name == UPDATE_PLAN
                            && let Some((explanation, steps)) = agent::codex::rollout_plan(&input)
                        {
                            entries.push(ConvertedEntry::Plan {
                                ts,
                                explanation,
                                steps,
                            });
                            continue;
                        }
                        let content = match input.get("cmd").and_then(Value::as_str) {
                            Some(command) if name == EXEC_COMMAND => {
                                ItemContent::CommandExecution {
                                    command: command.to_owned(),
                                    output: String::new(),
                                    exit_code: None,
                                    status: ItemStatus::InProgress,
                                }
                            }
                            _ => ItemContent::ToolCall {
                                image_reads: Vec::new(),
                                family: match input.get("path").and_then(Value::as_str) {
                                    Some(path) if name == VIEW_IMAGE => Some(ToolFamily::Read {
                                        path: path.to_owned(),
                                        first_line: None,
                                        note: None,
                                    }),
                                    _ => agent::codex::question_family(&name, &input, None),
                                }
                                .map(Box::new),
                                name,
                                input,
                                output: None,
                                status: ItemStatus::InProgress,
                            },
                        };
                        let index = entries.len();
                        push_item(&mut entries, ts, &mut next_id, content, Some(&call_id));
                        if !call_id.is_empty() {
                            pending.insert(call_id, index);
                        }
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(index) = pending.remove(call_id) else {
                            continue;
                        };
                        let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index) else {
                            continue;
                        };
                        let (result, failed) =
                            call_output(payload.get("output").unwrap_or(&Value::Null));
                        match &mut item.content {
                            ItemContent::CommandExecution {
                                output,
                                exit_code,
                                status,
                                ..
                            } => {
                                if aborted(&result.text) {
                                    *status = ItemStatus::Interrupted;
                                } else {
                                    let (code, printed) = exec_result(&result.text);
                                    *exit_code = code;
                                    // Codex's own rule for a command's status
                                    // (`core/src/tools/events.rs`): it failed
                                    // when it exited non-zero.
                                    *status = match code {
                                        Some(code) if code != 0 => ItemStatus::Failed,
                                        _ => ItemStatus::Completed,
                                    };
                                    *output = printed.to_owned();
                                }
                            }
                            // An image Codex was shown is the `imageView`
                            // item it reports live (`view_image.rs`); one it
                            // could not show stays the call, with its error.
                            ItemContent::ToolCall { name, input, .. }
                                if name == VIEW_IMAGE && !failed && !result.images.is_empty() =>
                            {
                                item.content = ItemContent::ImageRead {
                                    path: input
                                        .get("path")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned(),
                                    image: result.images.into_iter().next(),
                                };
                            }
                            ItemContent::ToolCall {
                                image_reads,
                                name,
                                input,
                                output,
                                status,
                                family,
                            } => {
                                if let Some(asked) =
                                    agent::codex::question_family(name, input, Some(&result.text))
                                {
                                    *family = Some(Box::new(asked));
                                }
                                *status = if failed {
                                    ItemStatus::Failed
                                } else if aborted(&result.text) {
                                    ItemStatus::Interrupted
                                } else {
                                    ItemStatus::Completed
                                };
                                *image_reads = result.images;
                                // Codex puts `Wall time: …` and `Output:`
                                // before an MCP tool's result for the model
                                // (`McpToolOutput::response_payload`,
                                // `core/src/tools/context.rs`); live, the
                                // result comes as the server gave it.
                                *output = Some(if name.contains('/') {
                                    exec_result(&result.text).1.to_owned()
                                } else {
                                    result.text
                                });
                            }
                            _ => {}
                        }
                    }
                    "web_search_call" => {
                        push_item(
                            &mut entries,
                            ts,
                            &mut next_id,
                            ItemContent::WebSearch {
                                query: web_search_query(
                                    payload.get("action").unwrap_or(&Value::Null),
                                ),
                            },
                            payload.get("id").and_then(Value::as_str),
                        );
                    }
                    "image_generation_call" => {
                        let mut item = serde_json::Map::new();
                        for (key, field) in [
                            ("status", "status"),
                            ("revised_prompt", "revisedPrompt"),
                            ("result", "result"),
                        ] {
                            if let Some(value) = payload.get(key) {
                                item.insert(field.into(), value.clone());
                            }
                        }
                        let mut content = agent::codex::image_generation(&Value::Object(item));
                        // A generation the rollout ends without a picture for never finished.
                        if let ItemContent::ToolCall { status, .. } = &mut content
                            && *status == ItemStatus::InProgress
                        {
                            *status = ItemStatus::Interrupted;
                        }
                        push_item(
                            &mut entries,
                            ts,
                            &mut next_id,
                            content,
                            payload.get("id").and_then(Value::as_str),
                        );
                    }
                    "tool_search_call" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let index = entries.len();
                        push_item(
                            &mut entries,
                            ts,
                            &mut next_id,
                            ItemContent::ToolCall {
                                image_reads: Vec::new(),
                                name: "tool_search".into(),
                                input: payload.get("arguments").cloned().unwrap_or(Value::Null),
                                output: None,
                                status: ItemStatus::InProgress,
                                family: None,
                            },
                            Some(&call_id),
                        );
                        if !call_id.is_empty() {
                            pending.insert(call_id, index);
                        }
                    }
                    "tool_search_output" => {
                        let Some(index) = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .and_then(|call_id| pending.remove(call_id))
                        else {
                            continue;
                        };
                        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
                            && let ItemContent::ToolCall { output, status, .. } = &mut item.content
                        {
                            *output = Some(found_tools(payload.get("tools")));
                            *status = ItemStatus::Completed;
                        }
                    }
                    "local_shell_call" => {
                        let command = payload
                            .pointer("/action/command")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ");
                        push_item(
                            &mut entries,
                            ts,
                            &mut next_id,
                            ItemContent::CommandExecution {
                                command,
                                output: String::new(),
                                exit_code: None,
                                status: ItemStatus::Completed,
                            },
                            payload.get("id").and_then(Value::as_str),
                        );
                    }
                    _ => {}
                }
            }
            Some("event_msg") => match value.pointer("/payload/type").and_then(Value::as_str) {
                // A paginated rollout's own record of a command or a patch,
                // and a legacy one's of a patch, are what Codex shows for
                // the call of the same id: they replace its call.
                Some("item_completed") => {
                    let item = value.pointer("/payload/item").unwrap_or(&Value::Null);
                    let content = match item.get("type").and_then(Value::as_str) {
                        Some("CommandExecution") => agent::codex::rollout_command(item),
                        Some("FileChange") => ItemContent::FileChange {
                            changes: agent::codex::rollout_file_changes(
                                item.get("changes").unwrap_or(&Value::Null),
                            ),
                            status: agent::codex::rollout_status(
                                item.get("status").and_then(Value::as_str),
                            ),
                        },
                        _ => continue,
                    };
                    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    record(&mut entries, &mut pending, ts, &mut next_id, id, content);
                }
                Some("patch_apply_end") => {
                    let payload = &value["payload"];
                    let content = ItemContent::FileChange {
                        changes: agent::codex::rollout_file_changes(
                            payload.get("changes").unwrap_or(&Value::Null),
                        ),
                        status: agent::codex::rollout_status(
                            payload.get("status").and_then(Value::as_str),
                        ),
                    };
                    let id = payload
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    record(&mut entries, &mut pending, ts, &mut next_id, id, content);
                }
                Some("context_compacted") => entries.push(ConvertedEntry::Compacted { ts }),
                Some("thread_rolled_back") => {
                    if let Some(turns) = value.pointer("/payload/num_turns").and_then(Value::as_u64)
                    {
                        roll_back(&mut entries, &mut pending, turns);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    // A call the rollout never answered was cut off with its turn.
    for index in pending.into_values() {
        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
            && let ItemContent::CommandExecution { status, .. }
            | ItemContent::ToolCall { status, .. } = &mut item.content
        {
            *status = ItemStatus::Interrupted;
        }
    }
    Ok(ConvertedThread {
        external_id: external_id.to_string(),
        created_ms,
        updated_ms,
        title: None,
        entries,
    })
}

/// Drop the last `turns` user turns, as Codex replays a rollback
/// (`ContextManager::drop_last_n_user_turns`): everything from the user
/// message that opens the first dropped turn on, and when there are no more
/// turns than that, from the first user message on.
fn roll_back(entries: &mut Vec<ConvertedEntry>, pending: &mut HashMap<String, usize>, turns: u64) {
    let starts: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(
                entry,
                ConvertedEntry::Item { item, .. }
                    if matches!(item.content, ItemContent::UserMessage { .. })
            )
        })
        .map(|(index, _)| index)
        .collect();
    let turns = usize::try_from(turns).unwrap_or(usize::MAX);
    if turns == 0 || starts.is_empty() {
        return;
    }
    let cut = starts[starts.len().saturating_sub(turns)];
    entries.truncate(cut);
    pending.retain(|_, index| *index < cut);
}

/// `content` as the item `id` names: in place of the call of that id, which
/// is then answered, or as an item of its own when no call has the id.
fn record(
    entries: &mut Vec<ConvertedEntry>,
    pending: &mut HashMap<String, usize>,
    ts: u64,
    next_id: &mut u64,
    id: &str,
    content: ItemContent,
) {
    pending.remove(id);
    let call = (!id.is_empty())
        .then(|| {
            entries.iter_mut().rev().find_map(|entry| match entry {
                ConvertedEntry::Item { item, .. } if item.id == id => Some(item),
                _ => None,
            })
        })
        .flatten();
    match call {
        Some(item) => item.content = content,
        None => push_item(entries, ts, next_id, content, Some(id)),
    }
}

fn push_item(
    entries: &mut Vec<ConvertedEntry>,
    ts: u64,
    next_id: &mut u64,
    content: ItemContent,
    id: Option<&str>,
) {
    *next_id += 1;
    entries.push(ConvertedEntry::Item {
        ts,
        item: Box::new(ThreadItem {
            id: id
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("imported-codex-{next_id}")),
            parent_item_id: None,
            content,
        }),
    });
}

fn message_texts(payload: &Value, kind: &str) -> Vec<String> {
    payload
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn is_harness_text(text: &str) -> bool {
    let text = text.trim_start();
    ["<environment_context", "<permissions"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Whether `output` is what Codex writes for a call its interrupted turn
/// cut off (`abort_message`, `core/src/tools/parallel.rs` at
/// `rust-v0.167.0`): one it never dispatched, a command it stopped, or
/// another call it stopped.
fn aborted(output: &str) -> bool {
    output == "aborted before dispatch"
        || output
            .strip_prefix("Wall time: ")
            .is_some_and(|rest| rest.ends_with(" seconds\naborted by user"))
        || output
            .strip_prefix("aborted by user after ")
            .is_some_and(|rest| rest.ends_with('s'))
}

/// Codex 0.167's tool that runs a command line (`create_exec_command_tool`,
/// `core/src/tools/handlers/shell_spec.rs`), whose `cmd` is the command.
const EXEC_COMMAND: &str = "exec_command";
/// The tool that sets Codex's plan (`plan_spec.rs`).
const UPDATE_PLAN: &str = "update_plan";
/// The tool that shows Codex an image (`view_image_spec.rs`).
const VIEW_IMAGE: &str = "view_image";

/// A command's result as Codex words it for the model: a header of
/// `Key: value` lines, the code it exited with among them, then `Output:`
/// and what it printed. `exec_command` writes `Process exited with code N`
/// (`ExecCommandToolOutput::response_header`, `core/src/tools/context.rs`),
/// a command run through the exec runtime `Exit code: N`
/// (`format_exec_output_for_model`, `core/src/tools/mod.rs`); a process
/// still running when the call returned has no code yet. A text with no
/// header is all output.
fn exec_result(text: &str) -> (Option<i32>, &str) {
    let Some((header, printed)) = text
        .split_once("\nOutput:\n")
        .or_else(|| text.strip_suffix("\nOutput:").map(|header| (header, "")))
    else {
        return (None, text);
    };
    let code = header.lines().find_map(|line| {
        line.strip_prefix("Process exited with code ")
            .or_else(|| line.strip_prefix("Exit code: "))?
            .trim()
            .parse()
            .ok()
    });
    (code, printed)
}

/// The live adapter's name for a call: an MCP tool, which the rollout files
/// under a `mcp__<server>` namespace, is `<server>/<tool>`.
fn tool_name(payload: &Value) -> String {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    match payload
        .get("namespace")
        .and_then(Value::as_str)
        .and_then(|namespace| namespace.strip_prefix("mcp__"))
    {
        Some(server) => format!("{server}/{name}"),
        None => name.to_owned(),
    }
}

/// A call's output as text and images, and whether the tool reported an
/// error. Rollouts hold the model's content array, or a string that may
/// itself be a serialized MCP result.
fn call_output(raw: &Value) -> (ToolOutput, bool) {
    let parsed = raw
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result = parsed.as_ref().unwrap_or(raw);
    let failed = result.get("isError").and_then(Value::as_bool) == Some(true);
    let blocks = result.is_array() || result.get("content").is_some_and(Value::is_array);
    let output = if blocks {
        ToolOutput::from_content(result)
    } else {
        ToolOutput {
            text: readable_output(raw),
            images: Vec::new(),
        }
    };
    (output, failed)
}

/// The text Codex shows for a web search action.
fn web_search_query(action: &Value) -> String {
    let field = |key: &str| {
        action
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    match action.get("type").and_then(Value::as_str) {
        Some("search") => field("query")
            .or_else(|| {
                action
                    .get("queries")
                    .and_then(Value::as_array)
                    .and_then(|queries| queries.first())
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_owned(),
        Some("open_page") => field("url").unwrap_or_default().to_owned(),
        Some("find_in_page") => match (field("pattern"), field("url")) {
            (Some(pattern), Some(url)) => format!("'{pattern}' in {url}"),
            (Some(pattern), None) => format!("'{pattern}'"),
            (None, Some(url)) => url.to_owned(),
            (None, None) => String::new(),
        },
        _ => String::new(),
    }
}

/// The tools a search loaded, one per line, named as calls name them.
fn found_tools(tools: Option<&Value>) -> String {
    let mut names = Vec::new();
    for tool in tools.and_then(Value::as_array).into_iter().flatten() {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        match tool.get("tools").and_then(Value::as_array) {
            Some(nested) => names.extend(nested.iter().map(|nested| {
                tool_name(&serde_json::json!({
                    "namespace": name,
                    "name": nested.get("name").and_then(Value::as_str).unwrap_or_default(),
                }))
            })),
            None => names.push(name.to_owned()),
        }
    }
    names.join("\n")
}

fn readable_output(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        if let Ok(nested) = serde_json::from_str::<Value>(text) {
            return readable_output(&nested);
        }
        return text.to_string();
    }
    if let Some(text) = value.get("output").and_then(Value::as_str) {
        return text.to_string();
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}
