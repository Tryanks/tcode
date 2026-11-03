//! Render a command's captured output into a [`TerminalFrame`].
//!
//! Clients keep no emulator, so the width they can show is a subscription and
//! the wrapped, styled grid is what the host pushes. This runs on the host's
//! blocking-I/O executor: it is pure CPU over one string.

use tcode_protocol::{
    STORED_OUTPUT_BYTES, STORED_OUTPUT_COLS, STORED_OUTPUT_HISTORY, STORED_OUTPUT_ROWS,
    terminal::{HISTORY_LIMIT, TerminalFrame},
};
use term::{GridEmulator, Projector};

/// A command still running is pushed again as it grows, so its frame is a
/// window onto the end of its output.
const RUNNING_BYTES: usize = 512 * 1024;

/// Render `output` as it would have looked in a `cols`-wide terminal: its
/// last screen, and what scrolled off it as the frame's history; all of it
/// once the command has finished, up to the ceiling. What it leaves out, it
/// counts in `lines_evicted`.
///
/// `cols` is clamped into [`STORED_OUTPUT_COLS`], so the frame's own `cols` is
/// the authority on what the caller actually got.
pub fn render(output: &str, cols: u16, running: bool) -> TerminalFrame {
    let cols = cols.clamp(*STORED_OUTPUT_COLS.start(), *STORED_OUTPUT_COLS.end());
    let (bytes, history) = if running {
        (RUNNING_BYTES, HISTORY_LIMIT)
    } else {
        (STORED_OUTPUT_BYTES, STORED_OUTPUT_HISTORY)
    };
    let (tail, dropped) = tail(output.as_bytes(), bytes);
    let emulator =
        GridEmulator::with_scrollback(usize::from(cols), usize::from(STORED_OUTPUT_ROWS), history);
    emulator.feed(&normalized(tail));

    let snapshot = emulator.snapshot_since(0, history);
    let mut projector = Projector::default();
    let mut visible = (0..snapshot.screen_lines)
        .map(|row| projector.visible_row(&snapshot, row))
        .collect::<Vec<_>>();
    let history = projector.history(&snapshot);
    // A command block sizes to its content: trailing blank rows are not part of
    // the output, they are the unused bottom of the screen it ran on.
    // The projector drops trailing default cells, so an untouched row is empty.
    while visible.last().is_some_and(|row| row.cells.is_empty()) {
        visible.pop();
    }

    TerminalFrame {
        cols,
        rows: visible.len().min(u16::MAX as usize) as u16,
        styles: projector.into_styles(),
        visible,
        history,
        lines_evicted: dropped + snapshot.lines_evicted,
        ..TerminalFrame::default()
    }
}

/// The last `limit` bytes of `bytes`, from the start of a line, and how many
/// lines come before them.
fn tail(bytes: &[u8], limit: usize) -> (&[u8], u64) {
    let cut = bytes.len().saturating_sub(limit);
    if cut == 0 {
        return (bytes, 0);
    }
    let start = bytes[cut..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(bytes.len(), |at| cut + at + 1);
    let dropped = bytes[..start].iter().filter(|&&byte| byte == b'\n').count();
    (&bytes[start..], dropped as u64)
}

/// Captured output is a log, not a PTY stream: a bare LF means "next line at
/// column zero". Feeding it raw would leave every line indented by the last.
fn normalized(bytes: &[u8]) -> Vec<u8> {
    let mut previous_was_cr = false;
    let mut normalized = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if byte == b'\n' && !previous_was_cr {
            normalized.push(b'\r');
        }
        normalized.push(byte);
        previous_was_cr = byte == b'\r';
    }
    normalized
}

#[cfg(test)]
mod tests {
    use tcode_protocol::terminal::TerminalRow;

    fn text(row: &TerminalRow) -> String {
        let text: String = row
            .cells
            .iter()
            .map(|cell| {
                if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                }
            })
            .collect();
        text.trim_end().to_owned()
    }

    fn rows(frame: &tcode_protocol::terminal::TerminalFrame) -> Vec<String> {
        frame
            .history
            .iter()
            .chain(&frame.visible)
            .map(text)
            .collect()
    }

    /// A finished command's output longer than a screen keeps what scrolled
    /// off it, oldest first, ahead of the screen, so a window holds all of
    /// it: five thousand lines, all there.
    #[test]
    fn a_long_output_carries_what_scrolled_off_its_screen() {
        let output: String = (1..=5_000).map(|line| format!("line {line}\n")).collect();
        let frame = super::render(&output, 80, false);
        let rows = rows(&frame);
        assert_eq!(rows.first().map(String::as_str), Some("line 1"));
        assert_eq!(rows.last().map(String::as_str), Some("line 5000"));
        assert_eq!((rows.len(), frame.lines_evicted), (5_000, 0));
    }

    /// Past the ceiling, the frame holds the output's end and counts the
    /// lines before it.
    #[test]
    fn an_output_past_the_ceiling_counts_what_it_leaves_out() {
        let lines = tcode_protocol::STORED_OUTPUT_HISTORY + 500;
        let output: String = (1..=lines).map(|line| format!("line {line}\n")).collect();
        let frame = super::render(&output, 80, false);
        let rows = rows(&frame);
        assert_eq!(
            rows.last().map(String::as_str),
            Some(format!("line {lines}").as_str())
        );
        let first = rows.first().expect("rows");
        assert_eq!(
            first.as_str(),
            format!("line {}", frame.lines_evicted + 1),
            "what it leaves out is counted"
        );
        assert_eq!(rows.len() as u64 + frame.lines_evicted, lines as u64);
    }

    /// The densest output at the ceiling, every cell a character, makes a
    /// frame well inside one transport line.
    #[test]
    fn a_frame_at_the_ceiling_fits_one_transport_line() {
        let line = "x".repeat(79);
        let output: String = std::iter::repeat_n(
            format!("{line}\n"),
            tcode_protocol::STORED_OUTPUT_BYTES / 80,
        )
        .collect();
        let frame = super::render(&output, 80, false);
        let bytes = serde_json::to_vec(&frame)
            .expect("a frame serializes")
            .len();
        assert!(bytes < 12 * 1024 * 1024, "{bytes} bytes");
    }

    /// An output longer than a screen keeps what scrolled off it while the
    /// command runs, too, as far as its window goes.
    #[test]
    fn a_running_output_carries_its_last_rows() {
        let output: String = (1..=100).map(|line| format!("line {line}\n")).collect();
        let frame = super::render(&output, 80, true);
        let rows: Vec<String> = frame
            .history
            .iter()
            .chain(&frame.visible)
            .map(text)
            .collect();
        assert_eq!(rows.first().map(String::as_str), Some("line 1"));
        assert_eq!(rows.last().map(String::as_str), Some("line 100"));
        assert_eq!(rows.len(), 100);
    }
}
