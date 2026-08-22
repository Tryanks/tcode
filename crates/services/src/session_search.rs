//! Lazy full-text search over persisted session event logs.
//!
//! The index is deliberately in-memory: each session is parsed only when its
//! JSONL file's length or modification time changes. This keeps the append-only
//! persistence format authoritative and avoids a second on-disk database.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::SystemTime;

use agent::ItemContent;
use tcode_core::project::SessionMeta;
use tcode_core::session::{EntryContent, StoredEvent, Timeline};

use crate::store::SessionStore;

/// One final, folded timeline entry that contributes to content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableEntry {
    pub entry_id: String,
    pub turn: usize,
    pub text: String,
}

/// A content match suitable for presentation by a session picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub session_title: String,
    pub entry_id: String,
    pub turn: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct CachedSession {
    fingerprint: FileFingerprint,
    entries: Vec<SearchableEntry>,
}

/// Incremental, file-freshness-based content index for one [`SessionStore`].
pub struct SessionSearch {
    store: SessionStore,
    cache: HashMap<String, CachedSession>,
}

impl SessionSearch {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            cache: HashMap::new(),
        }
    }

    /// Search sessions in the supplied order, returning at most `limit` hits.
    /// Empty and whitespace-only queries intentionally return no content hits.
    pub fn search(
        &mut self,
        sessions: &[SessionMeta],
        query: &str,
        limit: usize,
    ) -> Vec<SessionSearchHit> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }

        let live_ids: HashSet<&str> = sessions.iter().map(|meta| meta.id.as_str()).collect();
        self.cache.retain(|id, _| live_ids.contains(id.as_str()));

        let mut hits = Vec::new();
        for meta in sessions {
            self.refresh(meta);
            let Some(cached) = self.cache.get(&meta.id) else {
                continue;
            };
            for entry in &cached.entries {
                let Some(snippet) = match_snippet(&entry.text, query, 140) else {
                    continue;
                };
                hits.push(SessionSearchHit {
                    session_id: meta.id.clone(),
                    session_title: meta.title.clone(),
                    entry_id: entry.entry_id.clone(),
                    turn: entry.turn,
                    snippet,
                });
                if hits.len() == limit {
                    return hits;
                }
            }
        }
        hits
    }

    fn refresh(&mut self, meta: &SessionMeta) {
        let path = self.store.root().join(format!("{}.jsonl", meta.id));
        let fingerprint = match fs::metadata(path) {
            Ok(metadata) => FileFingerprint {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileFingerprint {
                len: 0,
                modified: None,
            },
            Err(error) => {
                log::warn!("cannot inspect session log {}: {error}", meta.id);
                return;
            }
        };
        if self
            .cache
            .get(&meta.id)
            .is_some_and(|cached| cached.fingerprint == fingerprint)
        {
            return;
        }
        let entries = extract_searchable_entries(&self.store.read_events(&meta.id));
        self.cache.insert(
            meta.id.clone(),
            CachedSession {
                fingerprint,
                entries,
            },
        );
    }
}

/// Fold persisted events and extract the final searchable representation.
///
/// Folding first deduplicates streaming deltas and item lifecycle updates, so
/// callers index what the chat ultimately displays rather than every wire event.
pub fn extract_searchable_entries(events: &[StoredEvent]) -> Vec<SearchableEntry> {
    let timeline = Timeline::fold_events(events.iter().cloned());
    let mut entries = Vec::new();
    for entry in timeline
        .entries
        .iter()
        .chain(timeline.children.values().flatten())
    {
        let text = match &entry.content {
            EntryContent::Item(content) => searchable_item_text(content),
            EntryContent::Steer {
                text, attachments, ..
            } => Some(join_parts(
                std::iter::once(text.as_str()).chain(attachments.iter().map(String::as_str)),
            )),
            _ => None,
        };
        if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
            entries.push(SearchableEntry {
                entry_id: entry.id.clone(),
                turn: entry.turn,
                text,
            });
        }
    }
    entries.sort_by_key(|entry| entry.turn);
    entries
}

fn searchable_item_text(content: &ItemContent) -> Option<String> {
    match content {
        ItemContent::UserMessage {
            text, attachments, ..
        } => Some(join_parts(
            std::iter::once(text.as_str()).chain(attachments.iter().map(String::as_str)),
        )),
        ItemContent::AssistantMessage { text } => Some(text.clone()),
        ItemContent::CommandExecution { command, .. } => Some(command.clone()),
        ItemContent::FileChange { changes, .. } => Some(join_parts(
            changes.iter().map(|change| change.path.as_str()),
        )),
        ItemContent::ToolCall { name, input, .. } => {
            Some(format!("{name} {}", compact_json(input)))
        }
        ItemContent::Subagent {
            agent_type,
            description,
            summary,
            ..
        } => Some(join_parts(
            [agent_type.as_str(), description.as_str()]
                .into_iter()
                .chain(summary.iter().map(String::as_str)),
        )),
        ItemContent::WebSearch { query } => Some(query.clone()),
        ItemContent::Other {
            provider_kind,
            summary,
        } => Some(format!("{provider_kind} {summary}")),
        ItemContent::Reasoning { .. } => None,
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn join_parts<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Return a compact, whitespace-normalized snippet for a case-insensitive hit.
pub fn match_snippet(text: &str, query: &str, max_chars: usize) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let query = query.trim().to_lowercase();
    if normalized.is_empty() || query.is_empty() {
        return None;
    }
    let (match_start, match_end) = case_insensitive_range(&normalized, &query)?;
    let chars: Vec<char> = normalized.chars().collect();
    let start_char = normalized[..match_start].chars().count();
    let end_char = normalized[..match_end].chars().count();
    if chars.len() <= max_chars {
        return Some(normalized);
    }

    let match_len = end_char.saturating_sub(start_char);
    let context = max_chars.saturating_sub(match_len);
    let mut start = start_char.saturating_sub(context / 2);
    let end = (start + max_chars).min(chars.len());
    start = end.saturating_sub(max_chars);
    let mut snippet: String = chars[start..end].iter().collect();
    if start > 0 {
        snippet.insert(0, '…');
    }
    if end < chars.len() {
        snippet.push('…');
    }
    Some(snippet)
}

fn case_insensitive_range(text: &str, lower_query: &str) -> Option<(usize, usize)> {
    for (start, _) in text.char_indices() {
        let suffix = &text[start..];
        if !suffix.to_lowercase().starts_with(lower_query) {
            continue;
        }
        let mut folded_len = 0;
        let mut end = start;
        for ch in suffix.chars() {
            folded_len += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            end += ch.len_utf8();
            if folded_len >= lower_query.len() {
                return Some((start, end));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent::{AgentEvent, ItemStatus, ProviderKind, ThreadItem};
    use serde_json::json;
    use tcode_core::project::SessionMeta;

    use super::*;

    fn completed(id: &str, content: ItemContent) -> StoredEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
        .into()
    }

    #[test]
    fn extracts_user_and_assistant_messages() {
        let entries = extract_searchable_entries(&[
            completed(
                "user",
                ItemContent::UserMessage {
                    text: "Where is auth.rs?".into(),
                    context_len: None,
                    attachments: Vec::new(),
                },
            ),
            completed(
                "assistant",
                ItemContent::AssistantMessage {
                    text: "It is under crates/runtime.".into(),
                },
            ),
        ]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "Where is auth.rs?");
        assert_eq!(entries[1].text, "It is under crates/runtime.");
    }

    #[test]
    fn extracts_tool_titles_paths_and_commands() {
        let entries = extract_searchable_entries(&[
            completed(
                "command",
                ItemContent::CommandExecution {
                    command: "rg auth.rs crates".into(),
                    output: "large output is deliberately not indexed".into(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                },
            ),
            completed(
                "tool",
                ItemContent::ToolCall {
                    name: "read_file".into(),
                    input: json!({"path": "src/auth.rs"}),
                    output: None,
                    status: ItemStatus::Completed,
                },
            ),
        ]);
        assert_eq!(entries[0].text, "rg auth.rs crates");
        assert!(entries[1].text.contains("read_file"));
        assert!(entries[1].text.contains("src/auth.rs"));
    }

    #[test]
    fn query_matching_is_case_insensitive_and_generates_a_bounded_snippet() {
        let text = format!("{} AUTH.rs {}", "before ".repeat(20), "after ".repeat(20));
        let snippet = match_snippet(&text, "auth.RS", 60).expect("match");
        assert!(snippet.contains("AUTH.rs"));
        assert!(snippet.chars().count() <= 62); // up to two ellipses
        assert!(match_snippet(&text, "missing", 60).is_none());
    }

    #[test]
    fn searches_a_fixture_session_log_to_a_session_and_turn() {
        let root = std::env::temp_dir().join(format!(
            "tcode-session-search-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = SessionStore::open_at(root.clone()).expect("store");
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        meta.title = "Authentication cleanup".into();
        store
            .append_event(
                &meta.id,
                1,
                &AgentEvent::TurnStarted {
                    turn_id: "turn-1".into(),
                },
            )
            .unwrap();
        store
            .append_event(
                &meta.id,
                2,
                &completed(
                    "assistant",
                    ItemContent::AssistantMessage {
                        text: "I updated crates/runtime/src/auth.rs".into(),
                    },
                )
                .event,
            )
            .unwrap();

        let hits = SessionSearch::new(store).search(&[meta.clone()], "auth.rs", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, meta.id);
        assert_eq!(hits[0].turn, 0);
        assert!(hits[0].snippet.contains("auth.rs"));
        let _ = fs::remove_dir_all(root);
    }
}
