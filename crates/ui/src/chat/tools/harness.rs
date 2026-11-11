//! Windows for the harness's own calls about the session rather than the
//! workspace: the tools it loaded, the skill it launched, and its automatic
//! reviewer's verdict on an action.

use std::path::Path;

use agent::ItemStatus;
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div,
};
use gpui_base::h_flex;
use serde::Deserialize;

use super::window::{Argument, Chip, Header, Verb, first_line, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

#[derive(Deserialize)]
struct ToolSearch<'a> {
    #[serde(borrow)]
    query: &'a str,
}

#[derive(Deserialize)]
struct Skill<'a> {
    #[serde(borrow, alias = "name", alias = "id")]
    skill: &'a str,
}

/// A reviewer's verdict on an action: the action, and the risk it rated
/// when it rated one. Its rationale is the call's output.
#[derive(Deserialize)]
struct Review<'a> {
    #[serde(borrow)]
    summary: &'a str,
    #[serde(default, borrow)]
    risk: Option<&'a str>,
}

enum Kind<'a> {
    LoadTools(&'a str),
    Skill(&'a str),
    Review(Review<'a>),
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let kind = if call.named(&["toolsearch", "tool_search"]) {
        Kind::LoadTools(call.input::<ToolSearch<'a>>()?.query)
    } else if call.named(&["skill"]) {
        Kind::Skill(call.input::<Skill<'a>>()?.skill)
    } else if call.named(&["auto-reviewer"]) {
        Kind::Review(call.input::<Review<'a>>()?)
    } else {
        return None;
    };
    Some(Box::new(Harness { kind }))
}

struct Harness<'a> {
    kind: Kind<'a>,
}

/// The tools a search loaded: one `{"type":"tool_reference","tool_name":…}`
/// line each.
fn loaded_tools(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            (value.get("type")?.as_str()? == "tool_reference")
                .then(|| value.get("tool_name")?.as_str().map(str::to_owned))
                .flatten()
        })
        .collect()
}

impl ToolView for Harness<'_> {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let output = call.output.unwrap_or_default();
        match self.kind {
            Kind::LoadTools(query) => {
                let tools = loaded_tools(output);
                Header::new(IconName::Search, Verb("load_tools"))
                    .argument(Argument::mono(query))
                    .chip(
                        (!tools.is_empty())
                            .then(|| Chip::Count(super::counted("tools.tools", tools.len()))),
                    )
            }
            Kind::Skill(skill) => {
                Header::with_icon("icons/puzzle.svg", Verb("skill")).argument(Argument::mono(skill))
            }
            Kind::Review(Review { summary, risk }) => {
                let verb = match call.status {
                    ItemStatus::InProgress => Verb("review"),
                    ItemStatus::Completed => Verb("review_approved"),
                    _ => Verb("review_denied"),
                };
                let risk = risk.map(|risk| {
                    let label = crate::tr!("tools.risk", level = risk).into_owned();
                    if risk == "low" {
                        Chip::Count(label)
                    } else {
                        Chip::Warning(label)
                    }
                });
                Header::new(IconName::BadgeCheck, verb)
                    .argument(Argument::plain(one_line(summary)))
                    .chip(risk)
                    .error(first_line(output))
            }
        }
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let output = call.output.unwrap_or_default();
        match self.kind {
            Kind::LoadTools(_) => {
                let tools = loaded_tools(output);
                if tools.is_empty() {
                    return super::body(output_section(call, Bound::OUTPUT, ctx));
                }
                let cx = ctx.app();
                let theme = cx.theme();
                let names = tools.into_iter().enumerate().map(|(index, tool)| {
                    let selector = format!("tool-loaded-{}-{index}", call.id());
                    div()
                        .debug_selector(move || selector.clone())
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.foreground.opacity(0.85))
                        .child(tool)
                        .into_any_element()
                });
                let list = h_flex()
                    .w_full()
                    .gap_x_3()
                    .flex_wrap()
                    .text_secondary(false, cx)
                    .children(names);
                super::body([section(&crate::tr!("tools.loaded"), list, cx).into_any_element()])
            }
            Kind::Review(_) => {
                let rationale = output.trim();
                let text = (!rationale.is_empty()).then(|| {
                    let text =
                        bounded_text(call.entry, "output", rationale, false, Bound::HEAD, ctx);
                    section(&crate::tr!("tools.rationale"), text, ctx.app()).into_any_element()
                });
                super::body(text)
            }
            Kind::Skill(_) => super::body(output_section(call, Bound::HEAD, ctx)),
        }
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        match self.kind {
            Kind::LoadTools(_) => copy(
                &mut items,
                "chat.copy_output",
                &loaded_tools(call.output.unwrap_or_default()).join("\n"),
            ),
            Kind::Skill(_) | Kind::Review(_) => copy(
                &mut items,
                "chat.copy_output",
                call.output.unwrap_or_default(),
            ),
        }
        items
    }
}
