//! The terminal window: a command line, its output, how it exited.

use agent::ItemStatus;
use gpui::HighlightStyle;

use super::window::{Argument, Chip, Header, Verb, first_line};
use super::{Call, ToolView, copy};
use crate::chat::model::one_line_with_break_markers;
use crate::icon::IconName;

/// A `CommandExecution`'s header: the command on one line, its line breaks
/// marked, and an exit code that is not success.
pub(in crate::chat) fn command_header(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
    status: ItemStatus,
    muted: gpui::Hsla,
) -> Header {
    let (preview, breaks) = one_line_with_break_markers(command);
    let marker = HighlightStyle {
        color: Some(muted.opacity(0.45)),
        ..Default::default()
    };
    Header::new(IconName::SquareTerminal, Verb("run"))
        .argument(Argument::mono(preview).map(|argument| Argument {
            highlights: breaks.into_iter().map(|range| (range, marker)).collect(),
            ..argument
        }))
        .chip(
            exit_code
                .filter(|code| *code != 0 && status != ItemStatus::InProgress)
                .map(Chip::Exit),
        )
        .error(failure_line(output))
}

/// The line a failed command is told by: its output's first, past the
/// `Exit code N` some harnesses put first, which the exit chip already says.
fn failure_line(output: &str) -> Option<String> {
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next()?;
    if first
        .strip_prefix("Exit code ")
        .is_some_and(|code| code.parse::<i32>().is_ok())
    {
        return lines
            .next()
            .map(str::to_owned)
            .or_else(|| first_line(first));
    }
    Some(first.to_owned())
}

pub(in crate::chat) fn command_copy_items(command: &str, output: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    copy(&mut items, "chat.copy_command", command);
    copy(&mut items, "chat.copy_output", output);
    items
}

/// A shell call made as a tool: none is claimed yet.
pub(in crate::chat) fn tool_call<'a>(_call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    None
}
