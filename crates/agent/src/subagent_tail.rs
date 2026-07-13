//! Polling reader and canonical mapper for Claude subagent transcript JSONL.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{AgentEvent, ItemContent, ItemStatus, ThreadItem};

/// Stateful mapper for one subagent transcript. Tool calls are retained until
/// their matching result arrives so completion snapshots keep the original input.
pub(crate) struct TranscriptMapper {
    parent_id: String,
    tools: HashMap<String, (String, Value)>,
    next_user_id: u64,
}

impl TranscriptMapper {
    pub(crate) fn new(parent_id: impl Into<String>) -> Self {
        Self {
            parent_id: parent_id.into(),
            tools: HashMap::new(),
            next_user_id: 0,
        }
    }

    pub(crate) fn map_value(&mut self, value: &Value) -> Vec<AgentEvent> {
        if should_skip(value) {
            return Vec::new();
        }
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.map_assistant(value),
            Some("user") => self.map_user(value),
            _ => Vec::new(),
        }
    }

    fn map_assistant(&mut self, value: &Value) -> Vec<AgentEvent> {
        let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
            return Vec::new();
        };
        let message_id = value
            .pointer("/message/id")
            .or_else(|| value.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or("assistant");
        let mut events = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        events.push(AgentEvent::ItemCompleted(self.item(
                            format!("{message_id}:{index}"),
                            ItemContent::AssistantMessage { text: text.into() },
                        )));
                    }
                }
                Some("thinking") => {
                    let text = block
                        .get("thinking")
                        .or_else(|| block.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        events.push(AgentEvent::ItemCompleted(self.item(
                            format!("{message_id}:{index}"),
                            ItemContent::Reasoning { text: text.into() },
                        )));
                    }
                }
                Some("tool_use") => {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    self.tools
                        .insert(id.to_owned(), (name.clone(), input.clone()));
                    events.push(AgentEvent::ItemStarted(self.item(
                        id,
                        ItemContent::ToolCall {
                            name,
                            input,
                            output: None,
                            status: ItemStatus::InProgress,
                        },
                    )));
                }
                _ => {}
            }
        }
        events
    }

    fn map_user(&mut self, value: &Value) -> Vec<AgentEvent> {
        let Some(content) = value.pointer("/message/content") else {
            return Vec::new();
        };
        let blocks: Vec<&Value> = match content.as_array() {
            Some(blocks) => blocks.iter().collect(),
            None => vec![content],
        };
        let mut events = Vec::new();
        for block in blocks {
            if let Some(text) = block.as_str().or_else(|| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            }) {
                if is_user_noise(text) {
                    continue;
                }
                self.next_user_id += 1;
                events.push(AgentEvent::ItemCompleted(self.item(
                    format!("user-{}", self.next_user_id),
                    ItemContent::UserMessage { text: text.into() },
                )));
                continue;
            }
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let Some((name, input)) = self.tools.remove(id) else {
                continue;
            };
            let failed = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            events.push(AgentEvent::ItemCompleted(self.item(
                id,
                ItemContent::ToolCall {
                    name,
                    input,
                    output: Some(content_text(block.get("content"))),
                    status: if failed {
                        ItemStatus::Failed
                    } else {
                        ItemStatus::Completed
                    },
                },
            )));
        }
        events
    }

    fn item(&self, raw_id: impl std::fmt::Display, content: ItemContent) -> ThreadItem {
        ThreadItem {
            id: format!("{}:{raw_id}", self.parent_id),
            parent_item_id: Some(self.parent_id.clone()),
            content,
        }
    }
}

/// Incremental reader used both by the polling task and deterministic tests.
pub(crate) struct TailReader {
    path: PathBuf,
    offset: u64,
    mapper: TranscriptMapper,
}

impl TailReader {
    pub(crate) fn new(path: PathBuf, parent_id: impl Into<String>) -> Self {
        Self {
            path,
            offset: 0,
            mapper: TranscriptMapper::new(parent_id),
        }
    }

    pub(crate) fn read_appended(&mut self) -> std::io::Result<Vec<AgentEvent>> {
        let mut file = File::open(&self.path)?;
        if file.metadata()?.len() < self.offset {
            self.offset = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        let mut events = Vec::new();
        loop {
            let start = self.offset;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            if !line.ends_with('\n') {
                self.offset = start;
                break;
            }
            self.offset += bytes as u64;
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                events.extend(self.mapper.map_value(&value));
            }
        }
        Ok(events)
    }
}

pub(crate) fn find_transcript(
    claude_dir: &Path,
    session_id: &str,
    task_id: &str,
    tool_use_id: &str,
) -> Option<PathBuf> {
    let session_dir = find_named_dir(&claude_dir.join("projects"), session_id, 5)?;
    let task_path = session_dir.join("tasks").join(format!("{task_id}.output"));
    if task_path.is_file() {
        return Some(task_path);
    }
    let subagents = session_dir.join("subagents");
    for entry in std::fs::read_dir(subagents).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json")
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".meta.json"))
        {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta) else {
            continue;
        };
        if meta.get("toolUseId").and_then(Value::as_str) == Some(tool_use_id) {
            let transcript = path.with_file_name(
                path.file_name()
                    .and_then(|name| name.to_str())?
                    .trim_end_matches(".meta.json")
                    .to_owned()
                    + ".jsonl",
            );
            return transcript.is_file().then_some(transcript);
        }
    }
    None
}

fn find_named_dir(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|part| part.to_str()) == Some(name) {
            return Some(path);
        }
        if let Some(found) = find_named_dir(&path, name, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn should_skip(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("queue-operation" | "summary" | "file-history-snapshot")
    ) || value.get("attachment").is_some()
        || value.get("isMeta").and_then(Value::as_bool) == Some(true)
}

fn is_user_noise(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<command-message>",
        "<command-name>",
        "<local-command-stdout>",
        "Caveat: The messages below",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn tail_reader_maps_only_new_complete_lines() {
        let path = std::env::temp_dir().join(format!(
            "tcode-subagent-tail-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = File::create(&path).unwrap();
        writeln!(file, r#"{{"type":"user","message":{{"content":"do it"}}}}"#).unwrap();
        file.flush().unwrap();

        let mut tail = TailReader::new(path.clone(), "spawn-1");
        let first = tail.read_appended().unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), content: ItemContent::UserMessage { text }, .. })
                if parent == "spawn-1" && text == "do it"
        ));

        writeln!(file, r#"{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"working"}}]}}}}"#).unwrap();
        file.flush().unwrap();
        let second = tail.read_appended().unwrap();
        assert_eq!(second.len(), 1);
        assert!(matches!(
            &second[0],
            AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::AssistantMessage { text }, .. })
                if id == "spawn-1:m1:0" && text == "working"
        ));
        assert!(tail.read_appended().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }
}
