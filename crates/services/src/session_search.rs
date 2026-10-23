//! Full-text search over persisted session event logs.
//!
//! Each thread's searchable entries are extracted from its folded log once and
//! kept in the store with the identity of that log: a search folds only the
//! threads whose log changed since, and reads stored text for the rest.

use std::collections::HashMap;
use std::sync::Mutex;

use agent::ItemContent;
use tcode_core::project::SessionMeta;
use tcode_core::session::{EntryContent, StoredEvent, Timeline};
use tcode_protocol::SessionSearchHit;

use crate::store::{IndexedEntry, SessionStore};

/// The rules entries are extracted by. A change to them, or to what the fold
/// settles an entry to show, must change this so every thread is extracted
/// again.
const EXTRACTION_VERSION: u32 = 1;

/// One final, folded timeline entry that contributes to content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableEntry {
    pub entry_id: String,
    pub turn: usize,
    pub text: String,
}

/// Content search over one [`SessionStore`].
pub struct SessionSearch {
    store: SessionStore,
    /// The store's event generation of each thread whose entries were last
    /// found current, so a search compares a log's stored identity once per
    /// change to it in this process rather than on every query.
    verified: Mutex<HashMap<String, u64>>,
}

impl SessionSearch {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            verified: Mutex::default(),
        }
    }

    /// Search sessions in the supplied order, returning at most `limit` hits.
    /// Empty and whitespace-only queries intentionally return no content hits.
    pub fn search(
        &self,
        sessions: &[SessionMeta],
        query: &str,
        limit: usize,
    ) -> Vec<SessionSearchHit> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let folded = query.to_lowercase();
        let mut hits = Vec::new();
        for meta in sessions {
            if let Err(error) = self.refresh(&meta.id) {
                log::warn!("cannot search session {}: {error}", meta.id);
                continue;
            }
            let visited =
                self.store
                    .visit_search_matches(&meta.id, &folded, |entry_id, turn, text| {
                        if let Some(snippet) = match_snippet(text, query, 140) {
                            hits.push(SessionSearchHit {
                                session_id: meta.id.clone(),
                                session_title: meta.title.clone(),
                                entry_id: entry_id.to_owned(),
                                turn,
                                snippet,
                            });
                        }
                        hits.len() < limit
                    });
            if let Err(error) = visited {
                log::warn!("cannot search session {}: {error}", meta.id);
            }
            if hits.len() == limit {
                break;
            }
        }
        hits
    }

    /// Extract the entries of each thread in `ids` whose log changed since,
    /// stopping at the first store failure, which a search meets again.
    pub fn index(&self, ids: &[String]) {
        let started = std::time::Instant::now();
        let mut extracted = 0;
        for id in ids {
            match self.refresh(id) {
                Ok(fresh) => extracted += usize::from(fresh),
                Err(error) => {
                    log::warn!("search index of {id}: {error}");
                    return;
                }
            }
        }
        if extracted > 0 {
            log::info!(
                "search index: extracted {extracted} of {} threads in {} ms",
                ids.len(),
                started.elapsed().as_millis()
            );
        }
    }

    /// Extract `id`'s entries again unless they came from its log as it is,
    /// telling whether it did.
    fn refresh(&self, id: &str) -> std::io::Result<bool> {
        let generation = self.store.event_generation(id);
        let verified = |search: &Self| {
            search
                .verified
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.to_owned(), generation);
        };
        if self
            .verified
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            == Some(&generation)
        {
            return Ok(false);
        }
        if self.store.search_entries_current(id, EXTRACTION_VERSION)? {
            verified(self);
            return Ok(false);
        }
        let source = self.store.search_source(id)?;
        let events = self.store.read_log_until(id, source.end)?.records;
        let entries = extract_searchable_entries(&events);
        let folded: Vec<String> = entries.iter().map(|entry| fold(&entry.text)).collect();
        self.store.replace_search_entries(
            id,
            EXTRACTION_VERSION,
            &source,
            entries
                .iter()
                .zip(&folded)
                .map(|(entry, folded)| IndexedEntry {
                    entry_id: &entry.entry_id,
                    turn: entry.turn,
                    text: &entry.text,
                    folded,
                }),
        )?;
        verified(self);
        Ok(true)
    }
}

/// The text a query is matched against: whitespace collapsed as in a
/// snippet, then lowercased as the query is.
fn fold(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Fold persisted events and extract the final searchable representation.
///
/// Folding first deduplicates streaming deltas and item lifecycle updates, so
/// callers index what the chat ultimately displays rather than every wire event.
pub fn extract_searchable_entries(events: &[StoredEvent]) -> Vec<SearchableEntry> {
    let timeline = Timeline::fold_events(events.iter().cloned());
    let mut entries = Vec::new();
    for entry in &timeline.entries {
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
        ItemContent::ToolCall { name, input, .. } => Some(format!("{name} {input}")),
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
        ItemContent::ImageRead { path, .. } => Some(path.clone()),
        ItemContent::WebSearch { query } => Some(query.clone()),
        ItemContent::Other {
            provider_kind,
            summary,
        } => Some(format!("{provider_kind} {summary}")),
        ItemContent::Reasoning { .. } => None,
    }
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

    use crate::store::Mutation;

    use super::*;

    fn completed(id: &str, content: ItemContent) -> StoredEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
        .into()
    }

    /// What a search extracts is kept in the store: a later search, after the
    /// host restarts included, reads it instead of folding the log again,
    /// until the log changes.
    #[test]
    fn extracted_entries_outlive_the_host_that_made_them() {
        let root = std::env::temp_dir().join(format!("tcode-search-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        let reply = |id: &str, text: &str| {
            completed(id, ItemContent::AssistantMessage { text: text.into() }).event
        };
        store
            .append_event(&meta.id, 1, &reply("first", "the auth.rs fix"))
            .unwrap();
        let sessions = std::slice::from_ref(&meta);
        assert_eq!(
            SessionSearch::new(store.clone())
                .search(sessions, "auth.rs", 10)
                .len(),
            1
        );
        store.close().unwrap();
        drop(store);

        let store = SessionStore::open_at(root.clone()).unwrap();
        let search = SessionSearch::new(store.clone());
        assert_eq!(search.search(sessions, "AUTH.RS", 10)[0].entry_id, "first");
        assert_eq!(store.event_reads(), 0, "the restarted host read no log");
        store
            .append_event(&meta.id, 2, &reply("second", "auth.rs again"))
            .unwrap();
        assert_eq!(search.search(sessions, "auth.rs", 10).len(), 2);
        assert_eq!(store.event_reads(), 1, "the changed log was folded again");
        store.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn search_indexes_final_visible_content_and_refreshes_after_append_or_removal() {
        let root = std::env::temp_dir().join(format!("tcode-search-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        meta.title = "Authentication cleanup".into();
        let search = SessionSearch::new(store.clone());
        let sessions = std::slice::from_ref(&meta);
        assert!(search.search(sessions, "auth.rs", 10).is_empty());

        let events = [
            AgentEvent::TurnStarted {
                turn_id: "first".into(),
            },
            completed(
                "user",
                ItemContent::UserMessage {
                    text: "Where is auth.rs?".into(),
                    context_len: None,
                    attachments: vec!["diagram.png".into()],
                },
            )
            .event,
            completed(
                "command",
                ItemContent::CommandExecution {
                    command: "rg auth.rs crates".into(),
                    output: "DO_NOT_INDEX output".into(),
                    exit_code: Some(0),
                    status: ItemStatus::Completed,
                },
            )
            .event,
            completed(
                "tool",
                ItemContent::ToolCall {
                    image_reads: Vec::new(),
                    name: "read_file".into(),
                    input: json!({"path":"auth.rs"}),
                    output: Some("DO_NOT_INDEX output".into()),
                    status: ItemStatus::Completed,
                },
            )
            .event,
            completed(
                "reasoning",
                ItemContent::Reasoning {
                    text: "DO_NOT_INDEX reasoning".into(),
                },
            )
            .event,
            AgentEvent::ItemStarted(ThreadItem {
                id: "assistant".into(),
                parent_item_id: None,
                content: ItemContent::AssistantMessage {
                    text: "obsolete draft".into(),
                },
            }),
            completed(
                "assistant",
                ItemContent::AssistantMessage {
                    text: "I updated crates/runtime/src/auth.rs".into(),
                },
            )
            .event,
            AgentEvent::TurnCompleted {
                turn_id: "first".into(),
                status: agent::TurnStatus::Completed,
                usage: None,
            },
        ];
        for event in &events {
            store.append_event(&meta.id, 1, event).unwrap();
        }
        let hits = search.search(sessions, " AUTH.RS ", 10);
        assert_eq!(
            hits.iter()
                .map(|hit| hit.entry_id.as_str())
                .collect::<Vec<_>>(),
            ["user", "command", "tool", "assistant"]
        );
        assert!(hits.iter().all(|hit| hit.session_id == meta.id
            && hit.session_title == meta.title
            && hit.turn == 0));
        assert_eq!(hits[3].snippet, "I updated crates/runtime/src/auth.rs");
        assert_eq!(search.search(sessions, "auth.rs", 2).len(), 2);
        assert_eq!(
            search.search(sessions, "diagram.png", 10)[0].entry_id,
            "user"
        );
        for query in ["DO_NOT_INDEX", "obsolete draft", "  "] {
            assert!(search.search(sessions, query, 10).is_empty(), "{query}");
        }
        assert!(search.search(sessions, "auth.rs", 0).is_empty());

        store
            .append_event(
                &meta.id,
                2,
                &AgentEvent::TurnStarted {
                    turn_id: "second".into(),
                },
            )
            .unwrap();
        store
            .append_event(
                &meta.id,
                3,
                &completed(
                    "next",
                    ItemContent::AssistantMessage {
                        text: "auth.rs verified".into(),
                    },
                )
                .event,
            )
            .unwrap();
        let updated = search.search(sessions, "auth.rs", 10);
        assert_eq!(updated.len(), 5);
        assert_eq!(updated[4].entry_id, "next");
        assert_eq!(updated[4].turn, 1);
        // A replaced log of the same shape (line count, length) is still seen
        // as changed.
        let replaced = String::from_utf8(store.read_event_log(&meta.id).unwrap())
            .unwrap()
            .replace("auth.rs", "xyzw.rs");
        store
            .apply(&[Mutation::replace_event_log(&meta.id, replaced.into_bytes())])
            .unwrap();
        assert!(search.search(sessions, "auth.rs", 10).is_empty());
        assert_eq!(search.search(sessions, "xyzw.rs", 10).len(), 5);
        store.remove_session(&meta.id).unwrap();
        assert!(search.search(sessions, "xyzw.rs", 10).is_empty());
        for (text, query, expected) in [
            ("  中文\nCAFÉ\t😀 ", " café ", Some("中文 CAFÉ 😀")),
            ("İstanbul", "i\u{307}stan", Some("İstanbul")),
            ("中文", "文", Some("中文")),
            ("visible", "  ", None),
            ("   ", "visible", None),
            ("visible", "missing", None),
        ] {
            let snippet_meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            store
                .append_event(
                    &snippet_meta.id,
                    4,
                    &completed(
                        "snippet",
                        ItemContent::AssistantMessage { text: text.into() },
                    )
                    .event,
                )
                .unwrap();
            let hits = search.search(std::slice::from_ref(&snippet_meta), query, 10);
            assert_eq!(
                hits.iter()
                    .map(|hit| hit.snippet.as_str())
                    .collect::<Vec<_>>(),
                expected.into_iter().collect::<Vec<_>>(),
                "{text:?}: {query:?}"
            );
        }
        let snippet_meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        store
            .append_event(
                &snippet_meta.id,
                5,
                &completed(
                    "long",
                    ItemContent::AssistantMessage {
                        text: format!("{} AUTH.rs {}", "before ".repeat(40), "after ".repeat(40)),
                    },
                )
                .event,
            )
            .unwrap();
        let hits = search.search(std::slice::from_ref(&snippet_meta), "auth.RS", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("AUTH.rs"));
        assert!(hits[0].snippet.chars().count() <= 142);
        assert!(hits[0].snippet.starts_with('…') && hits[0].snippet.ends_with('…'));
        std::fs::remove_dir_all(root).unwrap();
    }
}
