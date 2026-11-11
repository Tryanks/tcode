use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use agent::claude::TranscriptCall;
use agent::{ItemContent, ItemStatus, ThreadItem};
use serde_json::Value;

use super::{ConvertedEntry, ConvertedThread, file_modified_ms, timestamp_ms};

pub(super) fn convert(path: &Path, external_id: &str) -> Result<ConvertedThread, String> {
    let file =
        File::open(path).map_err(|err| format!("failed to open {}: {err}", path.display()))?;
    let fallback_ts = file_modified_ms(path);
    let mut entries = Vec::new();
    let mut pending: HashMap<String, (usize, TranscriptCall)> = HashMap::new();
    let mut first_ts = None;
    let mut last_ts = None;
    // The last record of each kind wins; an empty one clears it.
    let mut titles = HashMap::<&str, Option<String>>::new();
    let mut next_id = 0_u64;

    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            records.push(value);
        }
    }
    let live = live_branch(&records);

    for value in &records {
        let ts = value
            .get("timestamp")
            .and_then(timestamp_ms)
            .unwrap_or(fallback_ts);
        first_ts.get_or_insert(ts);
        last_ts = Some(ts);
        if should_skip(value)
            || is_row(value)
                && !value
                    .get("uuid")
                    .and_then(Value::as_str)
                    .is_some_and(|uuid| live.contains(uuid))
        {
            continue;
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(content) = value.pointer("/message/content") else {
                    continue;
                };
                for block in content_blocks(content) {
                    match block {
                        UserBlock::Text(text) => {
                            for content in user_text(text) {
                                next_id += 1;
                                entries.push(ConvertedEntry::Item {
                                    ts,
                                    item: ThreadItem {
                                        id: format!("imported-user-{next_id}"),
                                        parent_item_id: None,
                                        content,
                                    },
                                });
                            }
                        }
                        UserBlock::ToolResult { id, block } => {
                            let Some((index, call)) = pending.remove(id) else {
                                continue;
                            };
                            if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
                            {
                                item.content = call.finish(block, value.get("toolUseResult"));
                            }
                        }
                    }
                }
            }
            Some("assistant") => {
                let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array)
                else {
                    continue;
                };
                for block in blocks {
                    let Some(kind) = block.get("type").and_then(Value::as_str) else {
                        continue;
                    };
                    let content = match kind {
                        "text" => block.get("text").and_then(Value::as_str).map(|text| {
                            ItemContent::AssistantMessage {
                                text: text.to_string(),
                                summarized: false,
                            }
                        }),
                        "thinking" => block.get("thinking").and_then(Value::as_str).map(|text| {
                            ItemContent::Reasoning {
                                text: text.to_string(),
                            }
                        }),
                        "tool_use" => {
                            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            let (call, content) = TranscriptCall::start(name.to_string(), input);
                            if !id.is_empty() {
                                pending.insert(id.to_string(), (entries.len(), call));
                            }
                            Some(content)
                        }
                        _ => None,
                    };
                    if let Some(content) = content {
                        next_id += 1;
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("imported-assistant-{next_id}"));
                        entries.push(ConvertedEntry::Item {
                            ts,
                            item: ThreadItem {
                                id,
                                parent_item_id: None,
                                content,
                            },
                        });
                    }
                }
            }
            Some("system")
                if value.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                entries.push(ConvertedEntry::Compacted { ts });
            }
            Some(kind @ ("agent-name" | "custom-title" | "ai-title")) => {
                let (kind, field) = match kind {
                    "agent-name" => ("agent-name", "agentName"),
                    "custom-title" => ("custom-title", "customTitle"),
                    _ => ("ai-title", "aiTitle"),
                };
                let title = value
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_string);
                titles.insert(kind, title);
            }
            _ => {}
        }
    }
    if records.is_empty() {
        return Err(format!(
            "{} contains no parseable JSON lines",
            path.display()
        ));
    }
    // A call the transcript never answered was cut off with its turn.
    for (index, _) in pending.into_values() {
        if let Some(ConvertedEntry::Item { item, .. }) = entries.get_mut(index)
            && let ItemContent::CommandExecution { status, .. }
            | ItemContent::FileChange { status, .. }
            | ItemContent::ToolCall { status, .. } = &mut item.content
        {
            *status = ItemStatus::Interrupted;
        }
    }
    let created_ms = first_ts.unwrap_or(fallback_ts);
    // Claude Code 2.1.322 names a session by its agent name, else its
    // custom title, else its generated title, before any prompt.
    let title = ["agent-name", "custom-title", "ai-title"]
        .into_iter()
        .find_map(|kind| titles.remove(kind).flatten());
    Ok(ConvertedThread {
        external_id: external_id.to_string(),
        created_ms,
        updated_ms: last_ts.unwrap_or(created_ms),
        title,
        entries,
    })
}

/// A conversation row: what the transcript chains by `uuid` and `parentUuid`.
fn is_row(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "assistant" | "system" | "attachment")
    )
}

/// The uuids of the branch Claude Code resumes, read as 2.1.322's transcript
/// loader picks it. Its leaf is the last row of the main conversation,
/// unless a `last-prompt` record names another (`leafUuid`): one a rewind
/// wrote (`explicit: true`) with no row after it, or one the last row does not
/// descend from. A rewind back to before the first message writes a
/// `last-prompt` with `explicit: true` and no leaf, and a compaction drops the
/// leaf named before it. From the leaf the branch runs up through each
/// record's `parentUuid`; a compaction boundary, whose `parentUuid` is null,
/// continues through its `logicalParentUuid`, so what was compacted still
/// shows. The rows a rewind left behind are on no such path.
fn live_branch(records: &[Value]) -> HashSet<String> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut parents = HashMap::<String, Option<String>>::new();
    let mut last_row = None;
    let mut named: Option<String> = None;
    let mut explicit = false;
    let mut emptied = false;
    for record in records {
        if record.get("type").and_then(Value::as_str) == Some("last-prompt") {
            let rewind = record.get("explicit") == Some(&Value::Bool(true));
            match record.get("leafUuid") {
                Some(Value::String(leaf)) if !leaf.is_empty() => {
                    explicit = rewind || explicit && named.as_ref() == Some(leaf);
                    named = Some(leaf.clone());
                    emptied = false;
                }
                Some(Value::Null) if rewind => {
                    named = None;
                    explicit = false;
                    emptied = true;
                }
                _ => {}
            }
            continue;
        }
        let Some(uuid) = text(record, "uuid") else {
            continue;
        };
        let parent = text(record, "parentUuid").or_else(|| text(record, "logicalParentUuid"));
        parents.insert(uuid.clone(), parent);
        if is_row(record) && record.get("isSidechain") != Some(&Value::Bool(true)) {
            if record.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                named = None;
            }
            last_row = Some(uuid);
            explicit = false;
            emptied = false;
        }
    }
    let ancestry = |from: &str| {
        let mut seen = HashSet::new();
        let mut next = Some(from.to_owned());
        while let Some(uuid) = next.filter(|uuid| parents.contains_key(uuid)) {
            if !seen.insert(uuid.clone()) {
                break;
            }
            next = parents[&uuid].clone();
        }
        seen
    };
    if emptied {
        return HashSet::new();
    }
    let named = named.filter(|leaf| parents.contains_key(leaf));
    let leaf = match (named, last_row) {
        (Some(named), Some(last)) if explicit || !ancestry(&last).contains(&named) => named,
        (Some(named), None) => named,
        (_, Some(last)) => last,
        (None, None) => return HashSet::new(),
    };
    ancestry(&leaf)
}

fn should_skip(value: &Value) -> bool {
    value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
}

/// What a user text shows, read as Claude Code 2.1.322 writes and shows it.
/// A slash command is recorded as its `<command-message>`, `<command-name>`
/// and `<command-args>` tags and shown as the command line; a local
/// command's output is recorded in `<local-command-stdout>` and
/// `<local-command-stderr>` and shown unless it is empty or "(no content)";
/// the caveat recorded before such output is not shown.
fn user_text(text: String) -> Vec<ItemContent> {
    if let Some(name) = tagged(&text, "command-name") {
        let line = match tagged(&text, "command-args").filter(|args| !args.is_empty()) {
            Some(args) => format!("{name} {args}"),
            None => name.to_string(),
        };
        return vec![user_message(line)];
    }
    if text.contains("<local-command-caveat>") {
        return Vec::new();
    }
    let output: Vec<_> = ["local-command-stdout", "local-command-stderr"]
        .into_iter()
        .filter_map(|tag| Some((tag, tagged(&text, tag)?)))
        .collect();
    if output.is_empty() {
        return vec![user_message(text)];
    }
    output
        .into_iter()
        .filter(|(_, output)| !output.is_empty() && *output != "(no content)")
        .map(|(tag, output)| ItemContent::Other {
            provider_kind: tag.to_string(),
            summary: output.to_string(),
        })
        .collect()
}

/// The trimmed text between `<tag>` and `</tag>`.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(text[start..end].trim())
}

fn user_message(text: String) -> ItemContent {
    ItemContent::UserMessage {
        text,
        attachments: Vec::new(),
    }
}

enum UserBlock<'a> {
    Text(String),
    /// A `tool_result` block, answering the call its `tool_use_id` names.
    ToolResult {
        id: &'a str,
        block: &'a Value,
    },
}

fn content_blocks(content: &Value) -> Vec<UserBlock<'_>> {
    if let Some(text) = content.as_str() {
        return vec![UserBlock::Text(text.to_string())];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .map(|text| UserBlock::Text(text.to_string())),
            Some("tool_result") => Some(UserBlock::ToolResult {
                id: block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                block,
            }),
            _ => None,
        })
        .collect()
}
