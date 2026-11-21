//! Visualize tool recognition and fallback `ToolView`.
//!
//! Recognizes Tcode's built-in `tcode_visualize` (`visualize`) tool as well as
//! external visualization / widget tools (`visualize`, `show_widget`,
//! `render_html`) whose input carries an `html` (or `widget_code`) payload.

use std::borrow::Cow;
use std::path::Path;

use agent::ItemStatus;
use gpui::AnyElement;
use serde::Deserialize;
use tcode_core::session::TimelineEntry;

use super::window::{Argument, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, code_section, copy, output_section};
use crate::icon::IconName;

const SERVERS: &[&str] = &["tcode_visualize", "visualize"];
const TOOL_NAMES: &[&str] = &[
    "visualize",
    "show_widget",
    "render_html",
    "html_render",
    "render_visual",
];

#[derive(Deserialize)]
struct VisualizeInput<'a> {
    #[serde(default, borrow)]
    title: Option<&'a str>,
    #[serde(default, borrow)]
    description: Option<&'a str>,
    #[serde(default, borrow)]
    html: Option<&'a str>,
    #[serde(default, borrow)]
    widget_code: Option<&'a str>,
    #[serde(default, borrow)]
    path: Option<&'a str>,
}

#[derive(Deserialize)]
struct VisualizeOutput {
    #[serde(default)]
    html: Option<String>,
}

/// Parsed fields for a visualization tool call.
#[derive(Debug, Clone)]
pub(in crate::chat) struct VisualizePayload<'a> {
    pub(in crate::chat) title: &'a str,
    pub(in crate::chat) description: Option<&'a str>,
    pub(in crate::chat) html: Cow<'a, str>,
    pub(in crate::chat) status: ItemStatus,
}

/// Extract visualization payload if `entry` is a visualize tool call.
pub(in crate::chat) fn payload(entry: &TimelineEntry) -> Option<VisualizePayload<'_>> {
    let call = Call::of(entry)?;
    extract(&call)
}

/// Whether `entry` should render as a standalone visualization card in the chat stream.
pub(in crate::chat) fn is_visualize(entry: &TimelineEntry) -> bool {
    payload(entry).is_some()
}

fn extract<'a>(call: &Call<'a>) -> Option<VisualizePayload<'a>> {
    let name_matches = call.served(SERVERS, TOOL_NAMES)
        || TOOL_NAMES
            .iter()
            .any(|name| call.name.tool.eq_ignore_ascii_case(name));
    if !name_matches {
        return None;
    }
    let input = call.input::<VisualizeInput<'a>>()?;
    let inline_html = input
        .html
        .or(input.widget_code)
        .filter(|h| !h.trim().is_empty())
        .map(Cow::Borrowed);
    let output_html = inline_html
        .is_none()
        .then(|| {
            call.output
                .and_then(|raw| serde_json::from_str::<VisualizeOutput>(raw).ok())
                .and_then(|out| out.html)
                .filter(|h| !h.trim().is_empty())
                .map(Cow::Owned)
        })
        .flatten();

    let html = match inline_html.or(output_html) {
        Some(h) => h,
        None if call.status == ItemStatus::InProgress
            && (input.html.is_some() || input.widget_code.is_some() || input.path.is_some()) =>
        {
            Cow::Borrowed("")
        }
        None => return None,
    };
    let title = input
        .title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Visual");
    let description = input.description.map(str::trim).filter(|d| !d.is_empty());
    Some(VisualizePayload {
        title,
        description,
        html,
        status: call.status,
    })
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let spec = extract(call)?;
    Some(Box::new(VisualizeView { spec }))
}

struct VisualizeView<'a> {
    spec: VisualizePayload<'a>,
}

impl ToolView for VisualizeView<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        Header::new(IconName::LayoutDashboard, Verb("visualize"))
            .argument(Argument::plain(self.spec.title))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let html =
            (!self.spec.html.is_empty()).then(|| code_section(call, &self.spec.html, "html", ctx));
        let output = output_section(call, Bound::OUTPUT, ctx);
        super::body(html.into_iter().chain(output))
    }

    fn copy_items(&self, _call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(&mut items, "chat.copy_html", &self.spec.html);
        items
    }
}
