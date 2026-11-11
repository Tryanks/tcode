//! The read window: the file and the lines read, numbered and highlighted
//! as the file's language, or the image read.

use std::sync::Arc;

use agent::ToolFamily;
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use super::window::{Argument, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, copy, output_section, thumbnails};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const MAX_OPEN_LINES: usize = 1_000;

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let Some(ToolFamily::Read {
        path,
        first_line,
        note,
    }) = call.family
    else {
        return None;
    };
    Some(Box::new(Read {
        path,
        first_line: *first_line,
        note: note.as_deref(),
    }))
}

struct Read<'a> {
    path: &'a str,
    first_line: Option<u64>,
    note: Option<&'a str>,
}

impl Read<'_> {
    /// The lines read, numbered from the line the harness said the read
    /// began at.
    fn lines<'o>(&self, output: &'o str) -> Vec<(Option<u64>, &'o str)> {
        output
            .lines()
            .enumerate()
            .map(|(index, line)| (self.first_line.map(|first| first + index as u64), line))
            .collect()
    }

    /// The lines read, when the harness said where the read began.
    fn range(&self, call: &Call<'_>) -> Option<String> {
        let first = self.first_line?;
        let output = call
            .output
            .filter(|_| call.status == agent::ItemStatus::Completed)?;
        let count = output.lines().count() as u64;
        (count > 0).then(|| {
            crate::tr!("tools.line_range", first = first, last = first + count - 1).into_owned()
        })
    }
}

impl ToolView for Read<'_> {
    fn header(&self, call: &Call<'_>, cwd: &std::path::Path) -> Header {
        let (icon, verb) = if call.images.is_empty() {
            (IconName::FileText, Verb("read"))
        } else {
            (IconName::Image, Verb("view"))
        };
        Header::new(icon, verb).argument(
            Argument::mono(super::display_path(self.path, cwd))
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
        copy(&mut items, "chat.copy_path", self.path);
        if let Some(output) = call.output {
            copy(&mut items, "tools.copy_content", output);
        }
        items
    }

    fn path(&self) -> Option<&str> {
        Some(self.path)
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
}

impl ShownLines {
    fn of(
        lines: &[(Option<u64>, &str)],
        path: &str,
        open: bool,
        theme: &crate::highlight::HighlightTheme,
    ) -> Self {
        let total = lines.len();
        let limit = Bound::HEAD.head;
        // Hiding two lines behind a control saves nothing.
        let folds = total > limit + 2;
        let shown = if !folds {
            lines
        } else if open {
            // Opened, a read lays out at most this many lines every frame.
            &lines[..total.min(MAX_OPEN_LINES)]
        } else {
            &lines[..limit]
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
            ),
            numbers: numbers.into(),
            folds,
            hidden: total - shown.len(),
            beyond: open
                .then(|| total.saturating_sub(MAX_OPEN_LINES))
                .filter(|n| *n > 0),
        }
    }
}

/// Lines of a file beside their numbers, highlighted as the file's language:
/// the first lines, then all of them in a capped scroll once asked for; and
/// the harness's note after them.
fn code(call: &Call<'_>, read: &Read<'_>, output: &str, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let key = format!("tool-text-{}-content", call.id());
    let open = ctx.is_open(&key);
    let theme = ctx.highlight_theme();
    let lines = ctx.derived(
        format!("{key}-{open}-{:?}", read.first_line),
        output,
        |output| ShownLines::of(&read.lines(output), read.path, open, &theme),
    );
    let ShownLines {
        highlighted,
        numbers,
        folds,
        hidden,
        beyond,
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
        .when(read.first_line.is_some(), |block| {
            block.child(
                div()
                    .flex_none()
                    .text_right()
                    .text_color(theme.muted_foreground)
                    .whitespace_nowrap()
                    .child(numbers.clone()),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.foreground)
                .child(
                    StyledText::new(highlighted.source().clone())
                        .with_highlights(highlighted.runs().cloned()),
                ),
        );
    let beyond = beyond.map(|count| {
        div()
            .text_secondary(false, cx)
            .child(crate::tr!("tools.lines_not_shown", count = count).into_owned())
            .into_any_element()
    });
    let note = read.note.map(|note| {
        div()
            .text_secondary(false, cx)
            .whitespace_normal()
            .child(note.to_owned())
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
