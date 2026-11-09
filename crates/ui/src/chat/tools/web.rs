//! The web windows: a fetch reads as the URL and what came back; a search
//! as its query and the links it found.

use std::path::Path;

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;

use super::window::{Argument, Chip, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, OpenUrl};

const FETCH: &[&str] = &["webfetch", "web_fetch", "fetch", "fetch_url"];
const SEARCH: &[&str] = &["websearch", "web_search", "search_web"];

#[derive(Deserialize)]
struct FetchInput<'a> {
    #[serde(borrow)]
    url: &'a str,
    #[serde(default, borrow)]
    prompt: Option<&'a str>,
}

#[derive(Deserialize)]
struct SearchInput<'a> {
    #[serde(borrow, alias = "searchTerm")]
    query: &'a str,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    if call.named(FETCH)
        && let Some(input) = call.input::<FetchInput<'a>>()
    {
        return Some(Box::new(Fetch { input }));
    }
    if call.named(SEARCH)
        && let Some(input) = call.input::<SearchInput<'a>>()
    {
        return Some(Box::new(Search { input }));
    }
    None
}

/// A web search item: its query, when the harness said it.
pub(in crate::chat) fn search_item_header(query: &str) -> Header {
    Header::new(IconName::Globe, Verb("search_web")).argument(Argument::plain(one_line(query)))
}

struct Fetch<'a> {
    input: FetchInput<'a>,
}

impl ToolView for Fetch<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        Header::new(IconName::Globe, Verb("fetch")).argument(Argument::plain(self.input.url))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let prompt = self
            .input
            .prompt
            .filter(|prompt| !prompt.is_empty())
            .map(|prompt| {
                let text = bounded_text(call.entry, "prompt", prompt, false, Bound::HEAD, ctx);
                section(&crate::tr!("tools.prompt"), text, ctx.app()).into_any_element()
            });
        let output = output_section(call, Bound::HEAD, ctx);
        super::body(prompt.into_iter().chain(output))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "tools.copy_url", self.input.url);
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}

struct Search<'a> {
    input: SearchInput<'a>,
}

#[derive(Deserialize)]
struct Link {
    #[serde(default)]
    title: String,
    url: String,
}

/// What a search returned: the links it lists on a `Links: [...]` line, as
/// Claude Code's WebSearch does, and the text around them.
fn links(output: &str) -> (Vec<Link>, String) {
    let mut links = Vec::new();
    let mut prose = Vec::new();
    for line in output.lines() {
        match line
            .strip_prefix("Links: ")
            .and_then(|json| serde_json::from_str::<Vec<Link>>(json).ok())
        {
            Some(found) => links.extend(found),
            None => prose.push(line),
        }
    }
    (links, prose.join("\n").trim().to_owned())
}

impl ToolView for Search<'_> {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let count = call
            .output
            .map(|output| links(output).0.len())
            .filter(|count| *count > 0)
            .map(|count| Chip::Count(crate::tr!("tools.links", count = count).into_owned()));
        search_item_header(self.input.query).chip(count)
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let (links, prose) = links(call.output.unwrap_or_default());
        if call.status == agent::ItemStatus::Failed || links.is_empty() {
            return super::body(output_section(call, Bound::HEAD, ctx));
        }
        let cx = ctx.app();
        let theme = cx.theme();
        let accent = theme.accent;
        let rows = links.iter().enumerate().map(|(index, link)| {
            let selector = format!("tool-link-{}-{index}", call.id());
            let url = link.url.clone();
            let menu_url = link.url.clone();
            let host = url::Url::parse(&link.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| link.url.clone());
            let title = if link.title.is_empty() {
                link.url.clone()
            } else {
                link.title.clone()
            };
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
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(theme.foreground.opacity(0.85))
                        .child(crate::widgets::fade_text(title)),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(220.))
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(host)),
                )
                .on_click(move |_, _, cx| cx.open_url(&url))
                .context_menu(move |menu, _, _| {
                    menu.menu(
                        crate::tr!("chat.open").into_owned(),
                        Box::new(OpenUrl(menu_url.clone())),
                    )
                    .menu(
                        crate::tr!("tools.copy_url").into_owned(),
                        Box::new(CopyText(menu_url.clone())),
                    )
                })
                .into_any_element()
        });
        let list = v_flex()
            .w_full()
            .min_w_0()
            .debug_selector({
                let id = call.id().to_owned();
                move || format!("tool-links-{id}")
            })
            .children(rows)
            .into_any_element();
        let prose = (!prose.is_empty())
            .then(|| bounded_text(call.entry, "output", &prose, false, Bound::HEAD, ctx));
        super::body(std::iter::once(list).chain(prose))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "tools.copy_query", self.input.query);
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}
