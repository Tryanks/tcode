//! The desktop windows: Tcode's own computer-use server
//! (`tcode_computer_use`), read in the formats it writes. An observation is
//! the window it saw, its screenshot and its outline; an action transaction
//! is its steps, each with the outcome the server reported.

use std::path::Path;

use agent::ItemStatus;
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use serde_json::Value;

use super::outline::{self, Node};
use super::window::{Argument, Chip, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section, thumbnails};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const SERVER: &[&str] = &["tcode_computer_use"];

#[derive(Deserialize, Default)]
struct RootFilters<'a> {
    #[serde(default, borrow)]
    text: Option<&'a str>,
    #[serde(default, borrow)]
    app: Option<&'a str>,
    #[serde(default, borrow)]
    bundle_id: Option<&'a str>,
    #[serde(default)]
    pid: Option<u64>,
}

#[derive(Deserialize)]
struct Observe<'a> {
    #[serde(default, borrow)]
    root: Option<&'a str>,
}

#[derive(Deserialize)]
struct Search<'a> {
    #[serde(default, borrow)]
    text: Option<&'a str>,
    #[serde(default, borrow)]
    role: Option<&'a str>,
}

#[derive(Deserialize)]
struct Element<'a> {
    #[serde(borrow, rename = "ref")]
    reference: &'a str,
}

#[derive(Deserialize)]
struct ReadText<'a> {
    /// Missing from a call the server refused for it.
    #[serde(default, borrow, rename = "ref")]
    reference: Option<&'a str>,
}

/// One action of a transaction, as `act_ui` takes it.
#[derive(Deserialize)]
struct Action {
    action: String,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    scroll_x: Option<f64>,
    #[serde(default)]
    scroll_y: Option<f64>,
    #[serde(default)]
    path: Option<Vec<[f64; 2]>>,
}

/// A condition `act_ui` verifies or `wait_for` waits on.
#[derive(Deserialize, Default)]
struct Condition {
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    until: Option<String>,
}

#[derive(Deserialize)]
struct Act {
    actions: Vec<Action>,
    #[serde(default)]
    expect: Option<Condition>,
}

enum Tool<'a> {
    FindRoots(RootFilters<'a>),
    Observe(Observe<'a>),
    Search(Search<'a>),
    Expand(Element<'a>),
    Inspect(Element<'a>),
    Act(Act),
    Wait(Condition),
    ReadText(ReadText<'a>),
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let on = |name: &str| call.served(SERVER, &[name], true);
    let tool = if on("find_roots") {
        Tool::FindRoots(call.input().unwrap_or_default())
    } else if on("observe_ui") {
        Tool::Observe(call.input()?)
    } else if on("search_ui") {
        Tool::Search(call.input()?)
    } else if on("expand_ui") {
        Tool::Expand(call.input()?)
    } else if on("inspect_ui") {
        Tool::Inspect(call.input()?)
    } else if on("act_ui") {
        Tool::Act(call.input()?)
    } else if on("wait_for") && call.input.get("state_id").is_some() {
        Tool::Wait(call.input()?)
    } else if on("read_text") {
        Tool::ReadText(call.input()?)
    } else {
        return None;
    };
    Some(Box::new(Desktop { tool }))
}

/// Whether `call` observes a window: the one call whose result carries the
/// screenshot the server read back.
pub(in crate::chat) fn is_observation(call: &Call<'_>) -> bool {
    call.served(SERVER, &["observe_ui"], true)
}

/// The app and the window title of the root an observation's `output`
/// reports; a title that is empty or only repeats the app is none.
pub(in crate::chat) fn observed_root(output: &str) -> (Option<String>, Option<String>) {
    let root = fact(output, "root");
    let app = root.and_then(|root| attribute(&format!(" {root}"), "app"));
    let title = root
        .and_then(|root| attribute(&format!(" {root}"), "title"))
        .filter(|title| !title.is_empty() && Some(title) != app.as_ref());
    (app, title)
}

struct Desktop<'a> {
    tool: Tool<'a>,
}

/// A refusal, as the server words it: a backend error is JSON whose
/// `message` says what went wrong; every other is its own text.
fn refusal(output: &str) -> Option<String> {
    serde_json::from_str::<Value>(output.trim())
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
}

/// The `key: value` facts an output opens with, before its outline.
fn fact<'o>(output: &'o str, key: &str) -> Option<&'o str> {
    output
        .lines()
        .take_while(|line| outline::parse_line(line).is_none())
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
}

/// The leading count of a fact: `elements: 6 interactive: 4` is 6.
fn count(output: &str, key: &str) -> Option<usize> {
    fact(output, key)?.split_whitespace().next()?.parse().ok()
}

/// The `key="value"` pair in a line the server writes, unescaped.
fn attribute(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!(" {key}=\""))? + key.len() + 3;
    let mut value = String::new();
    let mut chars = line[start..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(value),
            '\\' => value.push(chars.next()?),
            other => value.push(other),
        }
    }
    None
}

/// The report an action transaction or a wait leads with, and what follows
/// it: the successor's diff or outline, and the harness's notes.
fn report(output: &str) -> Option<(Value, &str)> {
    let mut stream = serde_json::Deserializer::from_str(output).into_iter::<Value>();
    let value = stream.next()?.ok().filter(Value::is_object)?;
    let rest = &output[stream.byte_offset()..];
    Some((value, rest.trim_start_matches('\n')))
}

/// How an element the output names reads: its role and name, from any line
/// of the output that lists it (the successor's outline, the harness's
/// candidate targets).
fn element_name(output: &str, reference: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim_start().strip_prefix("- ").unwrap_or(line);
        let node = outline::parse_line(line)?;
        (node.reference.as_deref() == Some(reference)).then(|| match node.name {
            Some(name) => format!("{} \"{name}\"", node.role),
            None => node.role,
        })
    })
}

fn point(x: f64, y: f64) -> String {
    format!("({x:.0}, {y:.0})")
}

impl Action {
    fn verb(&self) -> Verb {
        Verb(match self.action.as_str() {
            "press" => "press",
            "click" => "click",
            "set_text" => "set_text",
            "type_text" => "type",
            "keypress" => "keypress",
            "scroll" => "scroll",
            "drag" => "drag",
            "move_mouse" => "move_mouse",
            _ => "act",
        })
    }

    /// What the action acted on, as its fields say: the element, the text
    /// typed, the keys, the point.
    fn target(&self, output: &str) -> String {
        let element = self.reference.as_deref().map(|reference| {
            element_name(output, reference).unwrap_or_else(|| reference.to_owned())
        });
        let at = match (self.x, self.y) {
            (Some(x), Some(y)) => Some(point(x, y)),
            _ => None,
        };
        let what = match self.action.as_str() {
            "set_text" | "type_text" => self
                .text
                .as_deref()
                .map(|text| format!("\"{}\"", one_line(text))),
            "keypress" => self.keys.as_ref().map(|keys| keys.join("+")),
            "scroll" => {
                let delta = |delta: Option<f64>, axis: &str| {
                    delta
                        .filter(|delta| *delta != 0.)
                        .map(|delta| format!("{axis}{delta:+.0}"))
                };
                let deltas: Vec<String> = [delta(self.scroll_x, "x"), delta(self.scroll_y, "y")]
                    .into_iter()
                    .flatten()
                    .collect();
                (!deltas.is_empty()).then(|| deltas.join(" "))
            }
            "drag" => self.path.as_ref().and_then(|path| {
                let (first, last) = (path.first()?, path.last()?);
                Some(format!(
                    "{} → {}",
                    point(first[0], first[1]),
                    point(last[0], last[1])
                ))
            }),
            _ => None,
        };
        [what, element, at]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

impl Condition {
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(text) = &self.text {
            parts.push(format!("\"{}\"", one_line(text)));
        }
        if let Some(role) = &self.role {
            parts.push(role.clone());
        }
        if let Some(value) = &self.value {
            parts.push(format!("= \"{}\"", one_line(value)));
        }
        if let Some(reference) = &self.reference {
            parts.push(reference.clone());
        }
        if self.until.as_deref() == Some("absent") {
            parts.push(crate::tr!("tools.until_absent").into_owned());
        }
        parts.join(" ")
    }
}

impl Desktop<'_> {
    fn header_of(&self, call: &Call<'_>) -> Header {
        let output = call.output.unwrap_or_default();
        let done = call.status == ItemStatus::Completed;
        match &self.tool {
            Tool::FindRoots(filters) => {
                let filter = [filters.app, filters.bundle_id, filters.text]
                    .into_iter()
                    .flatten()
                    .map(str::to_owned)
                    .chain(filters.pid.map(|pid| format!("pid {pid}")))
                    .collect::<Vec<_>>()
                    .join(" · ");
                Header::new(IconName::Monitor, Verb("find_windows"))
                    .argument(Argument::plain(filter))
                    .chip(
                        count(output, "roots")
                            .filter(|_| done)
                            .map(|n| Chip::Count(super::counted("tools.windows", n))),
                    )
            }
            Tool::Observe(input) => {
                let (app, title) = observed_root(output);
                let argument = match app {
                    Some(app) => Argument::plain(app).map(|argument| argument.detail(title)),
                    None => input.root.and_then(Argument::mono),
                };
                Header::new(IconName::Monitor, Verb("observe"))
                    .argument(argument)
                    .chip(
                        count(output, "elements")
                            .filter(|_| done)
                            .map(|n| Chip::Count(super::counted("tools.elements", n))),
                    )
            }
            Tool::Search(input) => {
                let query = [input.text.map(one_line), input.role.map(str::to_owned)]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                Header::new(IconName::Search, Verb("search"))
                    .argument(Argument::plain(query))
                    .chip(
                        count(output, "matches")
                            .filter(|_| done)
                            .map(|n| Chip::Count(super::counted("tools.matches", n))),
                    )
            }
            Tool::Expand(element) => Header::new(IconName::Monitor, Verb("expand")).argument(
                Argument::plain(
                    element_name(output, element.reference)
                        .unwrap_or_else(|| element.reference.to_owned()),
                )
                .map(|argument| argument.detail(Some(element.reference.to_owned()))),
            ),
            Tool::Inspect(element) => {
                let inspected = serde_json::from_str::<Value>(output).ok();
                let named = inspected.as_ref().and_then(|value| {
                    let role = value.get("role")?.as_str()?;
                    let title = value.get("title").and_then(Value::as_str).unwrap_or("");
                    Some(if title.is_empty() {
                        role.to_owned()
                    } else {
                        format!("{role} \"{title}\"")
                    })
                });
                Header::new(IconName::Monitor, Verb("inspect")).argument(
                    Argument::plain(named.unwrap_or_else(|| element.reference.to_owned()))
                        .map(|argument| argument.detail(Some(element.reference.to_owned()))),
                )
            }
            Tool::Act(act) => {
                let report = report(output).map(|(value, _)| value);
                let (verb, argument) = match act.actions.as_slice() {
                    [action] => (action.verb(), action.target(output)),
                    actions => (
                        Verb("act"),
                        actions
                            .iter()
                            .map(|action| {
                                let target = action.target(output);
                                let verb = action.verb().text(ItemStatus::Declined);
                                if target.is_empty() {
                                    verb.to_string()
                                } else {
                                    format!("{verb} {target}")
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" → "),
                    ),
                };
                let steps = (act.actions.len() > 1).then(|| {
                    Chip::Count(crate::tr!("tools.steps", count = act.actions.len()).into_owned())
                });
                let didnt = report
                    .as_ref()
                    .filter(|report| report.get("outcome").and_then(Value::as_str) == Some("didnt"))
                    .map(|_| Chip::Warning(crate::tr!("tools.outcome.didnt").into_owned()));
                let in_front = report
                    .as_ref()
                    .filter(|report| {
                        report.get("activation").and_then(Value::as_str) == Some("foreground")
                    })
                    .map(|_| Chip::Tag(crate::tr!("tools.foreground.in_front").into_owned()));
                Header::new(IconName::Monitor, verb)
                    .argument(Argument::plain(argument))
                    .chip(steps)
                    .chip(in_front)
                    .chip(didnt)
            }
            Tool::Wait(condition) => {
                let timed_out = report(output)
                    .filter(|(report, _)| {
                        report.get("status").and_then(Value::as_str) == Some("timeout")
                    })
                    .map(|_| Chip::Warning(crate::tr!("tools.timed_out").into_owned()));
                Header::new(IconName::Hourglass, Verb("wait_for"))
                    .argument(Argument::plain(condition.describe()))
                    .chip(timed_out)
            }
            Tool::ReadText(input) => {
                let page = [fact(output, "offset"), fact(output, "total_bytes")];
                let detail = match page {
                    [Some(offset), Some(total)] => Some(format!("{offset}/{total}")),
                    _ => None,
                };
                Header::new(IconName::FileText, Verb("read")).argument(
                    input
                        .reference
                        .and_then(Argument::mono)
                        .map(|argument| argument.detail(detail)),
                )
            }
        }
    }

    /// The steps of a transaction: each action and what the server said of
    /// it; those after the one it stopped at never ran.
    fn steps(&self, call: &Call<'_>, act: &Act, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let output = call.output.unwrap_or_default();
        let report = report(output).map(|(value, _)| value);
        let results: Vec<&Value> = report
            .as_ref()
            .and_then(|report| report.get("steps")?.as_array())
            .map(|steps| steps.iter().collect())
            .unwrap_or_default();
        let stopped_at = report
            .as_ref()
            .and_then(|report| report.get("stopped_at")?.as_u64());
        let cx = ctx.app();
        let theme = cx.theme();
        let rows = act.actions.iter().enumerate().map(|(index, action)| {
            let number = index as u64 + 1;
            let result = results
                .iter()
                .find(|step| step.get("index").and_then(Value::as_u64) == Some(number));
            let outcome = result.and_then(|step| step.get("outcome")?.as_str());
            let raw_message = result.and_then(|step| step.get("message")?.as_str());
            let message = result
                .and_then(|step| foreground_line(step))
                .or_else(|| raw_message.map(str::to_owned));
            let skipped = result.is_none() && stopped_at.is_some_and(|stop| number > stop);
            let (label, color) = match (outcome, skipped) {
                (Some("worked"), _) => (Some(crate::tr!("tools.outcome.worked")), theme.success),
                (Some("didnt"), _) => (Some(crate::tr!("tools.outcome.didnt")), theme.danger),
                (Some(_), _) => (
                    Some(crate::tr!("tools.outcome.unknown")),
                    theme.muted_foreground,
                ),
                (None, true) => (
                    Some(crate::tr!("tools.outcome.not_run")),
                    theme.muted_foreground,
                ),
                (None, false) => (None, theme.muted_foreground),
            };
            let status = if result.is_some() {
                ItemStatus::Completed
            } else if skipped {
                ItemStatus::Declined
            } else {
                call.status
            };
            let selector = format!("tool-step-{}-{index}", call.id());
            let target = action.target(output);
            v_flex()
                .w_full()
                .min_w_0()
                .debug_selector(move || selector.clone())
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .w(px(16.))
                                .text_right()
                                .text_meta_mono(false, cx)
                                .child(number.to_string()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_medium()
                                .text_color(if skipped {
                                    theme.muted_foreground
                                } else {
                                    theme.foreground
                                })
                                .child(action.verb().text(status)),
                        )
                        .when(!target.is_empty(), |row| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_color(theme.foreground.opacity(0.85))
                                    .child(crate::widgets::fade_text(target)),
                            )
                        })
                        .when_some(label, |row, label| {
                            row.child(
                                div()
                                    .flex_none()
                                    .text_color(color)
                                    .child(label.into_owned()),
                            )
                        }),
                )
                .when_some(
                    message.filter(|message| !message.is_empty()),
                    |row, message| {
                        row.child(
                            div()
                                .pl(px(24.))
                                .min_w_0()
                                .text_color(theme.muted_foreground)
                                .child(crate::widgets::fade_text(message)),
                        )
                    },
                )
                .into_any_element()
        });
        let expect = act.expect.as_ref().map(|condition| {
            let status = report
                .as_ref()
                .and_then(|report| report.get("expect")?.as_str())
                .unwrap_or_default();
            let (label, color) = match status {
                "verified" => (crate::tr!("tools.expect.verified"), theme.success),
                "preexisting" => (crate::tr!("tools.expect.preexisting"), theme.success),
                "failed" => (crate::tr!("tools.expect.failed"), theme.danger),
                _ => (crate::tr!("tools.expect.pending"), theme.muted_foreground),
            };
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .pl(px(24.))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(crate::tr!("tools.expect.label").into_owned()),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(condition.describe())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(color)
                        .child(label.into_owned()),
                )
                .into_any_element()
        });
        section(
            &crate::tr!("tools.steps_caption"),
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .text_secondary(false, cx)
                .debug_selector({
                    let id = call.id().to_owned();
                    move || format!("tool-steps-{id}")
                })
                .children(rows)
                .children(expect),
            cx,
        )
        .into_any_element()
    }
}

impl ToolView for Desktop<'_> {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let header = self.header_of(call);
        match call.status {
            ItemStatus::Failed => header.error(call.output.and_then(refusal)),
            _ => header,
        }
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let images = thumbnails(call, ctx);
        if call.status == ItemStatus::Failed {
            let steps = match &self.tool {
                Tool::Act(act) => Some(self.steps(call, act, ctx)),
                _ => None,
            };
            let output = output_section(call, Bound::OUTPUT, ctx);
            return super::body(steps.into_iter().chain(output).chain(images));
        }
        let (text, control) = super::full_output(call, ctx);
        let tree = |nodes: &[Node], count: String, ctx: &mut ToolCtx<'_, '_>| {
            outline::section_of(call, nodes, &crate::tr!("tools.ui_tree"), count, ctx)
        };
        // The output a structured view was read from stays one click away;
        // with nothing read from it, it shows open.
        let raw = |structured: bool, ctx: &mut ToolCtx<'_, '_>| {
            let raw = text.trim();
            if raw.is_empty() {
                None
            } else if structured {
                Some(super::raw_section(call, raw, ctx))
            } else {
                let body = bounded_text(call.entry, "output", raw, true, Bound::HEAD, ctx);
                Some(section(&crate::tr!("tools.output"), body, ctx.app()).into_any_element())
            }
        };
        let sections: Vec<AnyElement> = match &self.tool {
            Tool::FindRoots(_) => {
                let nodes: Vec<Node> = text.lines().filter_map(root_node).collect();
                let n = count(&text, "roots").unwrap_or(nodes.len());
                let list = outline::section_of(
                    call,
                    &nodes,
                    &crate::tr!("tools.windows_caption"),
                    super::counted("tools.windows", n),
                    ctx,
                );
                let structured = list.is_some();
                list.into_iter().chain(raw(structured, ctx)).collect()
            }
            Tool::Observe(_) | Tool::Expand(_) => {
                let nodes = outline::nodes(&text);
                let n = count(&text, "elements").unwrap_or(nodes.len());
                let tree = tree(&nodes, super::counted("tools.elements", n), ctx);
                let structured = tree.is_some();
                images
                    .into_iter()
                    .chain(tree)
                    .chain(raw(structured, ctx))
                    .collect()
            }
            Tool::Search(_) => {
                let nodes = outline::nodes(&text);
                let n = count(&text, "matches").unwrap_or(nodes.len());
                let tree = tree(&nodes, super::counted("tools.matches", n), ctx);
                let structured = tree.is_some();
                tree.into_iter().chain(raw(structured, ctx)).collect()
            }
            Tool::Act(act) => {
                let steps = self.steps(call, act, ctx);
                let after = report(&text).map_or(&*text, |(_, rest)| rest);
                let nodes = outline::nodes(after);
                let label = super::counted("tools.elements", nodes.len());
                let tree = tree(&nodes, label, ctx);
                std::iter::once(steps)
                    .chain(images)
                    .chain(tree)
                    .chain(raw(true, ctx))
                    .collect()
            }
            Tool::Wait(_) => {
                let after = report(&text).map_or(&*text, |(_, rest)| rest);
                let nodes = outline::nodes(after);
                let label = super::counted("tools.elements", nodes.len());
                let tree = tree(&nodes, label, ctx);
                let structured = tree.is_some();
                tree.into_iter().chain(raw(structured, ctx)).collect()
            }
            Tool::ReadText(_) => {
                let page = text.split_once("\n---\n").map_or(&*text, |(_, page)| page);
                let body = bounded_text(call.entry, "output", page, true, Bound::HEAD, ctx);
                vec![section(&crate::tr!("tools.output"), body, ctx.app()).into_any_element()]
            }
            Tool::Inspect(_) => output_section(call, Bound::HEAD, ctx).into_iter().collect(),
        };
        super::body(sections.into_iter().chain(control))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}

/// A window `find_roots` lists: `@r1 window app="X" bundle_id="…" pid=1
/// title="…" window_id=2 frame=(…)`.
/// What was decided about bringing a step's app to the front, in the
/// user's language; the server's own words stay in the copied output.
fn foreground_line(step: &Value) -> Option<String> {
    let app = step.get("app").and_then(Value::as_str).unwrap_or_default();
    let decision = step.get("foreground")?.as_str()?;
    // Allowed but the keys did not go through: the server's words say why.
    if decision.starts_with("allowed")
        && step.get("outcome").and_then(Value::as_str) == Some("didnt")
    {
        return None;
    }
    let key = match decision {
        "allowed_once" => "tools.foreground.allowed_once",
        "allowed_always" => "tools.foreground.allowed_always",
        "allowed_by_setting" => "tools.foreground.allowed_by_setting",
        "declined" => "tools.foreground.declined",
        "declined_by_setting" => "tools.foreground.declined_by_setting",
        "unanswered" => "tools.foreground.unanswered",
        "not_asked" => "tools.foreground.not_asked",
        _ => return None,
    };
    Some(crate::tr!(key, app = app).into_owned())
}

fn root_node(line: &str) -> Option<Node> {
    let (reference, rest) = line.split_once(' ')?;
    reference.strip_prefix("@r")?;
    let kind = rest.split_whitespace().next()?;
    let app = attribute(line, "app");
    let title = attribute(line, "title").filter(|title| !title.is_empty());
    Some(Node {
        role: kind.to_owned(),
        name: title.or(app.clone()),
        detail: app,
        reference: Some(reference.to_owned()),
        ..Node::default()
    })
}
