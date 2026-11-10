//! The agent cursor's platform-neutral half: which action currently owns the
//! cursor, how long it stays, and the path it travels. Each platform overlay
//! draws it and runs `QueuedCommand`s on its own UI thread.

pub(crate) mod motion;

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use self::motion::{Effect, Point, Waypoint};

static SUBMISSIONS: Mutex<Submissions> = Mutex::new(Submissions {
    enabled: false,
    revision: 0,
    owner: None,
});

// The producer and the overlay's UI thread share this ordering boundary.
// Invalidation cannot race a producer into enqueueing an obsolete action after
// the clear callback.
pub(crate) struct Submissions {
    enabled: bool,
    revision: u64,
    owner: Option<(u64, u64)>,
}

impl Submissions {
    pub(crate) fn reserve(
        &mut self,
        feedback: Option<crate::feedback::FeedbackTicket>,
        now: Instant,
    ) -> Publication {
        self.revision += 1;
        self.owner = feedback
            .as_ref()
            .map(|ticket| (ticket.owner, ticket.action));
        Publication {
            revision: self.revision,
            reserved_at: now,
            feedback,
        }
    }

    fn accepts(&self, queued: &QueuedCommand, now: Instant) -> bool {
        queued.publication.revision == self.revision
            && (matches!(queued.command, UiCommand::Clear)
                || (self.enabled && queued.publication.is_current(now)))
    }

    fn clear(
        &mut self,
        owner: Option<u64>,
        action: Option<u64>,
        now: Instant,
    ) -> Option<QueuedCommand> {
        if let Some(owner) = owner
            && !self.owner.is_some_and(|current| {
                current.0 == owner && action.is_none_or(|action| current.1 == action)
            })
        {
            return None;
        }
        Some(QueuedCommand {
            command: UiCommand::Clear,
            publication: self.reserve(None, now),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Publication {
    revision: u64,
    reserved_at: Instant,
    feedback: Option<crate::feedback::FeedbackTicket>,
}

impl Publication {
    /// A call's cursor stays for as long as the call runs, however long its
    /// actions and verification take, and then lingers. An action outside an
    /// MCP call is over once it is delivered, so it lingers from its start.
    pub(crate) fn is_current(&self, now: Instant) -> bool {
        match &self.feedback {
            None => now < self.reserved_at + LINGER,
            Some(ticket) => {
                ticket.is_current() && ticket.finished().is_none_or(|done| now < done + LINGER)
            }
        }
    }
}

pub(crate) struct QueuedCommand {
    pub(crate) command: UiCommand,
    pub(crate) publication: Publication,
}

pub(crate) enum UiCommand {
    Show {
        window_id: u32,
        point: Option<Point>,
        waypoints: Vec<Waypoint>,
    },
    Clear,
}

// How long the cursor rests on its last point once a call has finished. The
// agent's next act_ui only comes after it has read this result and replied,
// commonly a few seconds later, so a shorter rest would fade the cursor out
// and back in between the calls of one task; it still leaves soon after the
// agent stops.
pub(crate) const LINGER: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayActionKind {
    Click,
    Point,
}

/// `enqueue` runs under the submission lock, so the clear it carries is
/// ordered before any later action.
pub(crate) fn set_enabled(on: bool, enqueue: impl FnOnce(QueuedCommand)) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    let was_enabled = submissions.enabled;
    submissions.enabled = on;
    if was_enabled
        && !on
        && let Some(command) = submissions.clear(None, None, Instant::now())
    {
        enqueue(command);
    }
}

pub(crate) fn is_enabled() -> bool {
    SUBMISSIONS.lock().unwrap().enabled
}

// Reserve before the target's point lookup: an older, slow lookup cannot
// overwrite a newer action or a disable/cancel that arrived while the lookup
// was blocked.
pub(crate) fn begin_action(
    feedback: Option<crate::feedback::FeedbackTicket>,
) -> Option<Publication> {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    (submissions.enabled && feedback.as_ref().is_none_or(|ticket| ticket.is_current()))
        .then(|| submissions.reserve(feedback, Instant::now()))
}

pub(crate) fn clear(owner: Option<u64>, action: Option<u64>, enqueue: impl FnOnce(QueuedCommand)) {
    let mut submissions = SUBMISSIONS.lock().unwrap();
    if let Some(command) = submissions.clear(owner, action, Instant::now()) {
        enqueue(command);
    }
}

/// Runs `command` on the overlay's UI thread unless a newer action, a clear
/// or a disable has superseded it, or its lifetime ran out in the queue.
pub(crate) fn run_accepted(
    command: QueuedCommand,
    now: Instant,
    run: impl FnOnce(UiCommand, Publication),
) {
    let submissions = SUBMISSIONS.lock().unwrap();
    if submissions.accepts(&command, now) {
        run(command.command, command.publication);
    }
}

/// Released once the cursor has reached an action's point, or at once when
/// nothing will be drawn there, so input is delivered where the user just saw
/// the cursor arrive.
pub(crate) struct CursorArrival(Option<Receiver<()>>);

// Bounds the wait if the UI thread is stalled; the cursor's longest planned
// path is well inside it.
const MAX_ARRIVAL_WAIT: Duration = Duration::from_secs(2);

impl CursorArrival {
    pub(crate) fn none() -> Self {
        Self(None)
    }

    pub(crate) fn wait(self) {
        if let Some(arrival) = self.0 {
            let _ = arrival.recv_timeout(MAX_ARRIVAL_WAIT);
        }
    }
}

fn awaited(waypoint: &mut Waypoint, paced: bool) -> CursorArrival {
    if !paced {
        return CursorArrival::none();
    }
    let (reached, arrival) = sync_channel(1);
    waypoint.reached = Some(reached);
    CursorArrival(Some(arrival))
}

pub(crate) fn is_finite_point(point: Point) -> bool {
    point.0.is_finite() && point.1.is_finite()
}

/// An action without a point renews the cursor where it already is. `None`
/// when the point is not drawable.
pub(crate) fn action(
    publication: Publication,
    window_id: u32,
    kind: OverlayActionKind,
    point: Option<Point>,
    paced: bool,
) -> Option<(QueuedCommand, CursorArrival)> {
    let Some(point) = point else {
        return Some((
            QueuedCommand {
                publication,
                command: UiCommand::Show {
                    window_id,
                    point: None,
                    waypoints: Vec::new(),
                },
            },
            CursorArrival::none(),
        ));
    };
    if !is_finite_point(point) {
        return None;
    }
    let effect = (kind == OverlayActionKind::Click).then_some(Effect::Click);
    let mut waypoint = Waypoint::new(point, effect);
    let arrival = awaited(&mut waypoint, paced);
    Some((
        QueuedCommand {
            publication,
            command: UiCommand::Show {
                window_id,
                point: Some(point),
                waypoints: vec![waypoint],
            },
        },
        arrival,
    ))
}

/// The returned arrival is the drag's start: the drag itself is delivered
/// while the cursor travels it.
pub(crate) fn drag(
    publication: Publication,
    window_id: u32,
    from: Point,
    to: Point,
    paced: bool,
) -> Option<(QueuedCommand, CursorArrival)> {
    if !is_finite_point(from) || !is_finite_point(to) {
        return None;
    }
    let mut start = Waypoint::new(from, Some(Effect::Press));
    let arrival = awaited(&mut start, paced);
    Some((
        QueuedCommand {
            publication,
            command: UiCommand::Show {
                window_id,
                point: Some(to),
                waypoints: vec![start, Waypoint::new(to, Some(Effect::Release))],
            },
        },
        arrival,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Submissions {
        Submissions {
            enabled: true,
            revision: 0,
            owner: None,
        }
    }

    fn action(publication: Publication) -> QueuedCommand {
        QueuedCommand {
            publication,
            command: UiCommand::Show {
                window_id: 7,
                point: Some((10.0, 20.0)),
                waypoints: Vec::new(),
            },
        }
    }

    #[test]
    fn delayed_callbacks_cannot_restore_cleared_superseded_or_expired_feedback() {
        let now = Instant::now();
        let mut submissions = enabled();
        let first = action(submissions.reserve(None, now));
        assert!(submissions.accepts(&first, now));
        let clear = submissions.clear(None, None, now).unwrap();
        assert!(!submissions.accepts(&first, now));
        assert!(submissions.accepts(&clear, now));
        let newer = action(submissions.reserve(None, now));
        assert!(
            !submissions.accepts(&clear, now),
            "late clear cannot erase newer feedback"
        );
        assert!(submissions.accepts(&newer, now));
        assert!(
            !submissions.accepts(&newer, now + LINGER),
            "queue delay consumes lifetime"
        );
        submissions.enabled = false;
        let _disable = submissions.clear(None, None, now);
        submissions.enabled = true;
        assert!(
            !submissions.accepts(&newer, now),
            "re-enable cannot revive pre-disable work"
        );
    }

    #[test]
    fn cancelling_one_session_does_not_clear_another_sessions_newer_feedback() {
        let now = Instant::now();
        let mut submissions = enabled();
        let first = crate::feedback::FeedbackSession::new();
        let second = crate::feedback::FeedbackSession::new();
        let old_run = first.begin(None);
        let current_run = second.begin(None);
        let old = action(submissions.reserve(Some(old_run.ticket()), now));
        let current = action(submissions.reserve(Some(current_run.ticket()), now));
        first.cancel();
        assert!(!submissions.accepts(&old, now));
        assert!(
            submissions
                .clear(Some(old_run.ticket().owner), None, now)
                .is_none()
        );
        assert!(submissions.accepts(&current, now));
        second.cancel();
        assert!(
            !submissions.accepts(&current, now),
            "even an undelivered cancel callback invalidates the marker"
        );
        let restarted = second.begin(None);
        let current = action(submissions.reserve(Some(restarted.ticket()), now));
        assert!(
            submissions
                .clear(
                    Some(current_run.ticket().owner),
                    Some(current_run.ticket().action),
                    now
                )
                .is_none(),
            "a dropped older request cannot clear its successor"
        );
        assert!(submissions.accepts(&current, now));
        drop(second);
        assert!(
            !submissions.accepts(&current, now),
            "actual service teardown invalidates queued publication"
        );
    }
}
