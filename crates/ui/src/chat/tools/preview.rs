//! The preview windows: Tcode's embedded browser (`tcode_preview`) as the
//! agent drove it — the page it opened, what it clicked, typed and pressed,
//! the screenshot it took, the page's outline as a snapshot read it, and the
//! script it evaluated beside the value it returned.

use std::path::Path;

use agent::ItemStatus;
use gpui::AnyElement;
use serde::Deserialize;
use serde_json::Value;

use super::outline;
use super::window::{Argument, Chip, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, code_section, copy, output_section, thumbnails};
use crate::chat::model::one_line;
use crate::icon::IconName;

const SERVERS: &[&str] = &["tcode_preview"];

#[derive(Deserialize)]
struct Url<'a> {
    #[serde(default, borrow)]
    url: Option<&'a str>,
}

/// The element a call acts on: a snapshot's ref, or a CSS selector.
#[derive(Deserialize)]
struct Target<'a> {
    #[serde(default, borrow, rename = "ref")]
    reference: Option<&'a str>,
    #[serde(default, borrow)]
    selector: Option<&'a str>,
    #[serde(default, borrow)]
    text: Option<&'a str>,
}

impl<'a> Target<'a> {
    fn handle(&self) -> Option<&'a str> {
        self.reference.or(self.selector)
    }
}

#[derive(Deserialize)]
struct Press<'a> {
    #[serde(borrow)]
    key: &'a str,
    #[serde(default, borrow)]
    modifiers: Vec<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scroll<'a> {
    #[serde(default)]
    delta_x: Option<f64>,
    #[serde(default)]
    delta_y: Option<f64>,
    #[serde(default, borrow)]
    selector: Option<&'a str>,
    #[serde(default, borrow, rename = "ref")]
    reference: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WaitFor<'a> {
    #[serde(default, borrow)]
    selector: Option<&'a str>,
    #[serde(default, borrow)]
    text: Option<&'a str>,
    #[serde(default, borrow)]
    url_includes: Option<&'a str>,
}

#[derive(Deserialize)]
struct Resize<'a> {
    #[serde(borrow)]
    mode: &'a str,
    #[serde(default)]
    width: Option<i64>,
    #[serde(default)]
    height: Option<i64>,
    #[serde(default, borrow)]
    preset: Option<&'a str>,
}

#[derive(Deserialize)]
struct Evaluate<'a> {
    #[serde(borrow)]
    js: &'a str,
}

enum Tool<'a> {
    Open(Option<&'a str>),
    Navigate(&'a str),
    Status,
    Snapshot,
    Screenshot,
    Click(&'a str),
    Type { selector: &'a str, text: &'a str },
    Press(Press<'a>),
    Scroll(Scroll<'a>),
    Wait(WaitFor<'a>),
    Resize(Resize<'a>),
    Evaluate(&'a str),
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let on = |name: &str| call.served(SERVERS, &[name], true);
    let tool = if on("preview_open") {
        Tool::Open(call.input::<Url<'a>>()?.url)
    } else if on("preview_navigate") {
        Tool::Navigate(call.input::<Url<'a>>()?.url?)
    } else if on("preview_status") {
        Tool::Status
    } else if on("preview_snapshot") {
        Tool::Snapshot
    } else if on("preview_screenshot") {
        Tool::Screenshot
    } else if on("preview_click") {
        Tool::Click(call.input::<Target<'a>>()?.handle()?)
    } else if on("preview_type") {
        let input = call.input::<Target<'a>>()?;
        Tool::Type {
            selector: input.handle()?,
            text: input.text?,
        }
    } else if on("preview_press") {
        Tool::Press(call.input()?)
    } else if on("preview_scroll") {
        Tool::Scroll(call.input()?)
    } else if on("preview_wait_for") {
        Tool::Wait(call.input()?)
    } else if on("preview_resize") {
        Tool::Resize(call.input()?)
    } else if on("preview_evaluate") {
        Tool::Evaluate(call.input::<Evaluate<'a>>()?.js)
    } else {
        return None;
    };
    Some(Box::new(Preview { tool }))
}

struct Preview<'a> {
    tool: Tool<'a>,
}

/// The page a status or snapshot reports, as JSON.
fn page(output: &str) -> Option<Value> {
    serde_json::from_str::<Value>(output.trim())
        .ok()
        .filter(Value::is_object)
}

fn text_of<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)?
        .as_str()
        .filter(|text| !text.trim().is_empty())
}

/// The page's title, else its URL.
fn page_name(value: &Value) -> Option<String> {
    text_of(value, "title")
        .or_else(|| text_of(value, "url"))
        .map(one_line)
}

impl ToolView for Preview<'_> {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let output = call.output.unwrap_or_default();
        let page = page(output);
        let browser = |verb| Header::new(IconName::Globe, Verb(verb));
        match &self.tool {
            Tool::Open(url) => browser("open").argument(url.and_then(Argument::plain)),
            Tool::Navigate(url) => browser("open").argument(Argument::plain(*url)),
            Tool::Status => browser("check_page")
                .argument(page.as_ref().and_then(page_name).and_then(Argument::plain)),
            Tool::Snapshot => {
                let count = page
                    .as_ref()
                    .and_then(|page| text_of(page, "outline"))
                    .map(|outline| outline::nodes(outline).len())
                    .map(|count| Chip::Count(super::counted("tools.elements", count)));
                browser("snapshot")
                    .argument(page.as_ref().and_then(page_name).and_then(Argument::plain))
                    .chip(count)
            }
            Tool::Screenshot => Header::new(IconName::Image, Verb("screenshot")),
            Tool::Click(selector) => browser("click").argument(Argument::mono(*selector)),
            Tool::Type { selector, text } => browser("type").argument(
                Argument::plain(format!("\"{}\"", one_line(text)))
                    .map(|argument| argument.detail(Some((*selector).to_owned()))),
            ),
            Tool::Press(press) => {
                let chord = press
                    .modifiers
                    .iter()
                    .copied()
                    .chain([press.key])
                    .collect::<Vec<_>>()
                    .join("+");
                browser("keypress").argument(Argument::mono(chord))
            }
            Tool::Scroll(scroll) => {
                let delta = |delta: Option<f64>, axis: &str| {
                    delta
                        .filter(|delta| *delta != 0.)
                        .map(|delta| format!("{axis}{delta:+.0}"))
                };
                let deltas = [delta(scroll.delta_x, "x"), delta(scroll.delta_y, "y")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                browser("scroll").argument(Argument::mono(deltas).map(|argument| {
                    argument.detail(scroll.reference.or(scroll.selector).map(str::to_owned))
                }))
            }
            Tool::Wait(wait) => {
                let condition = [
                    wait.selector.map(str::to_owned),
                    wait.text.map(|text| format!("\"{}\"", one_line(text))),
                    wait.url_includes.map(str::to_owned),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let timed_out = page
                    .as_ref()
                    .filter(|page| page.get("matched").and_then(Value::as_bool) == Some(false))
                    .map(|_| Chip::Warning(crate::tr!("tools.timed_out").into_owned()));
                Header::new(IconName::Hourglass, Verb("wait_for"))
                    .argument(Argument::plain(condition))
                    .chip(timed_out)
            }
            Tool::Resize(resize) => {
                let size = match (resize.preset, resize.width, resize.height) {
                    (Some(preset), _, _) => preset.to_owned(),
                    (None, Some(width), Some(height)) => format!("{width}×{height}"),
                    _ => resize.mode.to_owned(),
                };
                browser("resize").argument(Argument::mono(size))
            }
            Tool::Evaluate(js) => browser("evaluate").argument(Argument::mono(one_line(js))),
        }
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let images = thumbnails(call, ctx);
        if call.status == ItemStatus::Failed {
            let output = output_section(call, Bound::OUTPUT, ctx);
            return super::body(output.into_iter().chain(images));
        }
        match &self.tool {
            Tool::Screenshot => {
                let output = call
                    .images
                    .is_empty()
                    .then(|| output_section(call, Bound::OUTPUT, ctx))
                    .flatten();
                super::body(images.into_iter().chain(output))
            }
            Tool::Snapshot => {
                let (text, control) = super::full_output(call, ctx);
                let Some(page) = page(&text) else {
                    return super::body(output_section(call, Bound::OUTPUT, ctx));
                };
                let nodes = text_of(&page, "outline")
                    .map(outline::nodes)
                    .unwrap_or_default();
                let elements = outline::section_of(
                    call,
                    &nodes,
                    &crate::tr!("tools.page_elements"),
                    super::counted("tools.elements", nodes.len()),
                    ctx,
                );
                let raw = super::raw_section(call, text.trim(), ctx);
                super::body(elements.into_iter().chain([raw]).chain(control))
            }
            Tool::Evaluate(js) => {
                let code = code_section(
                    call,
                    js,
                    crate::highlight::language_name_for_path("script.js"),
                    ctx,
                );
                let output = output_section(call, Bound::OUTPUT, ctx);
                super::body(output.into_iter().chain(std::iter::once(code)))
            }
            _ => super::body(
                output_section(call, Bound::HEAD, ctx)
                    .into_iter()
                    .chain(images),
            ),
        }
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        match &self.tool {
            Tool::Open(Some(url)) | Tool::Navigate(url) => copy(&mut items, "tools.copy_url", url),
            Tool::Evaluate(js) => copy(&mut items, "tools.copy_code", js),
            Tool::Click(selector) | Tool::Type { selector, .. } => {
                copy(&mut items, "tools.copy_selector", selector)
            }
            _ => {}
        }
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}
