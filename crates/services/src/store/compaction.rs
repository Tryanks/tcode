use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::ops::Range;
use std::path::Path;

use tcode_core::session::{CompactedRecord, StoredEvent, Timeline, compact_records};
use tcode_protocol::{MAX_SESSION_HISTORY_BYTES, SESSION_WINDOW_BYTES};

use super::{LayoutHeader, SessionStore, encode_record, layout_epoch, parse_stored_line};

/// The most delta text one merged record carries: no more than one history
/// reply aims for, so a merged record never crowds out its neighbours.
const MAX_MERGED_TEXT: usize = SESSION_WINDOW_BYTES;

// JSON escapes a control character as six bytes, so even the worst merged
// text crosses as one history reply.
const _: () = assert!(6 * MAX_MERGED_TEXT < MAX_SESSION_HISTORY_BYTES);

#[derive(Debug)]
pub enum CompactOutcome {
    /// The log now holds the compacted records under the next layout epoch:
    /// `before` bytes became `after`.
    Rewritten { before: u64, after: u64 },
    /// Nothing compacts; the log was not touched.
    Unchanged,
    /// The log was left byte for byte as it was.
    Rejected(CompactRejection),
}

#[derive(Debug)]
pub enum CompactRejection {
    /// A line of the log is not a complete record: `line` is 1-based.
    Malformed {
        line: usize,
        reason: MalformedLine,
    },
    /// The compacted records do not fold as the log does.
    Gate,
    Io(std::io::Error),
}

#[derive(Debug)]
pub enum MalformedLine {
    NotUtf8,
    /// The log does not end in a line break: its last write was cut short.
    Truncated,
    Blank,
    Unparseable,
}

/// A log read strictly: every line is a record, except a layout header first.
struct StrictLog {
    epoch: u64,
    records: Vec<StoredEvent>,
    /// Where each record's line sits in the log, line break excluded.
    lines: Vec<Range<usize>>,
}

impl SessionStore {
    /// Rewrite a session's log so that it holds fewer records that fold
    /// exactly as the old ones do ([`compact_records`]), as a whole new file
    /// renamed over the old one. Any line that is not a complete record
    /// leaves the log untouched, as does a result that does not fold equal
    /// to the log. The first rewrite keeps the log as it was in
    /// `<id>.jsonl.orig`, and every rewrite bumps the layout epoch.
    ///
    /// Appends that land between the read and the rename are lost, so the
    /// caller must not run it beside a writer of the same log.
    pub fn compact_log(&self, id: &str) -> CompactOutcome {
        let path = self.events_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return CompactOutcome::Rejected(CompactRejection::Io(error)),
        };
        let log = match read_strict(&bytes) {
            Ok(log) => log,
            Err((line, reason)) => {
                return CompactOutcome::Rejected(CompactRejection::Malformed { line, reason });
            }
        };
        let original = Timeline::fold_stored(&log.records);
        let compacted = compact_records(&log.records, MAX_MERGED_TEXT);
        if compacted.len() == log.records.len() {
            return CompactOutcome::Unchanged;
        }

        let epoch = log.epoch + 1;
        let mut body = serde_json::to_vec(&LayoutHeader {
            layout_epoch: epoch,
        })
        .expect("serializable header");
        body.push(b'\n');
        let mut fold = Timeline::default();
        for record in &compacted {
            match record {
                CompactedRecord::Kept(index) => {
                    body.extend_from_slice(&bytes[log.lines[*index].clone()]);
                    fold.apply_stored(&log.records[*index]);
                }
                // The gate folds what will be read back, not what was meant.
                CompactedRecord::Merged(merged) => {
                    let line = match encode_stored(merged) {
                        Ok(line) => line,
                        Err(error) => return CompactOutcome::Rejected(CompactRejection::Io(error)),
                    };
                    let Ok(read_back) = parse_stored_line(&line) else {
                        return CompactOutcome::Rejected(CompactRejection::Gate);
                    };
                    body.extend_from_slice(line.as_bytes());
                    fold.apply_stored(&read_back);
                }
            }
            body.push(b'\n');
        }
        if fold != original {
            return CompactOutcome::Rejected(CompactRejection::Gate);
        }
        if let Err(error) = self.install_compacted(id, &body, log.epoch == 0) {
            return CompactOutcome::Rejected(CompactRejection::Io(error));
        }
        CompactOutcome::Rewritten {
            before: bytes.len() as u64,
            after: body.len() as u64,
        }
    }

    /// The layout epoch the session's log declares, read from its first line
    /// alone: 0 for a log never rewritten, or one that is missing.
    pub fn log_epoch(&self, id: &str) -> u64 {
        let Ok(file) = File::open(self.events_path(id)) else {
            return 0;
        };
        let mut first = String::new();
        match BufReader::new(file).read_line(&mut first) {
            Ok(_) => layout_epoch(first.trim()).unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Delete what rewrites leave beside the logs: the originals they kept,
    /// and the temporary files of rewrites a crash cut short. An original is
    /// kept until the next successful start, so this runs before the run
    /// rewrites anything; it would delete the run's own originals after that.
    /// Every file is attempted; the first failure is returned.
    pub fn discard_compaction_leftovers(&self) -> std::io::Result<()> {
        let mut result = Ok(());
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let leftover = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    [".jsonl.orig", ".jsonl.compacting", ".jsonl.orig.tmp"]
                        .iter()
                        .any(|suffix| name.ends_with(suffix))
                });
            if leftover
                && let Err(error) = fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }

    /// Durably replace the log with `body`, first keeping the log as it is
    /// now as the original when asked to and none is kept yet.
    fn install_compacted(&self, id: &str, body: &[u8], keep_original: bool) -> std::io::Result<()> {
        let path = self.events_path(id);
        let temporary = path.with_extension("jsonl.compacting");
        let installed = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(body)?;
            file.sync_all()?;
            drop(file);
            if keep_original {
                keep(&path, &self.original_events_path(id))?;
                sync_directory(&self.root)?;
            }
            fs::rename(&temporary, &path)?;
            sync_directory(&self.root)
        })();
        if installed.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        installed
    }
}

/// Keep `path` as it is now at `original`, unless an original is already
/// there: an earlier rewrite that never got to its rename already kept the
/// log, and nothing has been rewritten since.
fn keep(path: &Path, original: &Path) -> std::io::Result<()> {
    match fs::hard_link(path, original) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        // A file system without hard links gets a copy instead.
        Err(_) => {
            let copy = original.with_extension("orig.tmp");
            fs::copy(path, &copy)?;
            File::open(&copy)?.sync_all()?;
            fs::rename(copy, original)
        }
    }
}

/// Make the directory's entries durable: a rename is not until its directory
/// is synced. Windows offers no directory handle to sync.
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

/// Encode a record in the form it was read in: an envelope, or the legacy
/// bare event when it has no time.
fn encode_stored(record: &StoredEvent) -> std::io::Result<String> {
    match record.ts {
        Some(ts) => encode_record(ts, &record.event),
        None => serde_json::to_string(&record.event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    }
}

/// Read `bytes` whole, or name the first line (1-based) that is not a record.
fn read_strict(bytes: &[u8]) -> Result<StrictLog, (usize, MalformedLine)> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        let line = bytes[..error.valid_up_to()]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        (line + 1, MalformedLine::NotUtf8)
    })?;
    let mut log = StrictLog {
        epoch: 0,
        records: Vec::new(),
        lines: Vec::new(),
    };
    let mut start = 0;
    for (number, line) in text.split_inclusive('\n').enumerate() {
        let range = start..start + line.len();
        start = range.end;
        let Some(line) = line.strip_suffix('\n') else {
            return Err((number + 1, MalformedLine::Truncated));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err((number + 1, MalformedLine::Blank));
        }
        if number == 0
            && let Some(epoch) = layout_epoch(trimmed)
        {
            log.epoch = epoch;
            continue;
        }
        let record =
            parse_stored_line(trimmed).map_err(|_| (number + 1, MalformedLine::Unparseable))?;
        log.records.push(record);
        log.lines.push(range.start..range.end - 1);
    }
    Ok(log)
}

#[cfg(test)]
mod tests {
    use agent::{AgentEvent, DeltaKind, ItemContent, ThreadItem, TurnStatus};

    use super::*;

    fn temp_store() -> SessionStore {
        let root = std::env::temp_dir().join(format!("tcode-compact-{}", uuid::Uuid::new_v4()));
        SessionStore::open_at(root).unwrap()
    }

    fn at(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    /// A turn whose answer streamed as three deltas, from `ts` on.
    fn streamed_turn(ts: u64) -> Vec<StoredEvent> {
        let delta = |text: &str| AgentEvent::Delta {
            item_id: format!("answer-{ts}"),
            kind: DeltaKind::AssistantText,
            text: text.into(),
        };
        vec![
            at(
                ts,
                AgentEvent::TurnStarted {
                    turn_id: format!("turn-{ts}"),
                },
            ),
            at(ts + 1, delta("one ")),
            at(ts + 2, delta("two ")),
            at(ts + 3, delta("three")),
            at(
                ts + 4,
                AgentEvent::ItemCompleted(ThreadItem {
                    id: format!("answer-{ts}"),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: "one two three".into(),
                    },
                }),
            ),
            at(
                ts + 5,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{ts}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ),
        ]
    }

    fn append(store: &SessionStore, id: &str, records: &[StoredEvent]) {
        for record in records {
            store
                .append_event(id, record.ts.unwrap(), &record.event)
                .unwrap();
        }
    }

    /// The tolerant reader skips what it cannot parse; a rewrite from such a
    /// read would silently delete those lines.
    #[test]
    fn a_log_with_any_line_that_is_not_a_record_is_left_untouched() {
        let turn = streamed_turn(1);
        let records =
            crate::store::encode_event_log(turn.iter().map(|r| (r.ts.unwrap(), &r.event))).unwrap();
        // The six records with `line` inserted before the record at `index`.
        let inserted = |index: usize, line: &[u8]| -> Vec<u8> {
            let at = records
                .split_inclusive(|byte| *byte == b'\n')
                .take(index)
                .map(<[u8]>::len)
                .sum();
            [&records[..at], line, &records[at..]].concat()
        };
        let class = |reason: &MalformedLine| match reason {
            MalformedLine::NotUtf8 => "not UTF-8",
            MalformedLine::Truncated => "truncated",
            MalformedLine::Blank => "blank",
            MalformedLine::Unparseable => "unparseable",
        };
        let cases = [
            (
                "cut short",
                records[..records.len() - 1].to_vec(),
                6,
                "truncated",
            ),
            (
                "unknown event",
                inserted(2, b"{\"ts\":9,\"event\":{\"type\":\"not_an_event\"}}\n"),
                3,
                "unparseable",
            ),
            ("blank line", inserted(1, b"\n"), 2, "blank"),
            ("not UTF-8", inserted(1, &[0xff]), 2, "not UTF-8"),
            (
                "layout header after the first line",
                inserted(1, b"{\"layout_epoch\":3}\n"),
                2,
                "unparseable",
            ),
        ];
        for (label, log, expected_line, expected_class) in cases {
            let store = temp_store();
            fs::write(store.events_path("s"), &log).unwrap();
            match store.compact_log("s") {
                CompactOutcome::Rejected(CompactRejection::Malformed { line, reason }) => {
                    assert_eq!(
                        (line, class(&reason)),
                        (expected_line, expected_class),
                        "{label}"
                    );
                }
                outcome => panic!("{label}: {outcome:?}"),
            }
            assert_eq!(fs::read(store.events_path("s")).unwrap(), log, "{label}");
            let files: Vec<_> = fs::read_dir(store.root()).unwrap().collect();
            assert_eq!(files.len(), 1, "{label}: nothing but the log");
            fs::remove_dir_all(store.root()).unwrap();
        }
    }

    /// A rewritten log carries its layout epoch in a first line that every
    /// reader of records skips; appends continue after it, and a clone starts
    /// in the same layout.
    #[test]
    fn a_rewritten_log_reads_appends_and_clones_under_its_layout_header() {
        let store = temp_store();
        let turn = streamed_turn(1);
        append(&store, "s", &turn);
        assert!(matches!(
            store.compact_log("s"),
            CompactOutcome::Rewritten { .. }
        ));
        let next = streamed_turn(10);
        append(&store, "s", &next[..1]);

        let mut merged = turn[1].clone();
        merged.event = AgentEvent::Delta {
            item_id: "answer-1".into(),
            kind: DeltaKind::AssistantText,
            text: "one two three".into(),
        };
        let expected = vec![
            turn[0].clone(),
            merged,
            turn[4].clone(),
            turn[5].clone(),
            next[0].clone(),
        ];
        assert_eq!(store.read_events("s"), expected);
        let log = fs::read_to_string(store.events_path("s")).unwrap();
        assert_eq!(log.lines().next(), Some(r#"{"layout_epoch":1}"#));
        assert_eq!(
            store.read_event_log("s").unwrap(),
            log.split_once('\n').unwrap().1.as_bytes()
        );

        store.clone_events("s", "fork").unwrap();
        assert_eq!(fs::read(store.events_path("fork")).unwrap(), log.as_bytes());
        assert_eq!(store.read_events("fork"), expected);
        fs::remove_dir_all(store.root()).unwrap();
    }

    /// The log as it was before its first rewrite is kept beside it; later
    /// rewrites never replace it, and removing the session removes it.
    #[test]
    fn the_first_rewrite_keeps_the_original_until_the_session_is_removed() {
        let store = temp_store();
        append(&store, "s", &streamed_turn(1));
        let original = fs::read(store.events_path("s")).unwrap();
        assert!(matches!(
            store.compact_log("s"),
            CompactOutcome::Rewritten { .. }
        ));
        assert_eq!(store.log_epoch("s"), 1);
        append(&store, "s", &streamed_turn(10));
        assert!(matches!(
            store.compact_log("s"),
            CompactOutcome::Rewritten { .. }
        ));
        assert_eq!(store.log_epoch("s"), 2);
        let compacted = fs::read(store.events_path("s")).unwrap();
        assert!(matches!(store.compact_log("s"), CompactOutcome::Unchanged));
        assert_eq!(fs::read(store.events_path("s")).unwrap(), compacted);
        assert_eq!(fs::read(store.original_events_path("s")).unwrap(), original);

        store.remove_session_logs(&["s".to_string()]).unwrap();
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
        fs::remove_dir_all(store.root()).unwrap();
    }
}
