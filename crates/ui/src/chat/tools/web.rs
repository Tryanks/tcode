//! The web windows: a fetch reads as the URL and what came back; a search
//! as its query and the links it found.

use std::path::Path;

use agent::{ToolFamily, WebLink};
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use gpui_base::{h_flex, v_flex};

use super::window::{Argument, Chip, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, OpenUrl};

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    match call.family? {
        ToolFamily::Fetch { url, prompt } => Some(Box::new(Fetch {
            url,
            prompt: prompt.as_deref(),
        })),
        ToolFamily::WebSearch { query, links } => Some(Box::new(Search {
            query,
            links: links.as_deref().unwrap_or_default(),
        })),
        _ => None,
    }
}

/// A web search item: its query, when the harness said it.
pub(in crate::chat) fn search_item_header(query: &str) -> Header {
    Header::new(IconName::Globe, Verb("search_web")).argument(Argument::plain(one_line(query)))
}

struct Fetch<'a> {
    url: &'a str,
    prompt: Option<&'a str>,
}

impl ToolView for Fetch<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        Header::new(IconName::Globe, Verb("fetch")).argument(Argument::plain(self.url))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let prompt = self
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
        copy(&mut items, "tools.copy_url", self.url);
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}

struct Search<'a> {
    query: &'a str,
    links: &'a [WebLink],
}

impl ToolView for Search<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        let count = (!self.links.is_empty())
            .then(|| Chip::Count(crate::tr!("tools.links", count = self.links.len()).into_owned()));
        search_item_header(self.query).chip(count)
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        if call.status == agent::ItemStatus::Failed || self.links.is_empty() {
            return super::body(output_section(call, Bound::HEAD, ctx));
        }
        let cx = ctx.app();
        let theme = cx.theme();
        let accent = theme.accent;
        let rows = self.links.iter().enumerate().map(|(index, link)| {
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
        // The harness's own words around the links.
        let raw = call
            .output
            .filter(|output| !output.trim().is_empty())
            .map(|output| super::raw_section(call.entry, output, ctx));
        super::body(std::iter::once(list).chain(raw))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "tools.copy_query", self.query);
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}
