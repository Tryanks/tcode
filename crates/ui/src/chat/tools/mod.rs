//! Tool windows: what each tool call did, drawn from its canonical item.
//!
//! A window is chosen from what the item carries, never from the provider
//! that produced it (Principle 4): first its `ItemContent` kind — a command
//! and a file change are only ever those items — and then, for a
//! `ToolCall`, by its tool name
//! ([`agent::ToolName::parse`], so `mcp__s__t` and `s/t` name one tool)
//! and the shape of its input. Each family view tries its names and
//! deserializes the input into its own typed fields; a call no family
//! accepts, or whose input is not the shape a family expects, gets the
//! generic window. A provider that names a capability differently simply
//! gets the generic window until a matcher accepts its name and shape.

pub(in crate::chat) mod desktop;
pub(in crate::chat) mod diff;
pub(in crate::chat) mod generic;
pub(in crate::chat) mod harness;
pub(in crate::chat) mod other;
pub(in crate::chat) mod outline;
pub(in crate::chat) mod preview;
pub(in crate::chat) mod question;
pub(in crate::chat) mod read;
pub(in crate::chat) mod run;
pub(in crate::chat) mod script;
pub(in crate::chat) mod search;
pub(in crate::chat) mod shell;
pub(in crate::chat) mod subagent;
pub(in crate::chat) mod threads;
pub(in crate::chat) mod web;
pub(in crate::chat) mod window;

use std::{any::Any, collections::HashMap, ops::Deref, path::Path, rc::Rc, sync::Arc};

use agent::{Attachment, ItemContent, ItemStatus, ToolFamily, ToolName};
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use serde_json::Value;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::timeline::TimelineView;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);
pub(in crate::chat) use window::{ClickHandler, Header};

/// A `ToolCall` item as a view reads it.
pub(in crate::chat) struct Call<'a> {
    pub(in crate::chat) entry: &'a TimelineEntry,
    pub(in crate::chat) name: ToolName<'a>,
    pub(in crate::chat) input: &'a Value,
    pub(in crate::chat) output: Option<&'a str>,
    pub(in crate::chat) images: &'a [Attachment],
    pub(in crate::chat) status: ItemStatus,
    pub(in crate::chat) family: Option<&'a ToolFamily>,
}

impl<'a> Call<'a> {
    pub(in crate::chat) fn of(entry: &'a TimelineEntry) -> Option<Self> {
        let EntryContent::Item(ItemContent::ToolCall {
            image_reads,
            name,
            input,
            output,
            status,
            family,
        }) = &entry.content
        else {
            return None;
        };
        Some(Self {
            entry,
            name: ToolName::parse(name),
            input,
            output: output.as_deref(),
            images: image_reads,
            status: *status,
            family: family.as_deref(),
        })
    }

    /// Whether the call is to the harness's own tool by one of `names`, in
    /// any case: harnesses spell the same tool `Read` and `read`. An MCP
    /// server's tool of the same name (tcode_control's `search` and `read`)
    /// is that server's, not the family's.
    pub(in crate::chat) fn named(&self, names: &[&str]) -> bool {
        self.name.server.is_none()
            && names
                .iter()
                .any(|name| self.name.tool.eq_ignore_ascii_case(name))
    }

    /// Whether the call is to one of `servers`' tools named one of `names`.
    /// A transcript imported without its MCP namespace keeps only the tool's
    /// name, so `bare` also accepts the name alone, for names no harness
    /// gives a tool of its own.
    pub(in crate::chat) fn served(&self, servers: &[&str], names: &[&str], bare: bool) -> bool {
        let server = match self.name.server {
            Some(server) => servers.contains(&server),
            None => bare,
        };
        server && names.contains(&self.name.tool)
    }

    /// The input as a view's typed fields, borrowing its strings; `None`
    /// when the input is not that shape.
    pub(in crate::chat) fn input<T: Deserialize<'a>>(&self) -> Option<T> {
        T::deserialize(self.input).ok()
    }

    pub(in crate::chat) fn id(&self) -> &'a str {
        &self.entry.id
    }
}

/// A tool as the windows name it: "server · tool", or the tool alone.
pub(crate) fn subject(name: ToolName<'_>) -> String {
    match name.server {
        Some(server) => format!("{server} · {}", name.tool),
        None => name.tool.to_owned(),
    }
}

/// One family's window for a call it accepts.
pub(in crate::chat) trait ToolView {
    /// `cwd` is the thread's, which paths show relative to.
    fn header(&self, call: &Call<'_>, cwd: &Path) -> Header;
    /// The window's body, built only while it is open.
    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement;
    /// What the window's context menu copies: whole values a text selection
    /// cannot reach in a preview.
    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)>;
    /// A file the call is about, which the menu offers to open.
    fn path(&self) -> Option<&str> {
        None
    }
}

/// A family's matcher: its view when it accepts the call's name and input.
type Matcher = for<'a> fn(&Call<'a>) -> Option<Box<dyn ToolView + 'a>>;

/// Every family that claims calls by name and shape, in the order they are
/// asked. The generic window takes whatever none accepts.
const FAMILIES: &[Matcher] = &[
    read::tool_call,
    search::tool_call,
    web::tool_call,
    script::tool_call,
    desktop::tool_call,
    preview::tool_call,
    threads::tool_call,
    harness::tool_call,
    question::tool_call,
    other::tool_call,
];

/// Whether `call` drives a computer — a desktop app or the preview
/// browser — which a run of such calls is counted and grouped as.
pub(in crate::chat) fn drives_computer(call: &Call<'_>) -> bool {
    script::tool_call(call).is_some()
        || desktop::tool_call(call).is_some()
        || preview::tool_call(call).is_some()
}

pub(in crate::chat) fn view<'a>(call: &Call<'a>) -> Box<dyn ToolView + 'a> {
    FAMILIES
        .iter()
        .find_map(|family| family(call))
        .unwrap_or_else(|| Box::new(generic::Generic::of(call)))
}

/// The kind of window an activity shows as, which is what a run of
/// activities counts it as.
pub(in crate::chat) enum Family {
    Command,
    /// The files it edited.
    Edit(Vec<String>),
    Read,
    Search,
    /// An action on a computer: a desktop app or the preview browser.
    Computer,
    Tool,
    Agent,
}

/// Which family's window `entry` shows as: its item kind, then for a tool
/// call the family that claims it, as [`activity`] and the edit rows
/// dispatch it. `None` for what is not an activity.
pub(in crate::chat) fn family(entry: &TimelineEntry) -> Option<Family> {
    let EntryContent::Item(item) = &entry.content else {
        return None;
    };
    Some(match item {
        ItemContent::CommandExecution { .. } => Family::Command,
        ItemContent::FileChange { changes, .. } => {
            Family::Edit(changes.iter().map(|change| change.path.clone()).collect())
        }
        ItemContent::ToolCall { .. } => {
            let call = Call::of(entry)?;
            if read::tool_call(&call).is_some() {
                Family::Read
            } else if search::tool_call(&call).is_some() {
                Family::Search
            } else if drives_computer(&call) {
                Family::Computer
            } else {
                Family::Tool
            }
        }
        ItemContent::ImageRead { .. } => Family::Read,
        ItemContent::WebSearch { .. } | ItemContent::Other { .. } => Family::Tool,
        ItemContent::Subagent { .. } => Family::Agent,
        ItemContent::UserMessage { .. }
        | ItemContent::AssistantMessage { .. }
        | ItemContent::Reasoning { .. }
        | ItemContent::Plan { .. } => return None,
    })
}

/// An activity's window as its kind gives it, before its body.
pub(in crate::chat) struct Activity {
    pub(in crate::chat) header: Header,
    pub(in crate::chat) status: ItemStatus,
    /// The line its failure is told by, when it failed: taken from its
    /// output here rather than the whole output kept for it.
    failure: Option<String>,
    pub(in crate::chat) copy: Vec<(String, String)>,
    pub(in crate::chat) expandable: bool,
    /// A file the activity is about.
    pub(in crate::chat) path: Option<String>,
}

impl Activity {
    /// The line its failure is told by, when it failed.
    pub(in crate::chat) fn failure(&self) -> Option<String> {
        self.failure.clone()
    }
}

/// The window of one Work Log activity other than a file change:
/// dispatched on its item kind, then for a tool call on the family that
/// claims it. One still running that its turn no longer waits on says so.
pub(in crate::chat) fn activity(
    entry: &TimelineEntry,
    live_reasoning: bool,
    cwd: &Path,
) -> Activity {
    let mut activity = kind_activity(entry, live_reasoning, cwd);
    activity.header.background = entry.background && activity.status == ItemStatus::InProgress;
    activity
}

fn kind_activity(entry: &TimelineEntry, live_reasoning: bool, cwd: &Path) -> Activity {
    let settled = |header, copy| Activity {
        header,
        status: ItemStatus::Completed,
        failure: None,
        copy,
        expandable: false,
        path: None,
    };
    let EntryContent::Item(item) = &entry.content else {
        return settled(Header::bare(crate::icon::IconName::Info), Vec::new());
    };
    match item {
        ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        } => {
            let header = shell::command_header(command, output, *exit_code, *status);
            Activity {
                failure: window::failure_line(&header, *status, Some(output)),
                header,
                status: *status,
                copy: shell::command_copy_items(command, output),
                expandable: true,
                path: None,
            }
        }
        ItemContent::ToolCall { .. } => {
            let call = Call::of(entry).expect("a tool call");
            let view = view(&call);
            let header = view.header(&call, cwd);
            Activity {
                failure: window::failure_line(&header, call.status, call.output),
                header,
                status: call.status,
                copy: view.copy_items(&call),
                expandable: true,
                path: view.path().map(str::to_owned),
            }
        }
        ItemContent::Reasoning { text } => {
            let mut items = Vec::new();
            copy(&mut items, "chat.copy_text", text);
            Activity {
                header: Header::with_icon("icons/sparkles.svg", window::Verb("think")).shimmer(),
                status: if live_reasoning {
                    ItemStatus::InProgress
                } else {
                    ItemStatus::Completed
                },
                failure: None,
                copy: items,
                expandable: !live_reasoning,
                path: None,
            }
        }
        ItemContent::WebSearch { query } => {
            let mut items = Vec::new();
            copy(&mut items, "chat.copy_text", query);
            settled(web::search_item_header(query), items)
        }
        ItemContent::Other {
            provider_kind,
            summary,
        } => {
            let mut items = Vec::new();
            copy(&mut items, "chat.copy_text", summary);
            settled(generic::other_header(provider_kind, summary), items)
        }
        ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        } => subagent::activity(
            agent_type,
            description,
            *status,
            summary.as_deref(),
            model.as_deref(),
            effort.as_deref(),
        ),
        _ => settled(Header::bare(crate::icon::IconName::Info), Vec::new()),
    }
}

/// What a window copies, for a surface outside the window that offers the
/// same (an image tile of the call).
pub(in crate::chat) fn copy_items(entry: &TimelineEntry) -> Vec<(String, String)> {
    match Call::of(entry) {
        Some(call) => view(&call).copy_items(&call),
        None => Vec::new(),
    }
}

/// A count as it reads: the `{key}_one` label for one, else `key`'s.
pub(in crate::chat) fn counted(key: &str, count: usize) -> String {
    if count == 1 {
        crate::tr!(&format!("{key}_one")).into_owned()
    } else {
        crate::tr!(key, count = count).into_owned()
    }
}

/// A copy-menu item, when there is something to copy.
pub(in crate::chat) fn copy(items: &mut Vec<(String, String)>, key: &str, text: &str) {
    if !text.trim().is_empty() {
        items.push((crate::tr!(key).into_owned(), text.to_owned()));
    }
}

/// A tool output that reached this client as a preview, and how far the
/// reader has gone in fetching the rest.
pub(in crate::chat) enum ElidedOutput {
    Preview {
        full_bytes: u64,
        on_load: ClickHandler,
    },
    Loading,
    Loaded(SharedString),
    Failed {
        error: String,
        on_load: ClickHandler,
    },
}

/// The chat services a window's body reaches, and only through here.
pub(in crate::chat) struct ToolCtx<'a, 'b> {
    pub(in crate::chat) chat: &'a mut TimelineView,
    pub(in crate::chat) cx: &'a mut Context<'b, TimelineView>,
    pub(in crate::chat) cwd: &'a Path,
    /// A completed call's images show as tiles above its row, so its body
    /// leaves them out.
    pub(in crate::chat) images_inline: bool,
}

impl ToolCtx<'_, '_> {
    pub(in crate::chat) fn app(&self) -> &App {
        self.cx
    }

    pub(in crate::chat) fn session_id(&self) -> String {
        self.chat.session_key.clone().unwrap_or_default()
    }

    /// Whether the part of a body `key` names is open: a long text shown
    /// whole, a nested value unfolded.
    pub(in crate::chat) fn is_open(&self, key: &str) -> bool {
        self.chat.expanded.contains(key)
    }

    /// Opens or shuts the part of `entry`'s body that `key` names.
    pub(in crate::chat) fn toggle(&mut self, entry: &TimelineEntry, key: String) -> ClickHandler {
        let (turn, entry_id) = (entry.turn, entry.id.clone());
        Box::new(self.cx.listener(move |this, _, _, cx| {
            if !this.expanded.remove(&key) {
                this.expanded.insert(key.clone());
            }
            this.remeasure_activity(turn, &entry_id, cx);
            cx.notify();
        }))
    }

    /// How much of `entry`'s output this client holds, when the record
    /// carried only a preview of it.
    pub(in crate::chat) fn elided(&mut self, entry: &TimelineEntry) -> Option<ElidedOutput> {
        self.chat.elided_output(&entry.id, entry.turn, self.cx)
    }

    /// What `derive` makes of `input`, derived again only once `input`
    /// differs from the text it was last derived from under `key`.
    pub(in crate::chat) fn derived<T: 'static>(
        &mut self,
        key: String,
        input: &str,
        derive: impl FnOnce(&str) -> T,
    ) -> Rc<T> {
        self.chat.tool_texts.get(key, input, derive)
    }

    /// The highlighting theme the windows draw in.
    pub(in crate::chat) fn highlight_theme(&self) -> Arc<crate::highlight::HighlightTheme> {
        self.cx.theme().highlight_theme.clone()
    }
}

/// What the windows' bodies derive from their calls' text -- the lines they
/// show, the highlighting, an output pretty-printed -- kept while a window
/// is drawn, so a frame that draws it again does not derive it again. A
/// finished call's text never changes; a running one's is compared, not
/// trusted, so what changed is derived again.
#[derive(Default)]
pub(in crate::chat) struct DerivedTexts {
    entries: HashMap<String, Derived>,
    theme: Option<Arc<crate::highlight::HighlightTheme>>,
}

struct Derived {
    input: Box<str>,
    value: Rc<dyn Any>,
    drawn: bool,
}

impl DerivedTexts {
    pub(in crate::chat) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Call once per frame, before the rows render: what the last frame did
    /// not draw goes, and everything once the highlighting theme changed.
    pub(in crate::chat) fn sweep(&mut self, theme: &Arc<crate::highlight::HighlightTheme>) {
        if !self
            .theme
            .as_ref()
            .is_some_and(|drawn| Arc::ptr_eq(drawn, theme))
        {
            self.entries.clear();
            self.theme = Some(theme.clone());
        }
        self.entries
            .retain(|_, derived| std::mem::take(&mut derived.drawn));
    }

    fn get<T: 'static>(
        &mut self,
        key: String,
        input: &str,
        derive: impl FnOnce(&str) -> T,
    ) -> Rc<T> {
        if let Some(derived) = self.entries.get_mut(&key)
            && *derived.input == *input
            && let Ok(value) = derived.value.clone().downcast::<T>()
        {
            derived.drawn = true;
            return value;
        }
        let value = Rc::new(derive(input));
        self.entries.insert(
            key,
            Derived {
                input: input.into(),
                value: value.clone(),
                drawn: true,
            },
        );
        value
    }
}

/// How much of a long text a body shows before the reader asks for more.
#[derive(Clone, Copy)]
pub(in crate::chat) struct Bound {
    pub(in crate::chat) head: usize,
    pub(in crate::chat) tail: usize,
}

impl Bound {
    /// Output: where a run ended matters as much as how it began.
    pub(in crate::chat) const OUTPUT: Self = Self { head: 8, tail: 6 };
    /// A value read top down: an argument, a file, a list.
    pub(in crate::chat) const HEAD: Self = Self { head: 12, tail: 0 };
}

/// The height an opened text scrolls within.
const OPEN_MAX_HEIGHT: f32 = 360.;

/// The lines a bounded text shows, and the lines it hides between them.
pub(in crate::chat) struct Bounded<'t> {
    pub(in crate::chat) head: Vec<&'t str>,
    pub(in crate::chat) hidden: usize,
    pub(in crate::chat) tail: Vec<&'t str>,
}

/// Splits `text` into what a shut body shows: all of it when it is short,
/// else its first `head` and last `tail` lines around a count of the rest.
pub(in crate::chat) fn bound(text: &str, bound: Bound) -> Bounded<'_> {
    let lines: Vec<&str> = text.lines().collect();
    // Hiding two lines behind a control saves nothing.
    if lines.len() <= bound.head + bound.tail + 2 {
        return Bounded {
            head: lines,
            hidden: 0,
            tail: Vec::new(),
        };
    }
    Bounded {
        head: lines[..bound.head].to_vec(),
        hidden: lines.len() - bound.head - bound.tail,
        tail: lines[lines.len() - bound.tail..].to_vec(),
    }
}

/// A long text, bounded with an honest count of what it leaves out: open,
/// it shows whole in a capped scroll.
pub(in crate::chat) fn bounded_text(
    entry: &TimelineEntry,
    key: &str,
    text: &str,
    mono: bool,
    limit: Bound,
    ctx: &mut ToolCtx<'_, '_>,
) -> AnyElement {
    let key = format!("tool-text-{}-{key}", entry.id);
    let open = ctx.is_open(&key);
    let shown = ctx.derived(
        format!("{key}-{open}-{}-{}", limit.head, limit.tail),
        text,
        |text| Shown::of(text, open, limit),
    );
    let style = |el: gpui::Div, cx: &App| {
        el.w_full()
            .min_w_0()
            .text_secondary(false, cx)
            .text_color(cx.theme().foreground.opacity(0.85))
            .when(mono, |el| {
                el.font_family(cx.theme().mono_font_family.clone())
            })
            .whitespace_normal()
    };
    let selector = key.clone();
    let tagged = move |el: AnyElement| {
        let selector = selector.clone();
        div()
            .w_full()
            .min_w_0()
            .debug_selector(move || selector.clone())
            .child(el)
            .into_any_element()
    };
    if let Shown::Whole(text) = &*shown {
        return tagged(
            style(div(), ctx.app())
                .child(text.clone())
                .into_any_element(),
        );
    }
    let control = |label: String, ctx: &mut ToolCtx<'_, '_>| {
        let on_click = ctx.toggle(entry, key.clone());
        let selector = format!("{key}-more");
        h_flex()
            .child(div().debug_selector(move || selector.clone()).child(
                super::components::changed_files::quiet_control(
                    SharedString::from(format!("{key}-more")),
                    label.into(),
                    on_click,
                    ctx.app(),
                ),
            ))
            .into_any_element()
    };
    if let Shown::Open(shown) = &*shown {
        let shown = shown.clone();
        let less = control(crate::tr!("tools.show_less").into_owned(), ctx);
        return tagged(
            v_flex()
                .w_full()
                .gap_1()
                .child(
                    style(div(), ctx.app())
                        .id(SharedString::from(key.clone()))
                        .max_h(px(OPEN_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .child(shown),
                )
                .child(less)
                .into_any_element(),
        );
    }
    let Shown::Folded { head, hidden, tail } = &*shown else {
        unreachable!("a text is shown whole, open or folded")
    };
    let more = control(
        crate::tr!("tools.more_lines", count = hidden).into_owned(),
        ctx,
    );
    let cx = ctx.app();
    tagged(
        v_flex()
            .w_full()
            .min_w_0()
            .child(style(div(), cx).child(head.clone()))
            .child(more)
            .when(!tail.is_empty(), |text| {
                text.child(style(div(), cx).child(tail.clone()))
            })
            .into_any_element(),
    )
}

/// What a bounded text shows.
enum Shown {
    /// All of it: it is short.
    Whole(SharedString),
    /// All of it, once the reader opened it.
    Open(SharedString),
    /// Its first and last lines around a count of the rest.
    Folded {
        head: SharedString,
        hidden: usize,
        tail: SharedString,
    },
}

impl Shown {
    fn of(text: &str, open: bool, limit: Bound) -> Self {
        let lines = bound(text, limit);
        if lines.hidden == 0 {
            return Self::Whole(text.trim_end().to_owned().into());
        }
        if open {
            return Self::Open(text.trim_end().to_owned().into());
        }
        Self::Folded {
            head: lines.head.join("\n").into(),
            hidden: lines.hidden,
            tail: lines.tail.join("\n").into(),
        }
    }
}

/// The call's output as this client holds it — the whole one once the
/// reader loaded it — and, while the record carried only a preview, the
/// control that loads the rest.
pub(in crate::chat) fn full_output<'a>(
    call: &Call<'a>,
    ctx: &mut ToolCtx<'_, '_>,
) -> (HeldOutput<'a>, Option<AnyElement>) {
    held_output(call.entry, call.output.unwrap_or_default(), ctx)
}

fn held_output<'a>(
    entry: &TimelineEntry,
    output: &'a str,
    ctx: &mut ToolCtx<'_, '_>,
) -> (HeldOutput<'a>, Option<AnyElement>) {
    let elided = ctx.elided(entry);
    let text = match &elided {
        Some(ElidedOutput::Loaded(full)) => HeldOutput::Loaded(full.clone()),
        _ => HeldOutput::Record(output),
    };
    let control = elided.and_then(|elided| elided_control(elided, &entry.id, &ctx.chat.motion));
    (text, control)
}

/// An output as this client holds it: the record's, or the whole one the
/// reader loaded.
pub(in crate::chat) enum HeldOutput<'a> {
    Record(&'a str),
    Loaded(SharedString),
}

impl Deref for HeldOutput<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Record(text) => text,
            Self::Loaded(text) => text,
        }
    }
}

/// The call's output as text: the whole fetched one when the reader loaded
/// it, JSON pretty-printed once the call is done (a half result is not
/// JSON), danger-tinted when the call failed.
pub(in crate::chat) fn output_section(
    call: &Call<'_>,
    limit: Bound,
    ctx: &mut ToolCtx<'_, '_>,
) -> Option<AnyElement> {
    output_section_with(call, limit, ctx, |text| text)
}

/// [`output_section`] over the part of the output `part` takes from it:
/// what a script printed, past its harness's report.
pub(in crate::chat) fn output_section_with(
    call: &Call<'_>,
    limit: Bound,
    ctx: &mut ToolCtx<'_, '_>,
    part: impl FnOnce(&str) -> &str,
) -> Option<AnyElement> {
    let (text, control) = held_output(call.entry, call.output.unwrap_or_default(), ctx);
    if call.output.is_none() && matches!(text, HeldOutput::Record(_)) {
        return None;
    }
    // A half result is not JSON.
    let done = call.status != ItemStatus::InProgress;
    let text = ctx.derived(
        format!("tool-output-{}-{done}", call.id()),
        part(&text),
        |text| {
            let pretty = done.then(|| pretty_json(text)).flatten();
            SharedString::from(pretty.unwrap_or_else(|| text.to_owned()))
        },
    );
    let failed = call.status == ItemStatus::Failed;
    let label = if failed {
        crate::tr!("tools.error")
    } else {
        crate::tr!("tools.output")
    };
    let body = if text.trim().is_empty() {
        div()
            .text_secondary(false, ctx.app())
            .child(crate::tr!("tools.no_output").into_owned())
            .into_any_element()
    } else {
        bounded_text(call.entry, "output", &text, true, limit, ctx)
    };
    let cx = ctx.app();
    let body = div()
        .w_full()
        .min_w_0()
        .when(failed, |body| {
            body.pl_2()
                .border_l_2()
                .border_color(cx.theme().danger.opacity(0.6))
                .text_color(cx.theme().danger)
        })
        .child(body);
    Some(
        window::section(
            &label,
            v_flex().w_full().gap_1().child(body).children(control),
            cx,
        )
        .into_any_element(),
    )
}

/// `text` as indented JSON, when it is a JSON object or array.
pub(in crate::chat) fn pretty_json(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with(['{', '[']) {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    serde_json::to_string_pretty(&value).ok()
}

/// Where loading a shortened output stands, as its control shows it.
#[derive(Clone, PartialEq)]
enum Elision {
    Preview(usize),
    Loading,
    Failed(String),
}

/// The control under a shortened output: the offer to load the rest, then
/// "loading", then a failure with the offer again; each cross-fades into the
/// next. It goes with the preview once the whole output is in, which grows
/// the body in the same frame.
fn elided_control(
    elided: ElidedOutput,
    id: &str,
    motion: &crate::motion::owned::Owned,
) -> Option<AnyElement> {
    let (current, on_load) = match elided {
        ElidedOutput::Preview {
            full_bytes,
            on_load,
        } => (Elision::Preview(full_bytes as usize), Some(on_load)),
        ElidedOutput::Loading => (Elision::Loading, None),
        ElidedOutput::Loaded(_) => return None,
        ElidedOutput::Failed { error, on_load } => (Elision::Failed(error), Some(on_load)),
    };
    let id = id.to_owned();
    let motion = motion.clone();
    Some(
        crate::motion::Sampled::new(move |window, cx| {
            let control_id = SharedString::from(format!("load-output-{id}"));
            let mut on_load = on_load;
            motion.cross_fade(
                format!("load-output-state-{id}"),
                current.clone(),
                CHANGE,
                window,
                cx,
                move |state, _, cx| {
                    // The control the current state offers loads; a leaving
                    // one is inert.
                    let on_load = if *state == current {
                        on_load.take()
                    } else {
                        None
                    };
                    let note = |text: String| {
                        div()
                            .text_secondary(false, cx)
                            .child(text)
                            .into_any_element()
                    };
                    let load = |on_load: Option<ClickHandler>, cx: &App| {
                        let selector = control_id.to_string();
                        div().debug_selector(move || selector.clone()).child(
                            super::components::changed_files::quiet_control(
                                control_id.clone(),
                                crate::tr!("chat.load_full_output").into_owned().into(),
                                on_load.unwrap_or_else(|| Box::new(|_, _, _| {})),
                                cx,
                            ),
                        )
                    };
                    let (text, offer) = match state {
                        Elision::Preview(size) => (
                            crate::tr!(
                                "chat.output_elided",
                                size = crate::thread_export::format_size(*size as u64)
                            )
                            .into_owned(),
                            true,
                        ),
                        Elision::Loading => {
                            return note(crate::tr!("chat.full_output_loading").into_owned());
                        }
                        Elision::Failed(error) => (
                            crate::tr!("chat.full_output_failed", error = error).into_owned(),
                            true,
                        ),
                    };
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(note(text))
                        .when(offer, |row| row.child(load(on_load, cx)))
                        .into_any_element()
                },
            )
        })
        .into_any_element(),
    )
}

/// The images a call was shown, as thumbnails that open the image viewer.
pub(in crate::chat) fn thumbnails(call: &Call<'_>, ctx: &ToolCtx<'_, '_>) -> Option<AnyElement> {
    thumbnails_titled(call, call.name.tool, ctx)
}

/// [`thumbnails`], each titled in the viewer by `title` and its place.
pub(in crate::chat) fn thumbnails_titled(
    call: &Call<'_>,
    title: &str,
    ctx: &ToolCtx<'_, '_>,
) -> Option<AnyElement> {
    if call.images.is_empty() || !ctx.images_inline {
        return None;
    }
    let session_id = ctx.session_id();
    let tiles: Vec<_> = (0..call.images.len())
        .map(|index| {
            let tile = super::components::images::Tile::of(
                call.entry,
                index,
                format!("{title} · {}", index + 1),
            );
            (tile.key(), tile)
        })
        .collect();
    let id = SharedString::from(format!("call-tiles-{}", call.id()));
    Some(super::components::images::sampled_tile_row(
        ctx.chat.workspace_store.read(ctx.cx).image_host(),
        id,
        tiles,
        session_id,
        &ctx.chat.motion,
    ))
}

/// Code a call ran, folded to its size until asked for, then highlighted
/// whole as `language` in a capped scroll.
pub(in crate::chat) fn code_section(
    call: &Call<'_>,
    code: &str,
    language: &str,
    ctx: &mut ToolCtx<'_, '_>,
) -> AnyElement {
    let key = format!("tool-code-{}", call.id());
    let open = ctx.is_open(&key);
    let on_click = ctx.toggle(call.entry, key.clone());
    let lines = code.lines().count().max(1);
    let label = if open {
        crate::tr!("tools.hide_code")
    } else {
        crate::tr!("tools.code_lines", count = lines)
    };
    let toggle = format!("{key}-toggle");
    let control = h_flex().child(
        div()
            .debug_selector({
                let toggle = toggle.clone();
                move || toggle.clone()
            })
            .child(super::components::changed_files::quiet_control(
                SharedString::from(toggle),
                label.into_owned().into(),
                on_click,
                ctx.app(),
            )),
    );
    let code_block = open.then(|| {
        let source = code.trim_end();
        let theme = ctx.highlight_theme();
        let highlighted = ctx.derived(key.clone(), source, |source| {
            crate::highlight::highlighted(
                source.to_owned().into(),
                SharedString::new(language),
                &theme,
            )
        });
        let cx = ctx.app();
        let theme = cx.theme();
        div()
            .id(SharedString::from(key.clone()))
            .debug_selector(move || key.clone())
            .w_full()
            .min_w_0()
            .max_h(px(OPEN_MAX_HEIGHT))
            .overflow_y_scroll()
            .font_family(theme.mono_font_family.clone())
            .text_token(&theme.tokens.typography.xs)
            .text_color(theme.foreground)
            .child(
                gpui::StyledText::new(highlighted.source().clone())
                    .with_highlights(highlighted.runs().cloned()),
            )
    });
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(control)
        .children(code_block)
        .into_any_element()
}

/// The output a body read a structured view from, as the call returned
/// it: folded until asked for, then bounded as any output is.
pub(in crate::chat) fn raw_section(
    call: &Call<'_>,
    text: &str,
    ctx: &mut ToolCtx<'_, '_>,
) -> AnyElement {
    let key = format!("tool-raw-{}", call.id());
    let open = ctx.is_open(&key);
    let on_click = ctx.toggle(call.entry, key.clone());
    let label = if open {
        crate::tr!("tools.hide_raw")
    } else {
        crate::tr!("tools.show_raw")
    };
    let toggle = format!("{key}-toggle");
    let control = h_flex().child(
        div()
            .debug_selector({
                let toggle = toggle.clone();
                move || toggle.clone()
            })
            .child(super::components::changed_files::quiet_control(
                SharedString::from(toggle),
                label.into_owned().into(),
                on_click,
                ctx.app(),
            )),
    );
    let raw = open.then(|| bounded_text(call.entry, "output", text, true, Bound::HEAD, ctx));
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(control)
        .children(raw)
        .into_any_element()
}

/// `path` as the workspace shows it.
pub(in crate::chat) fn display_path(path: &str, cwd: &Path) -> String {
    crate::workspace_walk::relativize_to_workspace(path, cwd)
}

/// The body sections stacked with the rail's spacing.
pub(in crate::chat) fn body(sections: impl IntoIterator<Item = AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .children(sections)
        .into_any_element()
}

#[cfg(test)]
pub(in crate::chat) mod tests;
