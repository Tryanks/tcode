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
    let mut first_user = None;
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
                                first_user.get_or_insert_with(|| text.clone());
                                ItemContent::UserMessage {
                                    text,
                                    context_len: None,
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
                        let content = ItemContent::ToolCall {
                            image_reads: Vec::new(),
                            name: tool_name(payload),
                            input,
                            output: None,
                            status: ItemStatus::InProgress,
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
                        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
                            && let ItemContent::ToolCall {
                                image_reads,
                                output,
                                status,
                                ..
                            } = &mut item.content
                        {
                            let (result, failed) =
                                call_output(payload.get("output").unwrap_or(&Value::Null));
                            *status = if failed {
                                ItemStatus::Failed
                            } else if result.text == ABORTED_BEFORE_DISPATCH {
                                ItemStatus::Interrupted
                            } else {
                                ItemStatus::Completed
                            };
                            *image_reads = result.images;
                            *output = Some(result.text);
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
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("context_compacted") =>
            {
                entries.push(ConvertedEntry::Compacted { ts });
            }
            _ => {}
        }
    }
    // A call the rollout never answered was cut off with its turn.
    for index in pending.into_values() {
        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
            && let ItemContent::ToolCall { status, .. } = &mut item.content
        {
            *status = ItemStatus::Interrupted;
        }
    }
    Ok(ConvertedThread {
        external_id: external_id.to_string(),
        created_ms,
        updated_ms,
        first_user,
        entries,
    })
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

/// What Codex writes as the output of a call it dropped when its turn was
/// interrupted.
const ABORTED_BEFORE_DISPATCH: &str = "aborted before dispatch";

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
