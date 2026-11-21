//! The `visualize` tool of Tcode's own `tcode_visualize` server: what a call
//! asks to be drawn, and the window of a call that drew nothing.

use std::path::Path;

use agent::{ItemStatus, McpRegistration};
use gpui::AnyElement;
use serde::Deserialize;
use tcode_core::session::TimelineEntry;

use super::window::{Argument, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, code_section, copy, output_section};
use crate::icon::IconName;

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(default, borrow)]
    title: Option<&'a str>,
    #[serde(default, borrow)]
    description: Option<&'a str>,
    #[serde(default, borrow)]
    html: Option<&'a str>,
    #[serde(default, borrow)]
    path: Option<&'a str>,
}

/// What a call to the tool asks to be drawn.
pub(in crate::chat) struct Visualization<'a> {
    /// Empty when the call names none.
    pub(in crate::chat) title: &'a str,
    pub(in crate::chat) description: Option<&'a str>,
    pub(in crate::chat) page: Page<'a>,
    /// Whether the call is still running, so the page may yet change.
    pub(in crate::chat) running: bool,
}

/// Where a visualization's HTML is.
pub(in crate::chat) enum Page<'a> {
    /// In the call.
    Inline(&'a str),
    /// In a file on the thread's host, which the card reads: the call's
    /// result does not carry it.
    File(&'a str),
}

fn visualization<'a>(call: &Call<'a>) -> Option<Visualization<'a>> {
    if !call.served(&[McpRegistration::SERVER_NAME_VISUALIZE], &["visualize"]) {
        return None;
    }
    let input = call.input::<Input<'a>>()?;
    let filled = |text: Option<&'a str>| text.map(str::trim).filter(|text| !text.is_empty());
    // As the server chooses: the page in the call when it holds one, else
    // the file.
    let page = match (filled(input.html), filled(input.path)) {
        (None, Some(path)) => Page::File(path),
        _ => Page::Inline(input.html.unwrap_or_default()),
    };
    Some(Visualization {
        title: filled(input.title).unwrap_or_default(),
        description: filled(input.description),
        page,
        running: call.status == ItemStatus::InProgress,
    })
}

/// The visualization `entry` draws as a card in the thread: a call still
/// running or completed. One that failed, was declined or was interrupted
/// drew nothing, and stays a tool row that shows what the harness reported.
pub(in crate::chat) fn card(entry: &TimelineEntry) -> Option<Visualization<'_>> {
    let call = Call::of(entry)?;
    matches!(call.status, ItemStatus::InProgress | ItemStatus::Completed)
        .then(|| visualization(&call))
        .flatten()
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    Some(Box::new(VisualizeView(visualization(call)?)))
}

struct VisualizeView<'a>(Visualization<'a>);

impl VisualizeView<'_> {
    fn inline(&self) -> &str {
        match self.0.page {
            Page::Inline(html) => html,
            Page::File(_) => "",
        }
    }
}

impl ToolView for VisualizeView<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        Header::new(IconName::LayoutDashboard, Verb("visualize"))
            .argument(Argument::plain(self.0.title))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let html = self.inline();
        let html = (!html.trim().is_empty()).then(|| code_section(call, html, "html", ctx));
        let output = output_section(call, Bound::OUTPUT, ctx);
        super::body(html.into_iter().chain(output))
    }

    fn copy_items(&self, _call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "chat.copy_html", self.inline());
        items
    }

    fn path(&self) -> Option<&str> {
        match self.0.page {
            Page::File(path) => Some(path),
            Page::Inline(_) => None,
        }
    }
}
