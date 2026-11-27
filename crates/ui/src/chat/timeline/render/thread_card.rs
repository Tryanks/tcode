//! The card an agent or a thread stands as in the flow of the thread that
//! started it: what it was asked to do, who runs it and how it stands. A
//! press opens it where its work is: an agent beside its lead, as its row
//! in the Agents panel does, a thread of its own in its own place.

use agent::{ItemStatus, ProviderKind};
use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::h_flex;
use tcode_core::session::TimelineEntry;
use tcode_core::settlement::AgentExecution;

use super::super::TimelineView;
use super::CHANGE;
use crate::chat::model::one_line;
use crate::chat::tools::{self, subagent::Started};
use crate::icon::{Icon, IconName};
use crate::material::event_card::{self, CardTone};
use crate::theme::ActiveTheme as _;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::widgets::{BreathingDot, Spinner};

/// How what the card stands for stands, in a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::chat) enum Standing {
    /// The call that starts the thread has not returned.
    Starting,
    Working,
    /// Waiting on a request inside it.
    Waiting,
    /// No turn runs, while work it started does.
    Background,
    Finished,
    Failed,
    Interrupted,
    Cancelled,
    /// The user did not let it run.
    Declined,
    /// A thread of its own: started, and none of this thread's to follow.
    Started,
    /// Running as last heard, on a machine out of this window's reach.
    Unreachable,
    /// Its machine left the group.
    MachineLeft,
}

impl Standing {
    fn of_item(status: ItemStatus) -> Self {
        match status {
            ItemStatus::InProgress => Self::Working,
            ItemStatus::Completed => Self::Finished,
            ItemStatus::Failed => Self::Failed,
            ItemStatus::Interrupted => Self::Interrupted,
            ItemStatus::Declined => Self::Declined,
        }
    }

    fn of_execution(execution: AgentExecution) -> Self {
        match execution {
            AgentExecution::Working => Self::Working,
            AgentExecution::Waiting => Self::Waiting,
            AgentExecution::Background => Self::Background,
            AgentExecution::Finished => Self::Finished,
            AgentExecution::Failed => Self::Failed,
            AgentExecution::Cancelled => Self::Cancelled,
            AgentExecution::Interrupted => Self::Interrupted,
        }
    }

    fn of_machine(standing: crate::machines::Standing) -> Self {
        match standing {
            crate::machines::Standing::Execution(execution) => Self::of_execution(execution),
            crate::machines::Standing::Unreachable => Self::Unreachable,
            crate::machines::Standing::MachineLeft => Self::MachineLeft,
        }
    }

    fn label(self) -> SharedString {
        let key = match self {
            Self::Starting => "tools.status.starting",
            Self::Working => "agents.state_working",
            Self::Waiting => "agents.state_waiting",
            Self::Background => "agents.state_background",
            Self::Finished => "agents.state_finished",
            Self::Failed => "agents.state_failed",
            Self::Interrupted => "agents.state_interrupted",
            Self::Cancelled => "agents.state_cancelled",
            Self::Declined => "tools.status.declined",
            Self::Started => "tools.status.started",
            Self::Unreachable => "machines.dispatch.unreachable",
            Self::MachineLeft => "machines.dispatch.machine_left",
        };
        crate::tr!(key).into_owned().into()
    }

    fn status(self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let icon = |name: IconName| Some(Icon::new(name).size(px(13.)).into_any_element());
        let (glyph, color) = match self {
            Self::Starting | Self::Working => (
                Some(Spinner::new().color(theme.primary).into_any_element()),
                theme.primary,
            ),
            Self::Background => (
                Some(BreathingDot::new(theme.primary).into_any_element()),
                theme.primary,
            ),
            Self::Waiting => (icon(IconName::CircleDot), theme.warning),
            Self::Finished => (icon(IconName::CircleCheck), theme.success),
            Self::Failed => (icon(IconName::CircleX), theme.danger),
            Self::Interrupted => (icon(IconName::CircleAlert), theme.warning),
            Self::Cancelled | Self::Declined => (icon(IconName::Ban), theme.muted_foreground),
            Self::Started => (None, theme.muted_foreground),
            // Out of reach is not failed: the thread may still be running.
            Self::Unreachable => (icon(IconName::Unlink), theme.muted_foreground),
            Self::MachineLeft => (icon(IconName::CircleDashed), theme.muted_foreground),
        };
        event_card::status(glyph, self.label(), color, cx).into_any_element()
    }
}

/// What a press on the card opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::chat) enum Opens {
    /// An agent, beside its lead.
    Agent(String),
    /// A thread of its own.
    Thread(String),
    /// A thread on another machine of the group, where that machine's
    /// store places it.
    Elsewhere { machine: String, thread: String },
}

/// The glyph the card's tile shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::chat) enum Mark {
    /// A subagent of the harness's own.
    Bot,
    /// The started thread's harness in its profile's colour, as the sidebar
    /// marks the thread.
    Profile(ProviderKind, Option<String>),
    /// The provider the call asked for, while the thread is not known here.
    Provider(ProviderKind),
}

/// A card as it reads.
#[derive(Debug, PartialEq)]
pub(in crate::chat) struct ThreadCard {
    pub(in crate::chat) mark: Mark,
    /// A bot hung off the tile: the thread reports back as an agent.
    pub(in crate::chat) badge: bool,
    /// The task it was given.
    pub(in crate::chat) title: String,
    /// Who runs it: the agent's type or the harness, and the model.
    pub(in crate::chat) subtitle: String,
    pub(in crate::chat) standing: Standing,
    /// The line a failed agent is told by.
    pub(in crate::chat) failure: Option<String>,
    /// The first message a thread of its own was started with.
    pub(in crate::chat) message: Option<String>,
    /// Muted lines on a thread elsewhere: what is still to be told its
    /// machine, and why it may not be heard from.
    pub(in crate::chat) notes: Vec<String>,
    pub(in crate::chat) opens: Option<Opens>,
    /// Why the card opens nothing, for a thread on a machine this window is
    /// not linked to.
    pub(in crate::chat) closed: Option<SharedString>,
    copy: Vec<(String, String)>,
}

/// The provider `list_providers` names `name`.
fn provider_named(name: &str) -> Option<ProviderKind> {
    ProviderKind::NATIVE
        .into_iter()
        .chain([ProviderKind::Acp])
        .find(|kind| tcode_core::settings::provider_key(*kind) == name.trim())
}

fn joined<'a>(parts: impl IntoIterator<Item = Option<&'a str>>) -> String {
    parts
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

impl TimelineView {
    /// The card `entry` stands as, `None` when it stands as none.
    pub(in crate::chat) fn thread_card(
        &self,
        entry: &TimelineEntry,
        cx: &App,
    ) -> Option<ThreadCard> {
        let mut copy = Vec::new();
        Some(match tools::subagent::card(entry)? {
            Started::Native {
                agent_type,
                description,
                status,
                summary,
                model,
                effort,
            } => {
                tools::copy(&mut copy, "tools.copy_task", description);
                if let Some(summary) = summary {
                    tools::copy(&mut copy, "tools.copy_summary", summary);
                }
                let title = Some(one_line(description))
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| agent_type.to_owned());
                ThreadCard {
                    mark: Mark::Bot,
                    badge: false,
                    title,
                    subtitle: joined([Some(agent_type), model, effort]),
                    standing: if self.runs_in_background(entry, status, cx) {
                        Standing::Background
                    } else {
                        Standing::of_item(status)
                    },
                    // A completed agent's summary is its report, which
                    // reaches the lead as a message of its own.
                    failure: summary
                        .filter(|_| status == ItemStatus::Failed)
                        .and_then(tools::window::first_line),
                    message: None,
                    notes: Vec::new(),
                    opens: tools::subagent::child_thread(self, entry, cx).map(Opens::Agent),
                    closed: None,
                    copy,
                }
            }
            Started::Thread(start) if start.machine.is_some() => self.machine_card(start, cx)?,
            Started::Thread(start) => {
                let store = self.workspace_store.read(cx);
                let meta = start
                    .thread_id
                    .as_deref()
                    .and_then(|id| store.thread_meta(id));
                let requested = start.provider.and_then(provider_named);
                let mark = match (meta, requested) {
                    (Some(meta), _) => Mark::Profile(meta.provider, meta.profile_id.clone()),
                    (None, Some(provider)) => Mark::Provider(provider),
                    (None, None) => Mark::Bot,
                };
                let harness = match (meta, requested) {
                    (Some(meta), _) => Some(store.harness_name_of(meta)),
                    (None, provider) => provider.map(|provider| provider.display_name().into()),
                };
                let model = match meta.and_then(|meta| Some((meta, meta.model.as_deref()?))) {
                    Some((meta, model)) => Some(
                        store
                            .thread_model_name(&meta.id, model)
                            .unwrap_or_else(|| model.to_owned()),
                    ),
                    None => start.model.map(str::to_owned),
                };
                let title = meta
                    .map(|meta| one_line(&meta.title))
                    .filter(|title| !title.is_empty())
                    .or(start.title.clone())
                    .or_else(|| start.message.map(one_line))
                    .unwrap_or_default();
                let standing = match (&start.thread_id, start.subagent) {
                    _ if start.running => Standing::Starting,
                    (_, false) => Standing::Started,
                    (Some(id), true) => match store.agent_status(id) {
                        Some(status) => Standing::of_execution(status.execution),
                        // Not reported: settled, archived, or not yet heard of.
                        None => match meta {
                            Some(meta)
                                if meta.report() == tcode_core::settlement::AgentReport::Failed =>
                            {
                                Standing::Failed
                            }
                            Some(meta) if meta.cancelled_at.is_some() => Standing::Cancelled,
                            Some(meta) if !meta.is_settled() && meta.archived_at.is_none() => {
                                Standing::Working
                            }
                            _ => Standing::Finished,
                        },
                    },
                    (None, true) => Standing::Finished,
                };
                if let Some(message) = start.message {
                    tools::copy(&mut copy, "chat.copy_text", message);
                }
                tools::copy(
                    &mut copy,
                    "chat.copy_output",
                    start.output.unwrap_or_default(),
                );
                if let Some(id) = &start.thread_id {
                    let link = crate::markdown::OpenThreadAnchor {
                        thread_id: id.clone(),
                        position: None,
                    };
                    tools::copy(&mut copy, "tools.copy_link", &link.url());
                }
                ThreadCard {
                    // The bot alone already says agent.
                    badge: start.subagent && mark != Mark::Bot,
                    mark,
                    title,
                    subtitle: joined([
                        harness.as_deref(),
                        model.as_deref(),
                        start.branch.as_deref(),
                    ]),
                    standing,
                    failure: None,
                    message: start.message.filter(|_| !start.subagent).map(one_line),
                    notes: Vec::new(),
                    opens: start.thread_id.clone().map(if start.subagent {
                        Opens::Agent
                    } else {
                        Opens::Thread
                    }),
                    closed: None,
                    copy,
                }
            }
        })
    }

    /// The card of a thread started on another machine of the group: that
    /// machine's thread, read from that machine's store while this window
    /// is linked to it, and otherwise from what this thread's host keeps of
    /// it.
    fn machine_card(&self, start: tools::threads::Start<'_>, cx: &App) -> Option<ThreadCard> {
        let mut copy = Vec::new();
        let machine = crate::machines::machine(start.machine?, cx);
        let there = machine.store.as_ref().map(|store| store.read(cx));
        let meta = there.and_then(|store| store.thread_meta(start.thread_id.as_deref()?));
        let requested = start.provider.and_then(provider_named);
        let mark = match (meta, requested) {
            (Some(meta), _) => Mark::Profile(meta.provider, meta.profile_id.clone()),
            (None, Some(provider)) => Mark::Provider(provider),
            (None, None) => Mark::Bot,
        };
        let harness = match (there.zip(meta), requested) {
            (Some((store, meta)), _) => Some(store.harness_name_of(meta)),
            (None, provider) => provider.map(|provider| provider.display_name().into()),
        };
        let model = match there.zip(meta.and_then(|meta| Some((meta, meta.model.as_deref()?)))) {
            Some((store, (meta, model))) => Some(
                store
                    .thread_model_name(&meta.id, model)
                    .unwrap_or_else(|| model.to_owned()),
            ),
            None => start.model.map(str::to_owned),
        };
        let title = meta
            .map(|meta| one_line(&meta.title))
            .filter(|title| !title.is_empty())
            .or(start.title.clone())
            .or_else(|| start.message.map(one_line))
            .unwrap_or_default();
        // What this thread's host keeps of the thread it started there.
        let record = start.thread_id.as_deref().and_then(|thread| {
            let lead = self.session_id.as_deref()?;
            self.workspace_store
                .read(cx)
                .thread_meta(lead)?
                .remote_children
                .iter()
                .find(|child| child.machine == machine.id && child.thread_id == thread)
                .cloned()
        });
        let (standing, notes) = match (&start.thread_id, start.subagent) {
            _ if start.running => (Standing::Starting, Vec::new()),
            (_, false) => (Standing::Started, Vec::new()),
            (Some(_), true) => match &record {
                Some(child) => {
                    let standing = crate::machines::standing(child, &machine, cx);
                    (
                        Standing::of_machine(standing),
                        crate::machines::notes(child, standing, &machine, cx),
                    )
                }
                // Answered, and not yet kept by the host.
                None => (Standing::Working, Vec::new()),
            },
            (None, true) => (Standing::Finished, Vec::new()),
        };
        if let Some(message) = start.message {
            tools::copy(&mut copy, "chat.copy_text", message);
        }
        tools::copy(
            &mut copy,
            "chat.copy_output",
            start.output.unwrap_or_default(),
        );
        if let Some(link) = &start.link {
            tools::copy(&mut copy, "tools.copy_link", link);
        }
        let open = machine.store.is_some();
        Some(ThreadCard {
            badge: start.subagent && mark != Mark::Bot,
            mark,
            title,
            subtitle: joined([
                Some(machine.name.as_str()),
                harness.as_deref(),
                model.as_deref(),
                start.branch.as_deref(),
            ]),
            standing,
            failure: None,
            message: start.message.filter(|_| !start.subagent).map(one_line),
            notes,
            opens: start
                .thread_id
                .clone()
                .filter(|_| open)
                .map(|thread| Opens::Elsewhere {
                    machine: machine.id.clone(),
                    thread,
                }),
            closed: (!open && start.thread_id.is_some())
                .then(|| crate::machines::not_open(&machine.name)),
            copy,
        })
    }

    /// `entry`'s card: a header of its tile, its task over who runs it, and
    /// how it stands; under it the line a failed agent is told by, or what
    /// a thread of its own was asked.
    pub(super) fn compose_thread_card(
        &mut self,
        entry: &TimelineEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(card) = self.thread_card(entry, cx) else {
            return div().into_any_element();
        };
        let id = entry.id.clone();
        let radius = px(event_card::RADIUS);
        let icon = match &card.mark {
            Mark::Bot => Icon::new(IconName::Bot),
            Mark::Profile(provider, profile) => crate::provider_card::profile_glyph(
                self.workspace_store.read(cx),
                *provider,
                profile.as_deref(),
            ),
            Mark::Provider(provider) => crate::provider_card::provider_glyph(*provider),
        };
        let badge = card.badge.then(|| Icon::new(IconName::Bot));
        let tile = match card.mark {
            Mark::Bot => event_card::tile(icon, badge, cx),
            Mark::Profile(..) | Mark::Provider(_) => event_card::glyph_tile(icon, badge, cx),
        };
        let native = matches!(
            &entry.content,
            tcode_core::session::EntryContent::Item(agent::ItemContent::Subagent { .. })
        );
        // The harness's own controls over an agent it runs in the
        // background, as over a command it runs there.
        let action = match card.standing {
            Standing::Background if native => Some(self.background_link(&id, cx)),
            Standing::Working if native && self.moves_to_background(cx) => {
                Some(self.move_to_background(&id))
            }
            _ => None,
        };
        let status = self.motion.cross_fade_part(
            format!("thread-card-status-{id}"),
            card.standing,
            CHANGE,
            window,
            cx,
            |standing, _, cx| standing.status(cx),
        );
        let label = format!("{}, {}", card.title, card.standing.label());
        let header = event_card::header(false, cx)
            .child(tile)
            .child(event_card::header_text(
                card.title,
                Some(SharedString::from(card.subtitle)).filter(|subtitle| !subtitle.is_empty()),
                cx,
            ))
            .children(action)
            .child(
                div()
                    .flex_none()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("thread-card-status-{id}")
                    })
                    .child(status),
            );
        let theme = cx.theme();
        let failure = card.failure.map(|line| {
            let id = id.clone();
            event_card::section(cx).child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.danger)
                    .debug_selector(move || format!("thread-card-failure-{id}"))
                    .child(crate::widgets::fade_text(line)),
            )
        });
        let message = card.message.map(|message| {
            let id = id.clone();
            event_card::section(cx).child(
                div()
                    .min_w_0()
                    .line_clamp(2)
                    .text_color(theme.muted_foreground)
                    .debug_selector(move || format!("thread-card-message-{id}"))
                    .child(message),
            )
        });
        let notes = (!card.notes.is_empty()).then(|| {
            let id = id.clone();
            event_card::section(cx).child(
                div()
                    .min_w_0()
                    .text_color(theme.muted_foreground)
                    .debug_selector(move || format!("thread-card-notes-{id}"))
                    .children(card.notes.iter().map(|note| div().child(note.clone()))),
            )
        });
        let tone = if card.standing == Standing::Failed {
            CardTone::Danger
        } else {
            CardTone::Neutral
        };
        let selector = {
            let id = id.clone();
            move || format!("thread-card-{id}")
        };
        let surface = event_card::event_card(tone, cx)
            .relative()
            .when(card.opens.is_some(), |surface| {
                surface.child(
                    self.hover
                        .layer(format!("thread-card-{id}"), theme.accent, radius),
                )
            })
            .child(header)
            .children(failure)
            .children(message)
            .children(notes);
        let origin = SharedString::from(format!("thread-card-{id}"));
        let surface = match card.opens.clone() {
            Some(opens) => {
                let store = self.workspace_store.clone();
                let host = self.agent_host(cx);
                let origin = origin.clone();
                crate::material::accessible_clickable(
                    surface,
                    SharedString::from(format!("thread-card-{id}")),
                    Role::Button,
                    label,
                    cx,
                )
                .debug_selector(selector)
                .with_tooltip(|_, _| {
                    crate::widgets::tooltip::Tooltip::new(
                        crate::tr!("tools.open_thread").into_owned(),
                    )
                })
                .rounded(radius)
                .cursor_pointer()
                .on_click(move |_, window, cx| match &opens {
                    Opens::Agent(child) => crate::agents_panel::open_agent(
                        &store,
                        child.clone(),
                        host.clone(),
                        Some(origin.clone()),
                        window,
                        cx,
                    ),
                    Opens::Thread(thread) => window.dispatch_action(
                        Box::new(crate::markdown::OpenThreadAnchor {
                            thread_id: thread.clone(),
                            position: None,
                        }),
                        cx,
                    ),
                    Opens::Elsewhere { machine, thread } => {
                        crate::machines::open_thread(machine, thread, None, cx);
                    }
                })
                .into_any_element()
            }
            None => match card.closed.clone() {
                Some(closed) => surface
                    .id(SharedString::from(format!("thread-card-{id}")))
                    .debug_selector(selector)
                    .with_tooltip(move |_, _| {
                        crate::widgets::tooltip::Tooltip::new(closed.to_string())
                    })
                    .into_any_element(),
                None => surface.debug_selector(selector).into_any_element(),
            },
        };
        let copy = card.copy;
        let surface = h_flex()
            .id(SharedString::from(format!("thread-card-menu-{id}")))
            .w_full()
            .child(surface)
            .context_menu(move |menu, _, _| {
                copy.iter().cloned().fold(menu, |menu, (label, text)| {
                    menu.menu(label, Box::new(CopyText(text)))
                })
            })
            .into_any_element();
        let surface = self.ringed_when_located(&id, radius, surface, window, cx);
        match card.opens {
            // What the agent's card beside its lead grows out of.
            Some(Opens::Agent(_)) => self
                .side_origins
                .mark(origin, radius, surface)
                .into_any_element(),
            _ => surface,
        }
    }
}
