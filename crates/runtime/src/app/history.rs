use super::*;
use std::ops::Range;
use tcode_protocol::{HostMessage, MAX_SESSION_HISTORY_BYTES, SESSION_HISTORY_RECORDS};

/// Count serialized bytes without allocating a second copy of a large record.
struct ByteBudget(usize);

impl std::io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "history byte budget exhausted",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Preserve contiguous absolute cursors while fitting the complete wire envelope.
fn bounded_range(
    records: &[SessionEventRecord],
    requested: Range<usize>,
    backwards: bool,
    overhead: usize,
) -> Result<Range<usize>, tcode_protocol::ProtocolError> {
    let mut budget = ByteBudget(MAX_SESSION_HISTORY_BYTES.saturating_sub(overhead));
    let mut count = 0;
    for offset in 0..requested.len() {
        let index = if backwards {
            requested.end - 1 - offset
        } else {
            requested.start + offset
        };
        if budget.0 == 0 {
            break;
        }
        budget.0 -= 1; // Record separator; also leaves room for an empty array.
        if serde_json::to_writer(&mut budget, &records[index]).is_err() {
            break;
        }
        count += 1;
    }
    if count == 0 && !requested.is_empty() {
        return Err(tcode_protocol::ProtocolError {
            code: "history_record_too_large".into(),
            message: "A history record exceeds the 8 MiB response limit.".into(),
        });
    }
    Ok(if backwards {
        requested.end - count..requested.end
    } else {
        requested.start..requested.start + count
    })
}

impl AppState {
    fn history_records(&self, session_id: &str) -> std::borrow::Cow<'_, [SessionEventRecord]> {
        self.event_records.get(session_id).map_or_else(
            || std::borrow::Cow::Owned(self.store.read_events(session_id)),
            |records| std::borrow::Cow::Borrowed(records.as_slice()),
        )
    }

    pub(crate) fn session_events_snapshot(
        &self,
        subscription: &tcode_protocol::Subscription,
    ) -> ServerEvent {
        let Topic::SessionEvents { session_id } = &subscription.topic else {
            unreachable!()
        };
        let records = self.history_records(session_id);
        let total = records.len();
        let total_turns = self.resident(session_id).map_or_else(
            || Timeline::fold_events(records.iter().cloned()).turns.len(),
            |session| session.timeline.turns.len(),
        ) as u64;
        let after = subscription.after.filter(|after| *after <= total as u64);
        let from = after.map_or_else(|| total.saturating_sub(400), |after| after as usize);
        let empty = HostMessage::Event(EventEnvelope {
            request_id: Some(u64::MAX),
            topic: subscription.topic.clone(),
            event: ServerEvent::SessionSnapshot {
                from: u64::MAX,
                records: vec![],
                total: u64::MAX,
                total_turns: u64::MAX,
                truncated: false,
            },
        });
        let overhead = serde_json::to_vec(&empty)
            .expect("serializable snapshot")
            .len()
            + 1;
        match bounded_range(&records, from..total, after.is_none(), overhead) {
            Ok(range) => ServerEvent::SessionSnapshot {
                from: range.start as u64,
                truncated: range.len() < total - from,
                records: records[range].to_vec(),
                total: total as u64,
                total_turns,
            },
            Err(error) => ServerEvent::SessionHistoryError(error),
        }
    }

    pub(crate) fn session_history_page(
        &self,
        session_id: &str,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let records = self.history_records(session_id);
        let end = before.min(records.len() as u64) as usize;
        let count = (limit as usize).clamp(1, SESSION_HISTORY_RECORDS);
        let requested = end.saturating_sub(count)..end;
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::SessionHistoryPage {
                records: vec![],
                from: u64::MAX,
                truncated: false,
            }),
        };
        let overhead = serde_json::to_vec(&empty).expect("serializable page").len() + 1;
        let range = bounded_range(&records, requested.clone(), true, overhead)?;
        Ok(QueryResponse::SessionHistoryPage {
            from: range.start as u64,
            truncated: range.len() < requested.len(),
            records: records[range].to_vec(),
        })
    }
}
