//! The read window: the file and the lines read, numbered and highlighted
//! as the file's language, or the image read.

use std::sync::Arc;

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;

use super::window::{Argument, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, copy, output_section, thumbnails};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const READ: &[&str] = &["read", "read_file", "readfile", "view"];
const VIEW_IMAGE: &[&str] = &["view_image", "read_image", "image_view"];
const MAX_OPEN_LINES: usize = 1_000;

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(
        borrow,
        alias = "path",
        alias = "filePath",
        alias = "target_file",
        alias = "absolute_path"
    )]
    file_path: &'a str,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let image = call.named(VIEW_IMAGE);
    if !image && !call.named(READ) {
        return None;
    }
    let input = call.input::<Input<'a>>()?;
    Some(Box::new(Read { input, image }))
}

struct Read<'a> {
    input: Input<'a>,
    image: bool,
}

/// What a read returned: its lines with their numbers, from the harness
/// when it numbered them (`N\t…`, `N→…`), else counted from the offset read
/// at; and a note after them.
struct Content<'o> {
    lines: Vec<(Option<u64>, &'o str)>,
    /// Whether the harness numbered the lines itself.
    numbered: bool,
    /// A note after the lines: how much more the file holds.
    note: Option<&'o str>,
}

fn numbered(line: &str) -> Option<(u64, &str)> {
    let trimmed = line.trim_start();
    let digits = trimmed.len()
        - trimmed
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    let number = trimmed[..digits].parse().ok()?;
    let rest = &trimmed[digits..];
    let text = rest.strip_prefix('\t').or_else(|| rest.strip_prefix('→'))?;
    Some((number, text))
}

impl Read<'_> {
    fn content<'o>(&self, output: &'o str) -> Content<'o> {
        let mut all: Vec<&str> = output.lines().collect();
        // A note the harness appends after the lines, set off by a blank
        // line: `[898 more lines in file. Use offset=381 to continue.]`.
        let note = match all.as_slice() {
            [.., blank, last]
                if blank.trim().is_empty() && last.starts_with('[') && last.ends_with(']') =>
            {
                let note = *last;
                all.truncate(all.len() - 2);
                Some(note)
            }
            _ => None,
        };
        if all.first().is_some_and(|line| numbered(line).is_some()) {
            // A line without a number is the harness's, not the file's.
            return Content {
                lines: all
                    .into_iter()
                    .map(|line| match numbered(line) {
                        Some((number, text)) => (Some(number), text),
                        None => (None, line),
                    })
                    .collect(),
                numbered: true,
                note,
            };
        }
        let first = self.input.offset.filter(|offset| *offset > 0).unwrap_or(1);
        Content {
            lines: all
                .into_iter()
                .enumerate()
                .map(|(index, line)| (Some(first + index as u64), line))
                .collect(),
            numbered: false,
            note,
        }
    }

    /// The lines read: as the harness numbered what it returned, else as
    /// the call asked for them.
    fn range(&self, call: &Call<'_>) -> Option<String> {
        let range = |first: u64, last: u64| {
            crate::tr!("tools.line_range", first = first, last = last).into_owned()
        };
        if let Some(output) = call
            .output
            .filter(|_| call.status != agent::ItemStatus::Failed)
        {
            let content = self.content(output);
            let mut numbers = content.lines.iter().filter_map(|(number, _)| *number);
            if content.numbered
                && let Some(first) = numbers.next()
            {
                return Some(range(first, numbers.next_back().unwrap_or(first)));
            }
        }
        let first = self.input.offset?.max(1);
        Some(range(first, first + self.input.limit?.saturating_sub(1)))
    }
}

impl ToolView for Read<'_> {
    fn header(&self, call: &Call<'_>, cwd: &std::path::Path) -> Header {
        let verb = if self.image || !call.images.is_empty() {
            Verb("view")
        } else {
            Verb("read")
        };
        let icon = if self.image || !call.images.is_empty() {
            IconName::Image
        } else {
            IconName::FileText
        };
        Header::new(icon, verb).argument(
            Argument::mono(super::display_path(self.input.file_path, cwd))
                .map(|argument| argument.detail(self.range(call))),
        )
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let images = thumbnails(call, ctx);
        let text = call.output.unwrap_or_default();
        if call.status == agent::ItemStatus::Failed || !call.images.is_empty() {
            let output = (!text.trim().is_empty() && call.images.is_empty())
                .then(|| output_section(call, Bound::OUTPUT, ctx))
                .flatten();
            return super::body(output.into_iter().chain(images));
        }
        let (text, control) = super::full_output(call, ctx);
        let lines = code(call, self, &text, ctx);
        super::body(std::iter::once(lines).chain(control).chain(images))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "chat.copy_path", self.input.file_path);
        if let Some(output) = call.output {
            let content = self.content(output);
            let text = content
                .lines
                .iter()
                .map(|(_, line)| *line)
                .collect::<Vec<_>>()
                .join("\n");
            copy(&mut items, "tools.copy_content", &text);
        }
        items
    }

    fn path(&self) -> Option<&str> {
        Some(self.input.file_path)
    }
}

/// The lines a read window shows, numbered and highlighted.
struct ShownLines {
    highlighted: Arc<crate::highlight::HighlightedSource>,
    numbers: SharedString,
    /// More lines than the first few, so the window folds them.
    folds: bool,
    /// The lines the window leaves out.
    hidden: usize,
    /// Opened, the lines past the most it shows.
    beyond: Option<usize>,
    note: Option<SharedString>,
}

impl ShownLines {
    fn of(
        content: &Content<'_>,
        path: &str,
        open: bool,
        theme: &crate::highlight::HighlightTheme,
    ) -> Self {
        let total = content.lines.len();
        let limit = Bound::HEAD.head;
        // Hiding two lines behind a control saves nothing.
        let folds = total > limit + 2;
        let shown = if !folds {
            &content.lines[..]
        } else if open {
            // Opened, a read lays out at most this many lines every frame.
            &content.lines[..total.min(MAX_OPEN_LINES)]
        } else {
            &content.lines[..limit]
        };
        let source = shown
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        let numbers = shown
            .iter()
            .map(|(number, _)| number.map(|number| number.to_string()).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            highlighted: crate::highlight::highlighted(
                source.into(),
                crate::highlight::language_name_for_path(path).into(),
                theme,
                || None,
            ),
            numbers: numbers.into(),
            folds,
            hidden: total - shown.len(),
            beyond: open
                .then(|| total.saturating_sub(MAX_OPEN_LINES))
                .filter(|n| *n > 0),
            note: content.note.map(|note| SharedString::from(note.to_owned())),
        }
    }
}

/// Lines of a file beside their numbers, highlighted as the file's language:
/// the first lines, then all of them in a capped scroll once asked for.
fn code(call: &Call<'_>, read: &Read<'_>, output: &str, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let key = format!("tool-text-{}-content", call.id());
    let open = ctx.is_open(&key);
    let theme = ctx.highlight_theme();
    let lines = ctx.derived(
        format!("{key}-{open}-{:?}", read.input.offset),
        output,
        |output| ShownLines::of(&read.content(output), read.input.file_path, open, &theme),
    );
    let ShownLines {
        highlighted,
        numbers,
        folds,
        hidden,
        beyond,
        note,
    } = &*lines;
    let (folds, hidden, beyond) = (*folds, *hidden, *beyond);
    let cx = ctx.app();
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let block = h_flex()
        .id(SharedString::from(key.clone()))
        .w_full()
        .min_w_0()
        .items_start()
        .gap_3()
        .font_family(mono)
        .text_token(&theme.tokens.typography.xs)
        .when(open, |block| block.max_h(px(360.)).overflow_y_scroll())
        .child(
            div()
                .flex_none()
                .text_right()
                .text_color(theme.muted_foreground)
                .whitespace_nowrap()
                .child(numbers.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.foreground)
                .child(
                    StyledText::new(highlighted.source().clone())
                        .with_highlights(highlighted.runs.iter().cloned()),
                ),
        );
    let beyond = beyond.map(|count| {
        div()
            .text_secondary(false, cx)
            .child(crate::tr!("tools.lines_not_shown", count = count).into_owned())
            .into_any_element()
    });
    let note = note.clone().map(|note| {
        div()
            .text_secondary(false, cx)
            .child(note)
            .into_any_element()
    });
    let control = folds.then(|| {
        let label = if open {
            crate::tr!("tools.show_less")
        } else {
            crate::tr!("tools.more_lines", count = hidden)
        }
        .into_owned();
        let on_click = ctx.toggle(call.entry, key.clone());
        let selector = format!("{key}-more");
        h_flex()
            .child(div().debug_selector(move || selector.clone()).child(
                super::super::components::changed_files::quiet_control(
                    SharedString::from(format!("{key}-more")),
                    label.into(),
                    on_click,
                    ctx.app(),
                ),
            ))
            .into_any_element()
    });
    let selector = key.clone();
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .debug_selector(move || selector.clone())
        .child(block)
        .children(beyond)
        .children(control)
        .children(note)
        .into_any_element()
}
