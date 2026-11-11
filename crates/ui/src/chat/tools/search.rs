//! The search window: the pattern and where it looked, then what it found,
//! each path opening its file.

use std::path::Path;

use agent::{SearchHit, SearchScope, ToolFamily};
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use super::window::{Argument, Chip, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, copy, output_section};
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::ContextMenuExt as _;

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let Some(ToolFamily::Search {
        scope,
        pattern,
        path,
        glob,
        hits,
    }) = call.family
    else {
        return None;
    };
    Some(Box::new(Search {
        scope: *scope,
        pattern: pattern.as_deref(),
        path: path.as_deref(),
        glob: glob.as_deref(),
        hits: hits.as_deref(),
    }))
}

struct Search<'a> {
    scope: SearchScope,
    pattern: Option<&'a str>,
    path: Option<&'a str>,
    glob: Option<&'a str>,
    hits: Option<&'a [SearchHit]>,
}

impl ToolView for Search<'_> {
    fn header(&self, call: &Call<'_>, cwd: &Path) -> Header {
        let (icon, verb) = match self.scope {
            SearchScope::Content => (IconName::Search, Verb("search")),
            SearchScope::Files => (IconName::FolderOpen, Verb("find")),
            SearchScope::Entries => (IconName::Folder, Verb("list")),
        };
        let place = self
            .path
            .map(|path| super::display_path(path, cwd))
            .filter(|place| !place.is_empty());
        let argument = match self.scope {
            SearchScope::Entries => Argument::mono(place.clone().unwrap_or_else(|| ".".to_owned())),
            SearchScope::Content | SearchScope::Files => {
                let detail = match (place, self.glob) {
                    (Some(place), Some(glob)) => Some(format!("{place} · {glob}")),
                    (Some(place), None) => Some(place),
                    (None, glob) => glob.map(str::to_owned),
                };
                Argument::mono(self.pattern.unwrap_or_default().to_owned())
                    .map(|argument| argument.detail(detail))
            }
        };
        let count = self
            .hits
            .filter(|_| call.status == agent::ItemStatus::Completed)
            .map(<[SearchHit]>::len)
            .filter(|count| *count > 0)
            .map(|count| Chip::Count(crate::tr!("tools.results", count = count).into_owned()));
        Header::new(icon, verb).argument(argument).chip(count)
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let hits = match self.hits {
            Some(hits) if call.status != agent::ItemStatus::Failed && !hits.is_empty() => hits,
            _ => return super::body(output_section(call, Bound::OUTPUT, ctx)),
        };
        let key = format!("tool-results-{}", call.id());
        let open = ctx.is_open(&key);
        let limit = Bound::HEAD.head;
        let folds = hits.len() > limit + 2;
        let shown = if open || !folds { hits } else { &hits[..limit] };
        let rows: Vec<AnyElement> = shown
            .iter()
            .enumerate()
            .map(|(index, hit)| result_row(call.id(), index, hit, ctx))
            .collect();
        let control = folds.then(|| {
            let label = if open {
                crate::tr!("tools.show_less")
            } else {
                crate::tr!("tools.more_results", count = hits.len() - limit)
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
        // The harness's own words around what it found: a count, that it
        // stopped short.
        let raw = call
            .output
            .filter(|output| !output.trim().is_empty())
            .map(|output| super::raw_section(call.entry, output, ctx));
        let selector = key.clone();
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .debug_selector(move || selector.clone())
            .child(
                v_flex()
                    .id(SharedString::from(format!("{key}-list")))
                    .w_full()
                    .min_w_0()
                    .when(open, |list| list.max_h(px(360.)).overflow_y_scroll())
                    .children(rows),
            )
            .children(control)
            .children(raw)
            .into_any_element()
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(
            &mut items,
            "tools.copy_pattern",
            self.pattern.unwrap_or_default(),
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
fn result_row(id: &str, index: usize, hit: &SearchHit, ctx: &ToolCtx<'_, '_>) -> AnyElement {
    let cx = ctx.app();
    let theme = cx.theme();
    let full = ctx.cwd.join(&hit.path);
    let shown = super::display_path(&full.to_string_lossy(), ctx.cwd);
    let label = match hit.line {
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
        .when_some(hit.text.as_deref(), |row, text| {
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
