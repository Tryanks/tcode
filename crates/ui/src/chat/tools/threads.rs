//! The thread windows: calls an agent made to Tcode about its threads —
//! `tcode_control` (outline, read, search, message, start, update, notify,
//! approve, report), and the `tcode_orchestrate` (dispatch, send, result,
//! settle) and `tcode_report` calls transcripts recorded before those moved
//! into `tcode_control`.
//! Each reads as what it did to which thread; the threads and entries it
//! names are links that open them, as a cited anchor does.

use std::collections::HashMap;
use std::path::Path;

use agent::ItemStatus;
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div,
};
use gpui_base::h_flex;
use serde_json::Value;

use super::window::{Argument, Chip, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

const CONTROL: &[&str] = &["tcode_control"];
const ORCHESTRATE: &[&str] = &["tcode_orchestrate"];
const REPORT: &[&str] = &["tcode_report"];

/// What a call did, by the tool it called.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Outline,
    Read,
    Search,
    SearchInItem,
    GetThread,
    ListThreads,
    Message,
    StartThread,
    Update,
    NotifyWhen,
    SettleSelf,
    RequestGrant,
    Dispatch,
    Result,
    Status,
    Settle,
    Archive,
    Cancel,
    Approve,
    Report,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let control = |names: &[&str]| call.served(CONTROL, names, false);
    let orchestrate = |names: &[&str]| call.served(ORCHESTRATE, names, false);
    let kind = if control(&["outline"]) {
        Kind::Outline
    } else if control(&["read"]) {
        Kind::Read
    } else if control(&["search"]) {
        Kind::Search
    } else if control(&["search_in_item"]) {
        Kind::SearchInItem
    } else if control(&["get_thread"]) {
        Kind::GetThread
    } else if control(&["list_threads"]) {
        Kind::ListThreads
    } else if control(&["send_message"]) || orchestrate(&["send"]) {
        Kind::Message
    } else if control(&["start_thread"]) {
        Kind::StartThread
    } else if control(&["update_threads", "control_thread"]) {
        Kind::Update
    } else if control(&["notify_when"]) {
        Kind::NotifyWhen
    } else if control(&["settle_self"]) {
        Kind::SettleSelf
    } else if control(&["request_grant"]) {
        Kind::RequestGrant
    } else if orchestrate(&["dispatch", "collaborate"]) {
        Kind::Dispatch
    } else if orchestrate(&["result"]) {
        Kind::Result
    } else if orchestrate(&["status"]) {
        Kind::Status
    } else if orchestrate(&["settle"]) {
        Kind::Settle
    } else if orchestrate(&["archive"]) {
        Kind::Archive
    } else if orchestrate(&["cancel"]) {
        Kind::Cancel
    } else if control(&["respond_approval"]) || orchestrate(&["approve"]) {
        Kind::Approve
    } else if control(&["report_result"]) || call.served(REPORT, &["report_result"], false) {
        Kind::Report
    } else {
        return None;
    };
    call.input
        .is_object()
        .then(|| Box::new(Threads { kind }) as Box<dyn ToolView + 'a>)
}

struct Threads {
    kind: Kind,
}

/// A thread, or an entry of one, that a call names: `<thread>#<position>`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::chat) struct Anchor {
    pub(in crate::chat) thread_id: String,
    pub(in crate::chat) position: Option<u64>,
}

impl Anchor {
    fn url(&self) -> String {
        match self.position {
            Some(position) => format!("tcode-thread://{}#{position}", self.thread_id),
            None => format!("tcode-thread://{}", self.thread_id),
        }
    }
}

/// Whether `text` starts with a thread id: a UUID.
fn uuid_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes().get(..36)?;
    let ok = bytes.iter().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => *byte == b'-',
        _ => byte.is_ascii_hexdigit(),
    });
    ok.then_some(36)
}

/// The anchors in `text`, in order.
fn anchors_in(text: &str, found: &mut Vec<Anchor>) {
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        let boundary = index == 0
            || !text.as_bytes()[index - 1].is_ascii_alphanumeric()
                && text.as_bytes()[index - 1] != b'-';
        if boundary && let Some(len) = uuid_len(rest) {
            let thread_id = rest[..len].to_owned();
            let after = &rest[len..];
            let digits = after
                .strip_prefix('#')
                .map(|after| after.bytes().take_while(u8::is_ascii_digit).count())
                .unwrap_or(0);
            let position = (digits > 0)
                .then(|| after[1..=digits].parse().ok())
                .flatten();
            let anchor = Anchor {
                thread_id,
                position,
            };
            if !found.contains(&anchor) {
                found.push(anchor);
            }
            index += len + if digits > 0 { digits + 1 } else { 0 };
            continue;
        }
        index += rest.chars().next().map_or(1, char::len_utf8);
    }
}

fn walk_strings<'v>(value: &'v Value, visit: &mut impl FnMut(&'v str)) {
    match value {
        Value::String(text) => visit(text),
        Value::Array(items) => items.iter().for_each(|item| walk_strings(item, visit)),
        Value::Object(map) => map.values().for_each(|item| walk_strings(item, visit)),
        _ => {}
    }
}

/// The titles an output gives the threads it lists: every object with an
/// `id` or `thread_id` and a `title`.
fn titles(value: &Value, found: &mut HashMap<String, String>) {
    match value {
        Value::Object(map) => {
            let id = map
                .get("thread_id")
                .or_else(|| map.get("id"))
                .and_then(Value::as_str);
            let title = map.get("title").and_then(Value::as_str);
            if let (Some(id), Some(title)) = (id, title)
                && uuid_len(id).is_some()
                && !title.trim().is_empty()
            {
                found
                    .entry(id.to_owned())
                    .or_insert_with(|| one_line(title));
            }
            map.values().for_each(|item| titles(item, found));
        }
        Value::Array(items) => items.iter().for_each(|item| titles(item, found)),
        _ => {}
    }
}

/// What a call says of the threads it names: each anchor, and the titles
/// its input and output give them.
struct Named {
    anchors: Vec<Anchor>,
    titles: HashMap<String, String>,
}

impl Named {
    fn of(call: &Call<'_>) -> Self {
        let mut anchors = Vec::new();
        let output = call
            .output
            .and_then(|output| serde_json::from_str::<Value>(output.trim()).ok());
        walk_strings(call.input, &mut |text| anchors_in(text, &mut anchors));
        let mut titles_found = HashMap::new();
        titles(call.input, &mut titles_found);
        if let Some(output) = &output {
            walk_strings(output, &mut |text| anchors_in(text, &mut anchors));
            titles(output, &mut titles_found);
        }
        // An entry's thread is named by the entry; the bare thread adds
        // nothing beside it.
        let entries: Vec<String> = anchors
            .iter()
            .filter(|anchor| anchor.position.is_some())
            .map(|anchor| anchor.thread_id.clone())
            .collect();
        anchors.retain(|anchor| anchor.position.is_some() || !entries.contains(&anchor.thread_id));
        Self {
            anchors,
            titles: titles_found,
        }
    }

    /// How a thread reads: its title when the call gave it one, else the
    /// head of its id.
    fn thread(&self, thread_id: &str) -> String {
        self.titles
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| thread_id.chars().take(8).collect())
    }

    fn label(&self, anchor: &Anchor) -> String {
        match anchor.position {
            Some(position) => format!("{} #{position}", self.thread(&anchor.thread_id)),
            None => self.thread(&anchor.thread_id),
        }
    }

    /// The threads `ids` name, as they read, joined.
    fn threads(&self, ids: &[&str]) -> String {
        crate::i18n::join_list(ids.iter().map(|id| self.thread(id)))
    }
}

fn string<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value
        .get(key)?
        .as_str()
        .filter(|text| !text.trim().is_empty())
}

fn strings<'v>(value: &'v Value, key: &str) -> Vec<&'v str> {
    match value.get(key) {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The ids of the threads a call acts on, from whichever field it names
/// them in.
fn targets(input: &Value) -> Vec<&str> {
    let mut ids = strings(input, "thread_ids");
    ids.extend(strings(input, "thread_id"));
    if let Some(changes) = input.get("changes").and_then(Value::as_array) {
        ids.extend(
            changes
                .iter()
                .filter_map(|change| string(change, "thread_id")),
        );
    }
    ids
}

/// The text a call carried to a thread: the message, the brief, the report.
fn carried(input: &Value) -> Option<(&'static str, &str)> {
    [
        ("text", "tools.message"),
        ("message", "tools.message"),
        ("brief", "tools.brief"),
        ("reason", "tools.reason"),
    ]
    .into_iter()
    .find_map(|(key, label)| string(input, key).map(|text| (label, text)))
}

impl Threads {
    fn verb(&self) -> Verb {
        Verb(match self.kind {
            Kind::Outline => "outline",
            Kind::Read => "read",
            Kind::Search | Kind::SearchInItem => "search",
            Kind::GetThread => "look_up",
            Kind::ListThreads => "list_threads",
            Kind::Message => "message",
            Kind::StartThread => "start_thread",
            Kind::Update => "update_threads",
            Kind::NotifyWhen => "notify_when",
            Kind::SettleSelf => "settle_self",
            Kind::RequestGrant => "request_grant",
            Kind::Dispatch => "dispatch",
            Kind::Result => "result",
            Kind::Status => "check_threads",
            Kind::Settle => "settle",
            Kind::Archive => "archive",
            Kind::Cancel => "cancel",
            Kind::Approve => "approve",
            Kind::Report => "report",
        })
    }

    fn argument(&self, call: &Call<'_>, named: &Named) -> Option<Argument> {
        let input = call.input;
        let threads = || named.threads(&targets(input));
        let entry = || {
            named
                .anchors
                .iter()
                .find(|anchor| anchor.position.is_some())
        };
        match self.kind {
            Kind::Outline
            | Kind::GetThread
            | Kind::Message
            | Kind::Result
            | Kind::Status
            | Kind::Settle
            | Kind::Archive
            | Kind::Cancel => Argument::plain(threads()),
            Kind::Read => {
                let from = string(input, "from").or_else(|| string(input, "anchor"));
                let to = string(input, "to");
                let mut found = Vec::new();
                from.into_iter()
                    .chain(to)
                    .for_each(|text| anchors_in(text, &mut found));
                let text = match found.as_slice() {
                    [from, to] if from.thread_id == to.thread_id => format!(
                        "{} #{}–#{}",
                        named.thread(&from.thread_id),
                        from.position.unwrap_or_default(),
                        to.position.unwrap_or_default()
                    ),
                    [first, ..] => named.label(first),
                    [] => threads(),
                };
                Argument::plain(text)
            }
            Kind::Search => Argument::plain(
                string(input, "query").map(one_line).unwrap_or_default(),
            )
            .map(|argument| {
                let scope = targets(input);
                argument.detail((!scope.is_empty()).then(|| named.threads(&scope)))
            }),
            Kind::SearchInItem => {
                Argument::plain(string(input, "query").map(one_line).unwrap_or_default())
                    .map(|argument| argument.detail(entry().map(|anchor| named.label(anchor))))
            }
            Kind::ListThreads => Argument::plain(
                string(input, "title_query")
                    .or_else(|| string(input, "query"))
                    .or_else(|| string(input, "scope"))
                    .map(one_line)
                    .unwrap_or_default(),
            ),
            Kind::StartThread | Kind::Dispatch => {
                let model = [string(input, "provider"), string(input, "model")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                Argument::plain(string(input, "title").map(one_line).unwrap_or_default())
                    .map(|argument| argument.detail(Some(model)))
            }
            Kind::Update => {
                let changes = input
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|changes| {
                        changes
                            .iter()
                            .map(|change| {
                                let op = string(change, "op").unwrap_or_default();
                                let thread = string(change, "thread_id")
                                    .map(|id| named.thread(id))
                                    .unwrap_or_default();
                                format!("{op} {thread}").trim().to_owned()
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                Argument::plain(changes)
            }
            // The events it watches for stay in the body: beside the threads
            // they would crowd them out of a narrow header.
            Kind::NotifyWhen => Argument::plain(threads()),
            Kind::SettleSelf => Argument::plain(string(input, "when").unwrap_or_default()),
            Kind::RequestGrant => {
                let scope = [string(input, "scope").map(str::to_owned)]
                    .into_iter()
                    .flatten()
                    .chain([strings(input, "tiers").join(", ")])
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                Argument::plain(scope)
            }
            Kind::Approve => Argument::plain(
                string(input, "option")
                    .or_else(|| {
                        (input.get("cancel") == Some(&Value::Bool(true))).then_some("cancel")
                    })
                    .or_else(|| string(input, "decision"))
                    .unwrap_or_default(),
            ),
            Kind::Report => Argument::plain(
                string(input, "text")
                    .and_then(super::window::first_line)
                    .map(|line| one_line(line.trim_start_matches('#').trim()))
                    .unwrap_or_default(),
            ),
        }
    }

    /// The count a search or a listing reports.
    fn count(&self, call: &Call<'_>) -> Option<Chip> {
        let output = serde_json::from_str::<Value>(call.output?.trim()).ok()?;
        let total = output.get("total")?.as_u64()? as usize;
        let key = match self.kind {
            Kind::Search => "tools.hits",
            Kind::ListThreads => "tools.threads",
            _ => return None,
        };
        Some(Chip::Count(super::counted(key, total)))
    }
}

impl ToolView for Threads {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let named = Named::of(call);
        let icon = match self.kind {
            Kind::Search | Kind::SearchInItem | Kind::ListThreads => IconName::Search,
            Kind::Dispatch | Kind::StartThread => IconName::Bot,
            Kind::Report => IconName::FileText,
            Kind::NotifyWhen => IconName::Bell,
            _ => IconName::MessageSquare,
        };
        Header::new(icon, self.verb())
            .argument(self.argument(call, &named))
            .chip(self.count(call))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let named = Named::of(call);
        let links = (!named.anchors.is_empty()).then(|| {
            let cx = ctx.app();
            let links = named.anchors.iter().enumerate().map(|(index, anchor)| {
                thread_link(call.id(), index, named.label(anchor), anchor.clone(), cx)
            });
            section(
                &crate::tr!("tools.threads_caption"),
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .flex_wrap()
                    .children(links),
                cx,
            )
            .into_any_element()
        });
        let carried = carried(call.input).map(|(label, text)| {
            let text = bounded_text(call.entry, "carried", text, false, Bound::HEAD, ctx);
            section(&crate::tr!(label), text, ctx.app()).into_any_element()
        });
        let output = (call.status != ItemStatus::InProgress)
            .then(|| output_section(call, Bound::HEAD, ctx))
            .flatten();
        super::body(links.into_iter().chain(carried).chain(output))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Some((_, text)) = carried(call.input) {
            copy(&mut items, "chat.copy_text", text);
        }
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}

/// A thread or entry the call named, as a link that opens it where the
/// app opens a cited anchor.
fn thread_link(
    call_id: &str,
    index: usize,
    label: String,
    anchor: Anchor,
    cx: &gpui::App,
) -> AnyElement {
    let theme = cx.theme();
    let selector = format!("tool-thread-{call_id}-{index}");
    let url = anchor.url();
    let open = crate::markdown::OpenThreadAnchor {
        thread_id: anchor.thread_id,
        position: anchor.position,
    };
    let accent = theme.accent;
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .max_w(gpui::px(320.))
        .overflow_hidden()
        .px_1()
        .rounded(crate::material::radius_chip(cx))
        .cursor_pointer()
        .hover(move |link| link.bg(accent))
        .text_secondary(false, cx)
        .text_color(theme.link)
        .child(crate::widgets::fade_text(label))
        .on_click(move |_, window, cx| {
            window.dispatch_action(Box::new(open.clone()), cx);
        })
        .context_menu(move |menu, _, _| {
            menu.menu(
                crate::tr!("tools.copy_link").into_owned(),
                Box::new(CopyText(url.clone())),
            )
        })
        .into_any_element()
}
