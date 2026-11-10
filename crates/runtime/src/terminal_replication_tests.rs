//! The replicated terminal grid, exercised through the production pipe and the
//! fan-out mux, plus controlled projection timing for the burst wire budget.
//!
//! Every assertion here compares a client's reconstructed model with the host
//! emulator's own grid, which is the whole point of the projection: a
//! subscriber that attaches at any moment must see what the host sees.

use std::time::{Duration, Instant};

use super::p4b_tests::{fixture, linked, next, observed_events};
use super::*;
use tcode_protocol::terminal::{
    CellWidth, HISTORY_LIMIT, TerminalCell, TerminalFrame, TerminalLink, TerminalRow, TerminalStyle,
};
use tcode_traverse::HostMux;

/// A client's replicated grid: the subscription frame plus every delta since.
struct Replica {
    frame: TerminalFrame,
    events: async_channel::Receiver<EventEnvelope>,
    terminal_id: u64,
}

impl Replica {
    /// Subscribe and wait for the frame the host publishes on attach.
    fn attach(
        link: &HostLink,
        events: async_channel::Receiver<EventEnvelope>,
        terminal_id: u64,
    ) -> Self {
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::Terminal { terminal_id },
            after: None,
        })
        .unwrap();
        let ServerEvent::TerminalFrame { frame, .. } = next(&events, |event| {
            matches!(event, ServerEvent::TerminalFrame { .. })
        }) else {
            unreachable!()
        };
        Self {
            frame: *frame,
            events,
            terminal_id,
        }
    }

    /// Apply one delivered event. A frame mid-stream replaces the model; that
    /// only happens on a restart or a style-table rebuild.
    fn apply(&mut self, envelope: EventEnvelope) {
        match envelope.event {
            ServerEvent::TerminalDelta {
                terminal_id,
                ref delta,
            } if terminal_id == self.terminal_id => self.frame.apply(delta),
            ServerEvent::TerminalFrame {
                terminal_id,
                ref frame,
            } if terminal_id == self.terminal_id => self.frame = (**frame).clone(),
            _ => {}
        }
    }

    /// Apply everything already delivered.
    fn pump(&mut self) {
        while let Ok(envelope) = self.events.try_recv() {
            self.apply(envelope);
        }
    }

    /// Wait for the next event the host delivers, and apply it.
    fn next(&mut self) {
        let envelope = self
            .events
            .recv_blocking()
            .expect("the host stopped delivering events");
        self.apply(envelope);
    }

    /// Take the host's events until the replica matches the host grid. A
    /// replica that differs is owed a delta, so each miss waits for the next.
    fn settle(&mut self, terminal: &term::Terminal) {
        loop {
            self.pump();
            if difference(&self.frame, &host_frame(terminal)).is_none() {
                return;
            }
            self.next();
        }
    }
}

/// Project the host emulator's screen exactly as the runtime does, without
/// consuming the damage the live projection depends on. A peek carries no
/// scrollback, so the replica's history is checked on its own.
fn host_frame(terminal: &term::Terminal) -> TerminalFrame {
    let snapshot = terminal.peek_snapshot();
    let mut projector = term::Projector::default();
    let visible = (0..snapshot.screen_lines)
        .map(|row| projector.visible_row(&snapshot, row))
        .collect();
    TerminalFrame {
        cols: snapshot.cols as u16,
        rows: snapshot.screen_lines as u16,
        modes: term::project::modes(&snapshot, terminal.modify_other_keys()),
        cursor: term::project::cursor(&snapshot),
        styles: projector.into_styles(),
        visible,
        lines_evicted: snapshot.lines_evicted,
        title: snapshot.title.clone(),
        exited: snapshot.exited,
        exit_code: snapshot.exit_code,
        ..TerminalFrame::default()
    }
}

/// A cell resolved out of its message-local style table, so two frames built by
/// different projectors compare by value.
type Resolved = (String, CellWidth, Option<TerminalLink>, TerminalStyle);

fn resolved_row(frame: &TerminalFrame, row: &TerminalRow) -> (bool, Vec<Resolved>) {
    let blank = TerminalCell::default();
    let cells = (0..usize::from(frame.cols))
        .map(|col| {
            let cell = row.cells.get(col).unwrap_or(&blank);
            (
                cell.text.clone(),
                cell.width,
                cell.link.clone(),
                frame.style(cell),
            )
        })
        .collect();
    (row.wrapped, cells)
}

/// The first way two grids differ on screen, or `None` when they are identical
/// in every replicated dimension but scrollback.
fn difference(replica: &TerminalFrame, host: &TerminalFrame) -> Option<String> {
    if (replica.cols, replica.rows) != (host.cols, host.rows) {
        return Some(format!(
            "dimensions {:?} != {:?}",
            (replica.cols, replica.rows),
            (host.cols, host.rows)
        ));
    }
    if replica.modes != host.modes {
        return Some(format!("modes {:?} != {:?}", replica.modes, host.modes));
    }
    if replica.cursor != host.cursor {
        return Some(format!("cursor {:?} != {:?}", replica.cursor, host.cursor));
    }
    if replica.exited != host.exited || replica.exit_code != host.exit_code {
        return Some("exit state differs".into());
    }
    if replica.title != host.title {
        return Some(format!("title {:?} != {:?}", replica.title, host.title));
    }
    if replica.visible.len() != host.visible.len() {
        return Some(format!(
            "visible rows {} != {}",
            replica.visible.len(),
            host.visible.len()
        ));
    }
    for (index, (left, right)) in replica.visible.iter().zip(&host.visible).enumerate() {
        let (left, right) = (resolved_row(replica, left), resolved_row(host, right));
        if left != right {
            return Some(format!(
                "row {index}\n replica: {:?}\n    host: {:?}",
                row_text(&left.1),
                row_text(&right.1)
            ));
        }
    }
    None
}

fn row_text(cells: &[Resolved]) -> String {
    cells
        .iter()
        .filter(|(_, width, _, _)| *width != CellWidth::Spacer)
        .map(|(text, _, _, _)| if text.is_empty() { " " } else { text })
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn frame_text(frame: &TerminalFrame) -> Vec<String> {
    frame
        .visible
        .iter()
        .map(|row| row_text(&resolved_row(frame, row).1))
        .collect()
}

struct Session {
    _host: SpawnedHost,
    mux: HostMux,
    link: HostLink,
    /// This link's replica of the terminal, the only consumer of its events;
    /// clones would compete for them.
    watch: Replica,
    session_id: String,
    terminal_id: u64,
    terminal: Arc<term::Terminal>,
}

/// Open a terminal panel and take a host handle for the PTY behind it.
fn terminal_session() -> Session {
    let (host, mux, link, session_id) = fixture();
    let events = observed_events(&link);
    link.command_blocking(Command::ToggleTerminalPanel {
        session_id: session_id.clone(),
    })
    .unwrap();
    let ServerEvent::SessionStatusReplaced(status) = next(
        &events,
        |event| matches!(event, ServerEvent::SessionStatusReplaced(status) if !status.terminals.is_empty()),
    ) else {
        unreachable!()
    };
    let terminal_id = status.terminals[0].id;
    let terminal = smol::block_on(
        host.update_state_for_test(move |state, _| state.terminal_handle(terminal_id)),
    )
    .unwrap()
    .expect("a spawned terminal");
    let watch = Replica::attach(&link, events, terminal_id);
    Session {
        _host: host,
        mux,
        link,
        watch,
        session_id,
        terminal_id,
        terminal,
    }
}

impl Session {
    fn send(&self, bytes: &str) {
        self.link
            .command_blocking(Command::TerminalInput {
                terminal_id: self.terminal_id,
                bytes: bytes.as_bytes().to_vec(),
            })
            .unwrap();
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.link
            .command_blocking(Command::ResizeTerminal {
                terminal_id: self.terminal_id,
                cols,
                rows,
                cell_width: 8,
                cell_height: 17,
            })
            .unwrap();
    }

    /// Take the host's events until the grid shows `needle` and has consumed
    /// the line ending printed after it. The line discipline hands the
    /// sentinel and its `\r\n` to the master side as separate writes, so the
    /// needle alone can show up while the cursor move is still in flight, and
    /// a grid compared moments later would already differ.
    fn wait_for(&mut self, needle: &str) {
        loop {
            self.watch.pump();
            let frame = &self.watch.frame;
            let sentinel_row = frame_text(frame)
                .iter()
                .position(|row| row.contains(needle));
            let settled = sentinel_row.is_some_and(|row| {
                frame
                    .cursor
                    .is_some_and(|cursor| (usize::from(cursor.row), cursor.col) == (row + 1, 0))
            });
            if settled {
                return;
            }
            self.watch.next();
        }
    }

    /// Replace the user's login shell with plain sh and silence its prompt.
    /// An explicit title also prevents foreground-process polling from changing
    /// metadata between replica comparisons. Sentinels are octal-escaped so
    /// the shell's echo of the typed line never matches.
    fn plain_shell(&mut self) {
        self.send(concat!(
            "exec /bin/sh\rPS1=; stty -echo; printf '\\033]2;replication-test\\007",
            "\\122\\105\\101\\104\\131\\n'\r",
        ));
        self.wait_for("READY");
    }

    fn shutdown(self) {
        self.link
            .command_blocking(Command::ShutdownAllAndFlush)
            .unwrap();
    }
}

/// The projection is the contract: a client that attaches after modes, a burst
/// that overflows any byte window, and a resize still reproduces the host grid
/// exactly — the thing raw-byte replay could not do.
#[cfg(unix)]
#[test]
fn late_attach_and_live_bursts_reproduce_modes_resize_and_retained_history() {
    use tcode_protocol::terminal::{CursorShape, TerminalMode};

    let mut session = terminal_session();
    session.plain_shell();
    // Alt screen + bracketed paste + steady-bar cursor, then output that
    // scrolls the screen many times over, then a sentinel.
    session.send(concat!(
        "printf '\\033[?1049h\\033[?2004h\\033[6 q'; ",
        "awk 'BEGIN{for(i=0;i<300;i++) print \"0123456789012345678901234567890123456789012345678901234567890123\"}'; ",
        "printf '\\102\\125\\114\\113\\104\\117\\116\\105\\n'\r",
    ));
    session.wait_for("BULKDONE");
    session.resize(100, 30);
    session.send("printf '\\122\\123\\132\\117\\113\\n'\r");
    session.wait_for("RSZOK");

    let late = linked(&session.mux);
    let late_events = observed_events(&late);
    let mut replica = Replica::attach(&late, late_events, session.terminal_id);
    let host = host_frame(&session.terminal);
    assert_eq!(
        difference(&replica.frame, &host),
        None,
        "a late subscriber must reproduce the host grid"
    );
    assert_eq!((host.cols, host.rows), (100, 30));
    assert!(host.modes.mode().contains(TerminalMode::ALT_SCREEN));
    assert!(host.modes.mode().contains(TerminalMode::BRACKETED_PASTE));
    assert_eq!(
        host.cursor.map(|cursor| cursor.shape),
        Some(CursorShape::Beam),
        "DECSCUSR 6 selects a bar cursor"
    );
    assert!(
        frame_text(&replica.frame)
            .iter()
            .any(|row| row.contains("RSZOK"))
    );

    // Continuation: leaving the alternate screen after the attach restores the
    // primary grid and its scrollback for the already-attached client.
    session.send(
        "printf '\\033[?1049l'; yes | head -n 2000; printf '\\120\\122\\111\\115\\101\\122\\131\\n'\r",
    );
    session.wait_for("PRIMARY");
    replica.settle(&session.terminal);
    // Scrollback follows the screen once the burst has gone quiet.
    while replica.frame.history.len() != HISTORY_LIMIT {
        replica.next();
    }
    assert!(
        !replica
            .frame
            .modes
            .mode()
            .contains(TerminalMode::ALT_SCREEN)
    );

    // A resize after the attach, narrow then wide, with a wide character at the
    // right edge of the narrow grid.
    session.send("printf '\\344\\270\\255\\346\\226\\207\\127\\111\\104\\105\\n'\r");
    session.wait_for("WIDE");
    session.resize(21, 12);
    replica.settle(&session.terminal);
    session.resize(96, 26);
    replica.settle(&session.terminal);

    session.shutdown();
}

/// Both clients write to the same PTY and reconstruct the same host state.
#[cfg(unix)]
#[test]
fn two_mux_clients_share_one_grid_and_one_pty() {
    let mut session = terminal_session();
    session.plain_shell();
    session.watch.settle(&session.terminal);

    let second_link = linked(&session.mux);
    let second_events = observed_events(&second_link);
    let mut second = Replica::attach(&second_link, second_events, session.terminal_id);
    assert_eq!(difference(&second.frame, &session.watch.frame), None);

    // Live output reaches both, and a resize is authoritative for both.
    session.send("printf '\\114\\111\\126\\105\\n'\r");
    session.wait_for("LIVE");
    session.watch.settle(&session.terminal);
    second.settle(&session.terminal);
    session.resize(71, 19);
    session.watch.settle(&session.terminal);
    second.settle(&session.terminal);
    assert_eq!((second.frame.cols, second.frame.rows), (71, 19));

    // Resubscribing reads the retained frame the other client is already
    // advancing, so neither is disturbed.
    second_link
        .unsubscribe(Subscription {
            known_index: None,
            topic: Topic::Terminal {
                terminal_id: session.terminal_id,
            },
            after: None,
        })
        .unwrap();
    let mut rejoined = Replica::attach(&second_link, second.events.clone(), session.terminal_id);
    session.watch.settle(&session.terminal);
    rejoined.settle(&session.terminal);

    second_link
        .command_blocking(Command::TerminalInput {
            terminal_id: session.terminal_id,
            bytes: b"printf '\\123\\105\\103\\117\\116\\104\\n'\r".to_vec(),
        })
        .unwrap();
    session.wait_for("SECOND");
    session.watch.settle(&session.terminal);
    rejoined.settle(&session.terminal);
    assert_eq!(difference(&session.watch.frame, &rejoined.frame), None);

    // Selection lives in the client. The host stores only what a client sends
    // it, and publishes that to everyone watching the session.
    let watcher = linked(&session.mux);
    let watcher_events = observed_events(&watcher);
    watcher
        .subscribe(Subscription {
            known_index: None,
            topic: Topic::SessionStatus {
                session_id: session.session_id.clone(),
            },
            after: None,
        })
        .unwrap();
    session
        .link
        .command_blocking(Command::CaptureTerminalSelection {
            session_id: session.session_id.clone(),
            terminal_id: session.terminal_id,
            selection: Some(tcode_protocol::TerminalSelection {
                line_start: 2,
                line_end: 3,
                text: "text selected in one client".into(),
            }),
        })
        .unwrap();
    next(&watcher_events, |event| {
        matches!(event, ServerEvent::SessionStatusReplaced(status)
            if status.terminal_contexts.iter().any(|context|
                context.text == "text selected in one client"
                    && context.line_start == 2 && context.line_end == 3))
    });

    session.shutdown();
}

/// Output arriving while a client attaches, plus a resize in the same window,
/// must leave that client with frame N followed by deltas N+1…: no gap, no
/// duplicate, and scrollback in order.
#[cfg(unix)]
#[test]
fn attachment_between_output_chunks_and_resize_preserves_contiguous_history() {
    let mut session = terminal_session();
    session.plain_shell();
    session.send(
        "awk 'BEGIN{for(i=0;i<1500;i++) print \"line-\" i}'; printf '\\120\\101\\125\\123\\105\\104\\n'; read release; awk 'BEGIN{for(i=1500;i<3000;i++) print \"line-\" i}'; printf '\\102\\125\\123\\131\\n'\r",
    );
    session.wait_for("PAUSED");
    let late = linked(&session.mux);
    let late_events = observed_events(&late);
    let mut replica = Replica::attach(&late, late_events, session.terminal_id);
    session.resize(88, 21);
    session.send("continue\r");
    session.wait_for("BUSY");
    replica.settle(&session.terminal);
    assert_eq!((replica.frame.cols, replica.frame.rows), (88, 21));

    let number = |frame: &TerminalFrame, row: &TerminalRow| {
        row_text(&resolved_row(frame, row).1)
            .strip_prefix("line-")?
            .parse::<u32>()
            .ok()
    };
    // Scrollback follows the screen once the burst has gone quiet: wait until
    // it reaches the line just above the screen's first.
    let host = host_frame(&session.terminal);
    let first_on_screen = host
        .visible
        .iter()
        .find_map(|row| number(&host, row))
        .expect("the burst's last lines are on screen");
    while replica
        .frame
        .history
        .last()
        .and_then(|row| number(&replica.frame, row))
        != Some(first_on_screen - 1)
    {
        replica.next();
    }
    assert!(replica.frame.history.len() > 100, "the burst scrolled");
    let numbers = replica
        .frame
        .history
        .iter()
        .filter_map(|row| number(&replica.frame, row))
        .collect::<Vec<_>>();
    assert!(numbers.len() > 100, "{numbers:?}");
    assert!(
        numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "scrollback arrived out of order or with a gap: {numbers:?}"
    );

    session.shutdown();
}

/// Short gaps between PTY reads must not repeatedly ship the retained ring.
/// The projection cadence is controlled here so CPU speed cannot change the budget.
#[cfg(unix)]
#[test]
fn projection_sends_changed_rows_and_defers_bursty_history_until_a_stable_gap() {
    use crate::terminal::{TerminalProjection, TerminalUpdate};

    let mut session = terminal_session();
    session.plain_shell();
    // With no subscriber the host projects nothing, so the projection driven
    // here is the only reader of the grid's damage.
    session
        .link
        .unsubscribe(Subscription {
            known_index: None,
            topic: Topic::Terminal {
                terminal_id: session.terminal_id,
            },
            after: None,
        })
        .unwrap();
    session.resize(100, 30);
    let mut projection = TerminalProjection::new();
    let mut replica = projection.reset(&session.terminal);
    let start = Instant::now();
    session.terminal.grid().feed(b"one row\r\n");
    projection.last_projected = start;
    let Some(TerminalUpdate::Delta(delta)) = projection.update(&session.terminal) else {
        panic!("changed content must produce a delta");
    };
    assert!(!delta.rows_replaced.is_empty());
    assert!(
        delta.rows_replaced.len() < 30,
        "a single line must not replace the screen"
    );
    replica.apply(&delta);
    assert!(difference(&replica, &host_frame(&session.terminal)).is_none());
    projection.last_projected = start + Duration::from_millis(16);
    assert!(
        projection.update(&session.terminal).is_none(),
        "a quiet grid has no content to send"
    );
    // A full screen that scrolls by a line moves the subscriber's rows up
    // rather than sending each of them again.
    session.terminal.grid().feed(&b"filler\r\n".repeat(40));
    let Some(TerminalUpdate::Delta(delta)) = projection.update(&session.terminal) else {
        panic!("changed content must produce a delta");
    };
    replica.apply(&delta);
    session.terminal.grid().feed(b"one more row\r\n");
    let Some(TerminalUpdate::Delta(delta)) = projection.update(&session.terminal) else {
        panic!("changed content must produce a delta");
    };
    assert_eq!(delta.scrolled, 1);
    assert!(
        delta.rows_replaced.len() <= 2,
        "a scrolled line replaced {} rows",
        delta.rows_replaced.len()
    );
    replica.apply(&delta);
    assert!(difference(&replica, &host_frame(&session.terminal)).is_none());
    let start = start + Duration::from_millis(32);
    let chunk = b"y\r\n".repeat(10_000);
    let mut bytes = 0;
    let mut last_output = start;
    for tick in 0..128 {
        let now = start + Duration::from_millis(tick * 16);
        if tick % 2 == 0 {
            session.terminal.grid().feed(&chunk);
            last_output = now;
        }
        projection.last_projected = now;
        match projection.update(&session.terminal) {
            Some(TerminalUpdate::Delta(delta)) => {
                assert!(
                    delta.history.is_none(),
                    "a one-frame gap republished scrollback"
                );
                bytes += tcode_protocol::encode_line(&ServerEvent::TerminalDelta {
                    terminal_id: session.terminal_id,
                    delta: Box::new(delta.clone()),
                })
                .unwrap()
                .len();
                replica.apply(&delta);
            }
            Some(TerminalUpdate::Frame(_)) => panic!("the burst replaced the whole frame"),
            None => {}
        }
    }
    // Even a small tail must stay deferred: appending it would leave a hole
    // where the earlier burst belongs, and must restart the quiet interval.
    session.terminal.grid().feed(b"tail\r\n");
    last_output += Duration::from_millis(32);
    projection.last_projected = last_output;
    let Some(TerminalUpdate::Delta(delta)) = projection.update(&session.terminal) else {
        panic!("the tail must update the visible screen");
    };
    assert!(delta.history.is_none());
    bytes += tcode_protocol::encode_line(&ServerEvent::TerminalDelta {
        terminal_id: session.terminal_id,
        delta: Box::new(delta.clone()),
    })
    .unwrap()
    .len();
    replica.apply(&delta);
    projection.last_projected = last_output + Duration::from_millis(99);
    assert!(projection.update(&session.terminal).is_none());
    projection.last_projected = last_output + Duration::from_millis(100);
    let Some(TerminalUpdate::Delta(delta)) = projection.update(&session.terminal) else {
        panic!("a stable gap must flush the retained history");
    };
    assert!(
        matches!(&delta.history, Some(tcode_protocol::terminal::TerminalHistoryUpdate::Replaced(rows)) if rows.len() == HISTORY_LIMIT)
    );
    bytes += tcode_protocol::encode_line(&ServerEvent::TerminalDelta {
        terminal_id: session.terminal_id,
        delta: Box::new(delta.clone()),
    })
    .unwrap()
    .len();
    replica.apply(&delta);
    assert!(!projection.owes_history());
    assert!(difference(&replica, &host_frame(&session.terminal)).is_none());
    assert!(
        bytes < chunk.len() * 64 / 4,
        "{bytes} bytes for a 1.92 MB burst"
    );
    session.shutdown();
}
