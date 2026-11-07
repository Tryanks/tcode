//! The search window: the pattern and where it looked, then what it found,
//! each path opening its file.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;

use super::window::{Argument, Chip, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, copy, output_section};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::ContextMenuExt as _;

const GREP: &[&str] = &[
    "grep",
    "search",
    "grep_files",
    "search_files",
    "ripgrep",
    "rg",
];
const FIND: &[&str] = &["glob", "find", "glob_files", "find_files"];
const LIST: &[&str] = &["ls", "list", "list_dir", "list_directory"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Grep,
    Find,
    List,
}

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(
        default,
        borrow,
        alias = "query",
        alias = "regex",
        alias = "globPattern"
    )]
    pattern: Option<&'a str>,
    #[serde(
        default,
        borrow,
        alias = "targetDirectory",
        alias = "target_directory",
        alias = "dir_path",
        alias = "directory"
    )]
    path: Option<&'a str>,
    #[serde(default, borrow, alias = "include")]
    glob: Option<&'a str>,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let kind = if call.named(GREP) {
        Kind::Grep
    } else if call.named(FIND) {
        Kind::Find
    } else if call.named(LIST) {
        Kind::List
    } else {
        return None;
    };
    let input = call.input::<Input<'a>>()?;
    // A search names what it looks for; a listing, where.
    if kind != Kind::List && input.pattern.is_none() {
        return None;
    }
    Some(Box::new(Search { kind, input }))
}

struct Search<'a> {
    kind: Kind,
    input: Input<'a>,
}

/// One line of what a search returned.
enum Found<'o> {
    /// `path:line:text`, a match in a file.
    Match {
        path: &'o str,
        line: &'o str,
        text: &'o str,
    },
    /// A file or directory.
    Path(&'o str),
    /// The harness's own words: `Found 3 files`, `No files found`.
    Note(&'o str),
}

fn found(line: &str) -> Found<'_> {
    let trimmed = line.trim_end();
    if is_note(trimmed) {
        return Found::Note(trimmed);
    }
    if let Some((path, rest)) = trimmed.split_once(':')
        && let Some((number, text)) = rest.split_once(':')
        && !path.is_empty()
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Found::Match {
            path,
            line: number,
            text: text.strip_prefix(' ').unwrap_or(text),
        };
    }
    Found::Path(trimmed.trim_start())
}

fn is_note(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    (lower.starts_with("found ") || lower.starts_with("no "))
        && (lower.contains("file") || lower.contains("match") || lower.contains("result"))
}

impl Search<'_> {
    fn results<'o>(&self, output: &'o str) -> (Vec<Found<'o>>, Vec<&'o str>) {
        let mut results = Vec::new();
        let mut notes = Vec::new();
        // Matches grouped under their file: `path:` then `  Line N: text`.
        let mut group: Option<&str> = None;
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            if let Some(path) = group
                && let Some(rest) = line.trim_start().strip_prefix("Line ")
                && let Some((number, text)) = rest.split_once(':')
                && number.bytes().all(|byte| byte.is_ascii_digit())
            {
                results.push(Found::Match {
                    path,
                    line: number,
                    text: text.strip_prefix(' ').unwrap_or(text),
                });
                continue;
            }
            if self.kind == Kind::Grep
                && let Some(path) = line.trim_end().strip_suffix(':')
                && !path.starts_with(char::is_whitespace)
            {
                group = Some(path);
                continue;
            }
            match found(line) {
                Found::Note(note) => notes.push(note),
                result => results.push(result),
            }
        }
        (results, notes)
    }

    /// Where a relative path the call returned lives: a listing names what
    /// is in its directory; a search, paths under the thread's.
    fn base(&self, cwd: &Path) -> PathBuf {
        match (self.kind, self.input.path) {
            (Kind::List, Some(path)) => cwd.join(path),
            _ => cwd.to_path_buf(),
        }
    }
}

impl ToolView for Search<'_> {
    fn header(&self, call: &Call<'_>, cwd: &Path) -> Header {
        let (icon, verb) = match self.kind {
            Kind::Grep => (IconName::Search, Verb("search")),
            Kind::Find => (IconName::FolderOpen, Verb("find")),
            Kind::List => (IconName::Folder, Verb("list")),
        };
        let place = self
            .input
            .path
            .map(|path| super::display_path(path, cwd))
            .filter(|place| !place.is_empty());
        let argument = match self.kind {
            Kind::List => Argument::mono(place.clone().unwrap_or_else(|| ".".to_owned())),
            Kind::Grep | Kind::Find => {
                let detail = match (place, self.input.glob) {
                    (Some(place), Some(glob)) => Some(format!("{place} · {glob}")),
                    (Some(place), None) => Some(place),
                    (None, glob) => glob.map(str::to_owned),
                };
                Argument::mono(self.input.pattern.unwrap_or_default().to_owned())
                    .map(|argument| argument.detail(detail))
            }
        };
        let count = (call.status == agent::ItemStatus::Completed)
            .then_some(call.output)
            .flatten()
            .map(|output| self.results(output).0.len())
            .filter(|count| *count > 0)
            .map(|count| Chip::Count(crate::tr!("tools.results", count = count).into_owned()));
        Header::new(icon, verb).argument(argument).chip(count)
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let (output, elided) = super::full_output(call, ctx);
        let (results, notes) = self.results(&output);
        if call.status == agent::ItemStatus::Failed || results.is_empty() {
            let section = output_section(call, Bound::OUTPUT, ctx);
            return super::body(section);
        }
        let key = format!("tool-results-{}", call.id());
        let open = ctx.is_open(&key);
        let limit = Bound::HEAD.head;
        let folds = results.len() > limit + 2;
        let shown = if open || !folds {
            &results[..]
        } else {
            &results[..limit]
        };
        let base = self.base(ctx.cwd);
        let rows: Vec<AnyElement> = shown
            .iter()
            .enumerate()
            .map(|(index, result)| result_row(call.id(), index, result, &base, ctx))
            .collect();
        let control = folds.then(|| {
            let label = if open {
                crate::tr!("tools.show_less")
            } else {
                crate::tr!("tools.more_results", count = results.len() - limit)
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
        let cx = ctx.app();
        let notes = notes.into_iter().map(|note| {
            div()
                .text_secondary(false, cx)
                .child(note.to_owned())
                .into_any_element()
        });
        let selector = key.clone();
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .debug_selector(move || selector.clone())
            .children(notes)
            .child(
                v_flex()
                    .id(SharedString::from(format!("{key}-list")))
                    .w_full()
                    .min_w_0()
                    .when(open, |list| list.max_h(px(360.)).overflow_y_scroll())
                    .children(rows),
            )
            .children(control)
            .children(elided)
            .into_any_element()
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(
            &mut items,
            "tools.copy_pattern",
            self.input.pattern.unwrap_or_default(),
        );
        copy(
            &mut items,
            "tools.copy_results",
            call.output.unwrap_or_default(),
        );
        items
    }
}

/// One thing a search found: its path, which opens the file, and for a
/// match its line and text.
fn result_row(
    id: &str,
    index: usize,
    result: &Found<'_>,
    base: &Path,
    ctx: &ToolCtx<'_, '_>,
) -> AnyElement {
    let cx = ctx.app();
    let theme = cx.theme();
    let (path, line, text) = match result {
        Found::Match { path, line, text } => (*path, Some(*line), Some(*text)),
        Found::Path(path) => (*path, None, None),
        Found::Note(note) => (*note, None, None),
    };
    let full = base.join(path);
    let shown = super::display_path(&full.to_string_lossy(), ctx.cwd);
    let label = match line {
        Some(line) => format!("{shown}:{line}"),
        None => shown,
    };
    let selector = format!("tool-result-{id}-{index}");
    let open = full.clone();
    let menu_path = full.to_string_lossy().into_owned();
    let menu_base = ctx.cwd.to_path_buf();
    let accent = theme.accent;
    h_flex()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .w_full()
        .min_w_0()
        .gap_2()
        .px_1()
        .rounded(crate::material::radius_chip(cx))
        .cursor_pointer()
        .hover(move |row| row.bg(accent))
        .text_secondary(false, cx)
        .child(
            div()
                .flex_none()
                .max_w(px(420.))
                .overflow_hidden()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.foreground.opacity(0.85))
                .child(crate::widgets::fade_text(label)),
        )
        .when_some(text, |row, text| {
            row.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .font_family(theme.mono_font_family.clone())
                    .child(crate::widgets::fade_text(text.trim().to_owned())),
            )
        })
        .on_click(move |_, _, cx| cx.open_with_system(&open))
        .context_menu(move |menu, _, _| menu.path_items(&menu_path, Some(&menu_base)))
        .into_any_element()
}
