use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use agent::{ItemContent, ItemStatus, ThreadItem, ToolOutput};
use serde_json::Value;

use super::{ConvertedEntry, ConvertedThread, file_modified_ms, timestamp_ms};

pub(super) fn convert(path: &Path, external_id: &str) -> Result<ConvertedThread, String> {
    let file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let fallback_ts = file_modified_ms(path);
    let mut lines = BufReader::new(file).lines();
    let first = lines
        .next()
        .ok_or_else(|| format!("{} is empty", path.display()))?
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
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

    let first_meta_ts = meta
        .get("timestamp")
        .and_then(timestamp_ms)
        .unwrap_or(fallback_ts);
    let created_ms = first_meta_ts;
    let mut updated_ms = first_meta_ts;
    let mut entries = Vec::new();
    let mut pending = HashMap::<String, usize>::new();
    let mut next_id = 0_u64;

    for line in lines {
        let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
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
                            ItemContent::ToolCall {
                                image_reads,
                                output,
                                status,
                                ..
                            } => {
                                *status = if failed {
                                    ItemStatus::Failed
                                } else if aborted(&result.text) {
                                    ItemStatus::Interrupted
                                } else {
                                    ItemStatus::Completed
                                };
                                *image_reads = result.images;
                                *output = Some(result.text);
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
                ConvertedEntry::Item {
                    item: ThreadItem {
                        content: ItemContent::UserMessage { .. },
                        ..
                    },
                    ..
                }
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
        item: ThreadItem {
            id: id
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("imported-codex-{next_id}")),
            parent_item_id: None,
            content,
        },
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
