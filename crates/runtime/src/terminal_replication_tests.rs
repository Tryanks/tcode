//! The replicated terminal grid, exercised through the production pipe and the
//! fan-out mux rather than against `AppState` directly.
//!
//! Every assertion here compares a client's reconstructed model with the host
//! emulator's own grid, which is the whole point of the projection: a
//! subscriber that attaches at any moment must see what the host sees.

use std::time::{Duration, Instant};

use super::p4b_tests::{fixture, linked, next};
use super::*;
use tcode_protocol::terminal::{
    CellWidth, HISTORY_LIMIT, TerminalCell, TerminalDelta, TerminalFrame, TerminalLink,
    TerminalRow, TerminalStyle,
};
use tcode_remote::HostMux;

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

    /// Apply everything already delivered. A frame mid-stream replaces the
    /// model; that only happens on a restart or a style-table rebuild.
    fn pump(&mut self) {
        while let Ok(envelope) = self.events.try_recv() {
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
    }

    /// Pump until the replica matches the host grid, or fail with the diff.
    fn settle(&mut self, terminal: &term::Terminal) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump();
            let host = host_frame(terminal);
            if difference(&self.frame, &host).is_none() {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "replica never matched the host grid: {}",
                    difference(&self.frame, &host).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Project the host emulator exactly as the runtime does, without consuming the
/// damage the live projection depends on.
fn host_frame(terminal: &term::Terminal) -> TerminalFrame {
    let snapshot = terminal.peek_snapshot();
    let mut projector = term::Projector::default();
    let visible = (0..snapshot.screen_lines)
        .map(|row| projector.visible_row(&snapshot, row))
        .collect();
    let history = projector.history(&snapshot);
    TerminalFrame {
        cols: snapshot.cols as u16,
        rows: snapshot.screen_lines as u16,
        modes: term::project::modes(&snapshot, terminal.modify_other_keys()),
        cursor: term::project::cursor(&snapshot),
        styles: projector.into_styles(),
        visible,
        history,
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

/// The first way two grids differ, or `None` when they are identical in every
/// replicated dimension.
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
    if replica.history.len() != host.history.len() {
        return Some(format!(
            "history rows {} != {}",
            replica.history.len(),
            host.history.len()
        ));
    }
    for (index, (left, right)) in replica.history.iter().zip(&host.history).enumerate() {
        let (left, right) = (resolved_row(replica, left), resolved_row(host, right));
        if left != right {
            let cell = left
                .1
                .iter()
                .zip(&right.1)
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            return Some(format!(
                "history row {index} cell {cell} wrapped {}/{}\n replica: {:?}\n    host: {:?}",
                left.0,
                right.0,
                left.1.get(cell),
                right.1.get(cell)
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
    /// The only consumer of this link's events; clones would compete for them.
    events: async_channel::Receiver<EventEnvelope>,
    session_id: String,
    terminal_id: u64,
    terminal: Arc<term::Terminal>,
}

/// Open a terminal panel and take a host handle for the PTY behind it.
fn terminal_session() -> Session {
    let (host, mux, link, session_id) = fixture();
    let events = link.events();
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
    Session {
        _host: host,
        mux,
        link,
        events,
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

    /// Block until the host grid shows `needle` anywhere on screen.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !frame_text(&host_frame(&self.terminal))
            .iter()
            .any(|row| row.contains(needle))
        {
            assert!(
                Instant::now() < deadline,
                "the host terminal never showed {needle}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Replace the user's login shell with plain sh and silence its prompt, so
    /// nothing prints asynchronously behind the assertions. Sentinels are
    /// octal-escaped so the shell's echo of the typed line never matches.
    fn plain_shell(&self) {
        self.send("exec /bin/sh\rPS1=; printf '\\122\\105\\101\\104\\131\\n'\r");
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
fn late_terminal_attach_reproduces_the_host_grid() {
    use tcode_protocol::terminal::{CursorShape, TerminalMode};

    let session = terminal_session();
    session.plain_shell();
    // Alt screen + bracketed paste + steady-bar cursor, then far more output
    // than the old 256 KiB replay ring could hold, then a sentinel.
    session.send(concat!(
        "printf '\\033[?1049h\\033[?2004h\\033[6 q'; ",
        "awk 'BEGIN{for(i=0;i<4400;i++) print \"0123456789012345678901234567890123456789012345678901234567890123\"}'; ",
        "printf '\\102\\125\\114\\113\\104\\117\\116\\105\\n'\r",
    ));
    session.wait_for("BULKDONE");
    session.resize(100, 30);
    session.send("printf '\\122\\123\\132\\117\\113\\n'\r");
    session.wait_for("RSZOK");

    let late = linked(&session.mux);
    let late_events = late.events();
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
        "printf '\\033[?1049l'; awk 'BEGIN{for(i=0;i<80;i++) print \"primary-\" i}'; printf '\\120\\122\\111\\115\\101\\122\\131\\n'\r",
    );
    session.wait_for("PRIMARY");
    replica.settle(&session.terminal);
    assert!(
        !replica.frame.history.is_empty(),
        "the primary screen accumulates its own scrollback"
    );
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

    // A CSI sequence and a UTF-8 character split across the moment another
    // client attaches. The client no longer parses, so this is a regression
    // guard rather than a risk.
    session.send(
        "printf '\\033[1;3'; sleep 1; printf '2m\\346\\274'; sleep 1; printf '\\242\\345\\255\\227\\033[0m\\123\\120\\114\\111\\124\\n'\r",
    );
    std::thread::sleep(Duration::from_millis(1200));
    let mid = linked(&session.mux);
    let mid_events = mid.events();
    let mut mid_replica = Replica::attach(&mid, mid_events, session.terminal_id);
    session.wait_for("SPLIT");
    replica.settle(&session.terminal);
    mid_replica.settle(&session.terminal);
    assert_eq!(difference(&mid_replica.frame, &replica.frame), None);
    assert!(
        frame_text(&replica.frame)
            .iter()
            .any(|row| row.contains("漢字SPLIT")),
        "{:?}",
        frame_text(&replica.frame)
    );

    session.shutdown();
}

/// Two ordinary mux clients see equivalent state, and PTY protocol replies are
/// produced once, on the host.
#[cfg(unix)]
#[test]
fn two_mux_clients_share_one_grid_and_one_pty() {
    let session = terminal_session();
    let mut first = Replica::attach(&session.link, session.events.clone(), session.terminal_id);
    session.plain_shell();
    first.settle(&session.terminal);

    let second_link = linked(&session.mux);
    let second_events = second_link.events();
    let mut second = Replica::attach(&second_link, second_events, session.terminal_id);
    assert_eq!(difference(&second.frame, &first.frame), None);

    // Live output reaches both, and a resize is authoritative for both.
    session.send("printf '\\114\\111\\126\\105\\n'\r");
    session.wait_for("LIVE");
    first.settle(&session.terminal);
    second.settle(&session.terminal);
    session.resize(71, 19);
    first.settle(&session.terminal);
    second.settle(&session.terminal);
    assert_eq!((second.frame.cols, second.frame.rows), (71, 19));

    // Resubscribing reads the retained frame the other client is already
    // advancing, so neither is disturbed.
    second_link
        .unsubscribe(Subscription {
            topic: Topic::Terminal {
                terminal_id: session.terminal_id,
            },
            after: None,
        })
        .unwrap();
    let mut rejoined = Replica::attach(&second_link, second.events.clone(), session.terminal_id);
    first.settle(&session.terminal);
    rejoined.settle(&session.terminal);

    // A device-attributes query is answered by the host emulator alone: two
    // attached clients must not produce two replies.
    session.send(
        "saved=$(stty -g); stty raw -echo min 0 time 20; printf '\\033[c'; sleep 1; reply=$(dd bs=1 count=64 2>/dev/null); stty \"$saved\"; printf '\\n\\104\\101%s\\n' \"${#reply}\"\r",
    );
    session.wait_for("DA");
    first.settle(&session.terminal);
    let text = frame_text(&first.frame).join("\n");
    assert!(
        text.contains("DA16"),
        "expected one 16-byte primary DA reply: {text}"
    );

    // Selection lives in the client. The host stores only what a client sends
    // it, and publishes that to everyone watching the session.
    let watcher = linked(&session.mux);
    let watcher_events = watcher.events();
    watcher
        .subscribe(Subscription {
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
fn attaching_during_output_and_a_resize_leaves_no_gap_or_duplicate() {
    let session = terminal_session();
    session.plain_shell();
    session.send(
        "awk 'BEGIN{for(i=0;i<6000;i++) print \"line-\" i}'; printf '\\102\\125\\123\\131\\n'\r",
    );
    // Attach and resize without waiting: the burst is usually still running.
    let late = linked(&session.mux);
    let late_events = late.events();
    let mut replica = Replica::attach(&late, late_events, session.terminal_id);
    session.resize(88, 21);
    session.wait_for("BUSY");
    replica.settle(&session.terminal);
    assert_eq!((replica.frame.cols, replica.frame.rows), (88, 21));

    let host = host_frame(&session.terminal);
    assert_eq!(
        replica.frame.history.len(),
        host.history.len().min(HISTORY_LIMIT)
    );
    assert!(replica.frame.history.len() > 100, "the burst scrolled");
    let numbers = replica
        .frame
        .history
        .iter()
        .filter_map(|row| {
            row_text(&resolved_row(&replica.frame, row).1)
                .strip_prefix("line-")?
                .parse::<u32>()
                .ok()
        })
        .collect::<Vec<_>>();
    assert!(numbers.len() > 100, "{numbers:?}");
    assert!(
        numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "scrollback arrived out of order or with a gap: {numbers:?}"
    );

    session.shutdown();
}

/// A delta carries only what changed, and a quiet terminal produces none.
#[cfg(unix)]
#[test]
fn deltas_carry_only_changed_rows_and_stop_when_the_grid_is_quiet() {
    let session = terminal_session();
    let mut replica = Replica::attach(&session.link, session.events.clone(), session.terminal_id);
    session.plain_shell();
    replica.settle(&session.terminal);

    while replica.events.try_recv().is_ok() {}
    session.send("printf '\\117\\116\\105\\114\\111\\116\\105\\n'\r");
    session.wait_for("ONELINE");
    std::thread::sleep(Duration::from_millis(300));

    let mut deltas: Vec<TerminalDelta> = Vec::new();
    while let Ok(envelope) = replica.events.try_recv() {
        if let ServerEvent::TerminalDelta { delta, .. } = envelope.event {
            deltas.push(*delta);
        }
    }
    assert!(!deltas.is_empty(), "typing must produce a delta");
    let rows = session.terminal.grid().dimensions().1;
    assert!(
        deltas.iter().all(|delta| delta.rows_replaced.len() < rows),
        "one line of output must not replace the whole screen"
    );

    // Nothing happens, so nothing is published.
    std::thread::sleep(Duration::from_millis(300));
    while let Ok(envelope) = replica.events.try_recv() {
        assert!(
            !matches!(envelope.event, ServerEvent::TerminalDelta { .. }),
            "an idle terminal published a delta"
        );
    }

    session.shutdown();
}

/// A megabytes-per-second flood must cost the wire a fraction of the raw
/// stream. Coalescing bounds the delta rate, row-level diffing keeps a scrolling
/// screen cheap, and scrollback that outruns its budget is republished once when
/// the burst settles rather than streamed row by row.
#[cfg(unix)]
#[test]
fn a_two_megabyte_burst_costs_far_less_than_its_bytes() {
    let session = terminal_session();
    let mut replica = Replica::attach(&session.link, session.events.clone(), session.terminal_id);
    session.plain_shell();
    session.resize(100, 30);
    replica.settle(&session.terminal);

    session.send("yes | head -c 2000000; printf '\\131\\105\\123\\104\\117\\116\\105\\n'\r");
    session.wait_for("YESDONE");
    let (mut deltas, mut bytes) = (0usize, 0usize);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        while let Ok(envelope) = replica.events.try_recv() {
            let ServerEvent::TerminalDelta { delta, .. } = &envelope.event else {
                continue;
            };
            deltas += 1;
            bytes += tcode_protocol::encode_line(&envelope.event).unwrap().len();
            replica.frame.apply(delta);
        }
        if difference(&replica.frame, &host_frame(&session.terminal)).is_none() {
            break;
        }
        assert!(Instant::now() < deadline, "the replica never caught up");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(replica.frame.history.len(), HISTORY_LIMIT);
    assert!(
        bytes < 500_000,
        "{bytes} B over {deltas} deltas for 2 MB of output"
    );

    session.shutdown();
}
