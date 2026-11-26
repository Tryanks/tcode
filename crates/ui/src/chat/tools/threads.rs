//! The thread windows: calls an agent made to Tcode about its threads
//! through `tcode_control` (outline, read, search, message, start, update,
//! notify, approve, report). Each reads as what it did to which thread; the
//! threads and entries it names are links that open them, as a cited anchor
//! does.

use std::collections::HashMap;
use std::path::Path;

use agent::ItemStatus;
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div,
};
use gpui_base::h_flex;
use serde_json::Value;
use tcode_core::session::TimelineEntry;

use super::window::{Argument, Chip, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section};
use crate::chat::model::one_line;
use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

const CONTROL: &[&str] = &["tcode_control"];

/// What a call did, by the tool it called.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Outline,
    Read,
    Search,
    SearchInItem,
    GetThread,
    ListThreads,
    ListMachines,
    Message,
    StartThread,
    Update,
    NotifyWhen,
    SettleSelf,
    Approve,
    Report,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let control = |names: &[&str]| call.served(CONTROL, names);
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
    } else if control(&["list_machines"]) {
        Kind::ListMachines
    } else if control(&["send_message"]) {
        Kind::Message
    } else if control(&["start_thread", "start_thread_on_machine"]) {
        Kind::StartThread
    } else if control(&["update_threads", "control_thread"]) {
        Kind::Update
    } else if control(&["notify_when"]) {
        Kind::NotifyWhen
    } else if control(&["settle_self"]) {
        Kind::SettleSelf
    } else if control(&["respond_approval"]) {
        Kind::Approve
    } else if control(&["report_result"]) {
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

/// A thread a `start_thread` call starts, as the call asks for it and, once
/// it returned, as its answer tells it.
pub(in crate::chat) struct Start<'a> {
    /// The provider by the name `list_providers` gives it.
    pub(in crate::chat) provider: Option<&'a str>,
    pub(in crate::chat) model: Option<&'a str>,
    pub(in crate::chat) title: Option<String>,
    pub(in crate::chat) message: Option<&'a str>,
    /// Whether it reports back to the thread that started it.
    pub(in crate::chat) subagent: bool,
    /// The thread, once the call returned.
    pub(in crate::chat) thread_id: Option<String>,
    /// The branch of its own worktree, when it has one.
    pub(in crate::chat) branch: Option<String>,
    pub(in crate::chat) running: bool,
    pub(in crate::chat) output: Option<&'a str>,
    /// The machine of the group it was started on, by id, when that is not
    /// the calling thread's.
    pub(in crate::chat) machine: Option<&'a str>,
    /// The link the answer gives it.
    pub(in crate::chat) link: Option<String>,
}

/// The thread `entry` starts: a `start_thread` or `start_thread_on_machine`
/// call still running or completed. One that failed, was declined or was
/// interrupted started nothing to show, and stays a tool row that shows what
/// it reported.
pub(in crate::chat) fn start(entry: &TimelineEntry) -> Option<Start<'_>> {
    let call = Call::of(entry)?;
    if !call.served(CONTROL, &["start_thread", "start_thread_on_machine"])
        || !matches!(call.status, ItemStatus::InProgress | ItemStatus::Completed)
    {
        return None;
    }
    let input = call.input;
    let answer = call
        .output
        .and_then(|output| serde_json::from_str::<Value>(output.trim()).ok());
    let answered = |key: &str| {
        answer
            .as_ref()
            .and_then(|answer| string(answer, key))
            .map(str::to_owned)
    };
    let subagent = match &answer {
        Some(answer) => answer.get("subagent") == Some(&Value::Bool(true)),
        None => input.get("subagent") == Some(&Value::Bool(true)),
    };
    Some(Start {
        provider: string(input, "provider"),
        model: string(input, "model"),
        title: answered("title").or_else(|| string(input, "title").map(one_line)),
        message: string(input, "message"),
        subagent,
        thread_id: answered("thread_id"),
        branch: answered("worktree_branch"),
        running: call.status == ItemStatus::InProgress,
        output: call.output,
        machine: string(input, "machine"),
        link: answered("link"),
    })
}

/// A thread, or an entry of one, that a call names: `<thread>#<position>`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(in crate::chat) struct Anchor {
    pub(in crate::chat) thread_id: String,
    pub(in crate::chat) position: Option<u64>,
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

/// The fields in which a call or its answer names a thread by its id alone.
/// An id anywhere else is a project's, a request's or a watch's.
const THREAD_FIELDS: &[&str] = &[
    "thread_id",
    "thread_ids",
    "target",
    "new_thread_id",
    "parent_session_id",
    "parent",
    "children",
    "forked_from",
    "started_by",
];

/// The anchors in `text`, in order: each entry, written `<thread>#<position>`,
/// and each bare thread id where `threads` says the text holds them.
fn anchors_in(text: &str, threads: bool, found: &mut Vec<Anchor>) {
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
            if (threads || anchor.position.is_some()) && !found.contains(&anchor) {
                found.push(anchor);
            }
            index += len + if digits > 0 { digits + 1 } else { 0 };
            continue;
        }
        index += rest.chars().next().map_or(1, char::len_utf8);
    }
}

/// The threads and entries `value` names: an entry wherever its anchor is
/// written, a thread in a field that holds thread ids or in a row that
/// carries its title beside its id.
fn named_in(value: &Value, threads: bool, found: &mut Vec<Anchor>) {
    match value {
        Value::String(text) => anchors_in(text, threads, found),
        Value::Array(items) => items.iter().for_each(|item| named_in(item, threads, found)),
        Value::Object(map) => map.iter().for_each(|(key, item)| {
            let threads =
                THREAD_FIELDS.contains(&key.as_str()) || key == "id" && map.contains_key("title");
            named_in(item, threads, found);
        }),
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
        named_in(call.input, false, &mut anchors);
        let mut titles_found = HashMap::new();
        titles(call.input, &mut titles_found);
        if let Some(output) = &output {
            named_in(output, false, &mut anchors);
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
    /// head of its id. A call names its own thread as `self` (`tcode_control`'s
    /// word, which the server replaces with the id only on its way to the host).
    fn thread(&self, thread_id: &str) -> String {
        if thread_id == "self" {
            return crate::tr!("tools.this_thread").into_owned();
        }
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

/// The text a call carried to a thread: the message or the report.
fn carried(input: &Value) -> Option<&str> {
    string(input, "text").or_else(|| string(input, "message"))
}

/// The variant an input names under `key`: the word itself, or the one key
/// of the object that carries the variant's fields.
fn variant<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    match value.get(key)? {
        Value::String(name) => Some(name),
        Value::Object(fields) => fields.keys().next().map(String::as_str),
        _ => None,
    }
}

impl Threads {
    fn verb(&self) -> Verb {
        Verb(match self.kind {
            Kind::Outline => "outline",
            Kind::Read => "read",
            Kind::Search | Kind::SearchInItem => "search",
            Kind::GetThread => "look_up",
            Kind::ListThreads => "list_threads",
            Kind::ListMachines => "list_machines",
            Kind::Message => "message",
            Kind::StartThread => "start_thread",
            Kind::Update => "update_threads",
            Kind::NotifyWhen => "notify_when",
            Kind::SettleSelf => "settle_self",
            Kind::Approve => "approve",
            Kind::Report => "report",
        })
    }

    /// What the call acted on, and the machine of the group it ran on when
    /// it names one.
    fn argument(&self, call: &Call<'_>, named: &Named) -> Option<Argument> {
        let argument = self.acted_on(call, named);
        match string(call.input, "machine") {
            Some(machine) => Some(argument.unwrap_or_default().machine(machine)),
            None => argument,
        }
    }

    fn acted_on(&self, call: &Call<'_>, named: &Named) -> Option<Argument> {
        let input = call.input;
        let threads = || named.threads(&targets(input));
        let entry = || {
            named
                .anchors
                .iter()
                .find(|anchor| anchor.position.is_some())
        };
        match self.kind {
            Kind::Outline | Kind::GetThread | Kind::Message => Argument::plain(threads()),
            Kind::Read => {
                let mut range = Vec::new();
                [string(input, "from"), string(input, "to")]
                    .into_iter()
                    .flatten()
                    .for_each(|text| anchors_in(text, false, &mut range));
                let text = match range.as_slice() {
                    [
                        Anchor {
                            thread_id,
                            position: Some(from),
                        },
                        Anchor {
                            thread_id: to_thread,
                            position: Some(to),
                        },
                    ] if thread_id == to_thread => {
                        format!("{} #{from}–#{to}", named.thread(thread_id))
                    }
                    [from, ..] => named.label(from),
                    // By `anchors` or a `cursor`: the first entry it names.
                    [] => entry()
                        .map(|anchor| named.label(anchor))
                        .unwrap_or_default(),
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
            Kind::ListMachines => None,
            Kind::ListThreads => Argument::plain(
                string(input, "title_query")
                    .or_else(|| string(input, "scope"))
                    .map(one_line)
                    .unwrap_or_default(),
            ),
            Kind::StartThread => {
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
                                let op = variant(change, "op").unwrap_or_default();
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
            Kind::SettleSelf => Argument::plain(variant(input, "when").unwrap_or_default()),
            Kind::Approve => Argument::plain(
                string(input, "option")
                    .or_else(|| {
                        (input.get("cancel") == Some(&Value::Bool(true))).then_some("cancel")
                    })
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
        if self.kind == Kind::ListMachines {
            let machines = output.get("machines")?.as_array()?.len();
            return Some(Chip::Count(super::counted(
                "machines.dispatch.machines",
                machines,
            )));
        }
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
            Kind::ListMachines => IconName::Network,
            Kind::StartThread => IconName::Bot,
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
        let machine = string(call.input, "machine");
        let links = (!named.anchors.is_empty()).then(|| {
            let cx = ctx.app();
            let links = named.anchors.iter().enumerate().map(|(index, anchor)| {
                thread_link(
                    call.id(),
                    index,
                    named.label(anchor),
                    anchor.clone(),
                    machine,
                    cx,
                )
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
        let carried = carried(call.input).map(|text| {
            let text = bounded_text(call.entry, "carried", text, false, Bound::HEAD, ctx);
            section(&crate::tr!("tools.message"), text, ctx.app()).into_any_element()
        });
        let output = (call.status != ItemStatus::InProgress)
            .then(|| output_section(call, Bound::HEAD, ctx))
            .flatten();
        super::body(links.into_iter().chain(carried).chain(output))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Some(text) = carried(call.input) {
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
/// app opens a cited anchor, or, for a call that ran on another machine of
/// the group (`machine`), where that machine's store places its threads.
fn thread_link(
    call_id: &str,
    index: usize,
    label: String,
    anchor: Anchor,
    machine: Option<&str>,
    cx: &gpui::App,
) -> AnyElement {
    let theme = cx.theme();
    let selector = format!("tool-thread-{call_id}-{index}");
    let url = match (machine, anchor.position) {
        (Some(machine), Some(position)) => {
            format!("tcode-thread://{}@{machine}#{position}", anchor.thread_id)
        }
        (Some(machine), None) => format!("tcode-thread://{}@{machine}", anchor.thread_id),
        (None, position) => crate::markdown::OpenThreadAnchor {
            thread_id: anchor.thread_id.clone(),
            position,
        }
        .url(),
    };
    // A thread on a machine this window is not linked to opens nowhere.
    let closed = machine
        .map(|machine| crate::machines::machine(machine, cx))
        .filter(|machine| machine.store.is_none())
        .map(|machine| crate::machines::not_open(&machine.name));
    let machine = machine.map(str::to_owned);
    let accent = theme.accent;
    let link = div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .max_w(gpui::px(320.))
        .overflow_hidden()
        .px_1()
        .rounded(crate::material::radius_chip(cx))
        .text_secondary(false, cx)
        .child(crate::widgets::fade_text(label));
    let link = match closed {
        Some(closed) => link
            .text_color(theme.muted_foreground)
            .with_tooltip(move |_, _| crate::widgets::tooltip::Tooltip::new(closed.to_string())),
        None => link
            .cursor_pointer()
            .hover(move |link| link.bg(accent))
            .text_color(theme.link)
            .on_click(move |_, window, cx| match &machine {
                Some(machine) => {
                    crate::machines::open_thread(machine, &anchor.thread_id, anchor.position, cx);
                }
                None => window.dispatch_action(
                    Box::new(crate::markdown::OpenThreadAnchor {
                        thread_id: anchor.thread_id.clone(),
                        position: anchor.position,
                    }),
                    cx,
                ),
            }),
    };
    link.context_menu(move |menu, _, _| {
        menu.menu(
            crate::tr!("tools.copy_link").into_owned(),
            Box::new(CopyText(url.clone())),
        )
    })
    .into_any_element()
}
