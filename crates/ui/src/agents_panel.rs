//! The Agents view: a lead thread's dispatched agents and the provider-native
//! subagents of its transcript, in the desktop right panel and the phone sheet.
//! It shows what the host reports; delivery is never decided here.

use crate::widgets::TooltipTrigger as _;
use std::cell::RefCell;
use std::rc::Rc;

use agent::{ItemContent, ItemStatus, ProviderKind};
use gpui::{
    Action, AnyElement, App, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use tcode_core::notice::HostNotice;
use tcode_core::session::{EntryContent, LatestActivity};
use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};

use crate::chat::ChatView;
use crate::icon::{Icon, IconName};
use crate::material;
use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::menu::DropdownMenu as _;
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::Tooltip;
use gpui_base::PopoverState;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentOpen(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentStop(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentSettle(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentCancel(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentStopBackground(String);

#[derive(Clone)]
struct AgentRow {
    key: String,
    title: String,
    provider: Option<ProviderKind>,
    /// The profile it runs on; absent on its provider's built-in one.
    profile_id: Option<String>,
    /// The model it runs on, and the reasoning effort it runs with, as
    /// reported.
    model: Option<String>,
    effort: Option<String>,
    /// Absent for a settled or archived agent the host reports no activity
    /// for.
    execution: Option<AgentExecution>,
    /// What it waits on the user for, when the host reports it waiting.
    attention: Option<Attention>,
    /// The branch it works on, when that is not its lead's.
    branch: Option<String>,
    /// Absent for a provider-native subagent.
    delivery: Option<AgentDelivery>,
    /// Whether the child's `report_result` reached its lead.
    report: AgentReport,
    archived: bool,
    /// What it is doing, while it runs and its host reports that.
    activity: Option<LatestActivity>,
    preview: Option<String>,
    /// The thread Open selects: the dispatched agent, or a native mirror.
    open: Option<String>,
    /// The dispatched agent that Stop, Settle and Cancel act on.
    agent: Option<String>,
    /// The piece of its lead's background work a native subagent runs as,
    /// while its harness lists it and stops it on its own, and no stop for
    /// it is out.
    stop_task: Option<String>,
    /// The provider that manages a native subagent.
    managed_by: Option<&'static str>,
    /// A native subagent's type, which its provider names it by.
    agent_type: Option<String>,
    /// When the latest run started and, once it has, ended (unix ms).
    started_at: Option<u64>,
    ended_at: Option<u64>,
}

impl AgentRow {
    /// "provider · model · effort"; a native row leaves the provider to the
    /// caption its list sits under.
    fn detail(&self) -> String {
        [
            self.provider
                .filter(|_| self.managed_by.is_none())
                .map(|provider| provider.display_name()),
            self.model.as_deref(),
            self.effort.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    }

    /// Its result, once what it is doing no longer takes that line.
    fn result(&self) -> Option<String> {
        self.preview.clone().filter(|_| self.activity.is_none())
    }

    /// What it waits on the user for, or else how its run stands.
    fn state(&self) -> Option<AgentState> {
        match self.attention {
            Some(attention) => Some(AgentState::Attention(attention)),
            None => self.execution.map(AgentState::Execution),
        }
    }

    /// How long its last run took, once it no longer runs.
    fn elapsed(&self) -> Option<String> {
        if self.execution.is_some_and(AgentExecution::running) {
            return None;
        }
        let started = self.started_at?;
        Some(crate::chat::format_elapsed_seconds(
            self.run_ended()?.saturating_sub(started),
        ))
    }

    /// Whether its elapsed time counts up with the clock: it runs, and its
    /// run started after the last one ended.
    fn clock_runs(&self) -> bool {
        self.execution.is_some_and(AgentExecution::running)
            && self.started_at.is_some()
            && self.run_ended().is_none()
    }

    /// When its last run ended. A start no later than the last end belongs to
    /// a finished run: a running agent whose next run has not started yet
    /// has no elapsed time.
    fn run_ended(&self) -> Option<u64> {
        let started = self.started_at?;
        self.ended_at.filter(|ended| *ended >= started)
    }
}

/// A lead thread's agents in the Agents view's groups, newest first.
#[derive(Default)]
pub(crate) struct Agents {
    needs_settle: Vec<AgentRow>,
    running: Vec<AgentRow>,
    native: Vec<AgentRow>,
    /// Dispatched agents whose result will not be delivered, not settled.
    done: Vec<AgentRow>,
    /// Agents whose thread is settled, dispatched or a native subagent's
    /// mirror: shown only on the Agents view's Settled shelf.
    settled: Vec<AgentRow>,
}

impl Agents {
    /// The thread's agents as the host reports them. Results are left out:
    /// only the lists that show them read them, see [`Self::with_results`].
    pub(crate) fn of(store: &WorkspaceStore, parent_id: &str, cx: &App) -> Self {
        let live = store.sidebar_sessions();
        let parent = store.thread_meta(parent_id);
        let attention = |id: Option<&str>| {
            let id = id?;
            if store.pending_approval_for(id) {
                Some(Attention::Approval)
            } else if store.pending_user_input_for(id) {
                Some(Attention::Answer)
            } else {
                None
            }
        };
        let lead_branch = store
            .session_status_for(Some(parent_id), cx)
            .and_then(|status| status.git_branch.clone())
            .or_else(|| {
                parent
                    .and_then(|parent| parent.worktree.as_ref())
                    .map(|worktree| worktree.branch.clone())
            });
        let running_activity = |id: Option<&str>, execution: AgentExecution| {
            store.latest_activity(id?).filter(|_| execution.running())
        };
        let status = store.session_status_for(Some(parent_id), cx);
        let stops = status
            .and_then(|status| status.provider.caps().background)
            .is_some_and(|control| control.stop.stops());
        let stop_task = |item: &str| {
            status?
                .background_tasks
                .iter()
                .find(|row| {
                    stops
                        && row.listed
                        && !row.ended
                        && row.stopping_since.is_none()
                        && row.task.item_id.as_deref() == Some(item)
                })
                .map(|row| row.task.id.clone())
        };
        let mut native = Vec::new();
        let mut settled_native = Vec::new();
        store.with_entries_for(
            Some(parent_id),
            |timeline| {
                for entry in &timeline.entries {
                    let EntryContent::Item(ItemContent::Subagent {
                        agent_type,
                        description,
                        status,
                        summary,
                        model,
                        effort,
                    }) = &entry.content
                    else {
                        continue;
                    };
                    let mirror = live.iter().find(|meta| {
                        meta.parent_session_id.as_deref() == Some(parent_id)
                            && meta.native_subagent.as_deref() == Some(entry.id.as_str())
                    });
                    let provider = parent.map(|parent| parent.provider);
                    let execution = match status {
                        ItemStatus::InProgress if entry.background => AgentExecution::Background,
                        ItemStatus::InProgress => AgentExecution::Working,
                        ItemStatus::Completed => AgentExecution::Finished,
                        ItemStatus::Interrupted => AgentExecution::Interrupted,
                        ItemStatus::Failed | ItemStatus::Declined => AgentExecution::Failed,
                    };
                    let group = if mirror.is_some_and(|meta| meta.is_settled()) {
                        &mut settled_native
                    } else {
                        &mut native
                    };
                    group.push(AgentRow {
                        key: entry.id.clone(),
                        title: Some(compact(description))
                            .filter(|title| !title.is_empty())
                            .unwrap_or_else(|| agent_type.clone()),
                        provider,
                        profile_id: parent.and_then(|parent| parent.profile_id.clone()),
                        model: model.clone(),
                        effort: effort.clone(),
                        execution: Some(execution),
                        attention: attention(mirror.map(|meta| meta.id.as_str())),
                        // It runs in its lead's process, on its lead's checkout.
                        branch: None,
                        delivery: None,
                        report: AgentReport::None,
                        archived: false,
                        activity: running_activity(mirror.map(|meta| meta.id.as_str()), execution),
                        preview: summary.as_deref().map(compact).filter(|s| !s.is_empty()),
                        open: mirror.map(|meta| meta.id.clone()),
                        agent: None,
                        stop_task: execution.running().then(|| stop_task(&entry.id)).flatten(),
                        managed_by: provider.map(|provider| provider.display_name()),
                        agent_type: Some(agent_type.clone()),
                        // A native subagent's end is not reported, only its start.
                        started_at: entry.ts.filter(|_| execution.running()),
                        ended_at: None,
                    });
                }
            },
            cx,
        );
        native.reverse();
        settled_native.reverse();

        let mut dispatched: Vec<_> = live
            .iter()
            .chain(store.archived_sessions())
            .filter(|meta| {
                meta.is_dispatched() && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .collect();
        dispatched.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        let mut agents = Self {
            native,
            ..Self::default()
        };
        for meta in dispatched {
            let archived = meta.archived_at.is_some();
            // An archived agent has no reported activity; only its
            // settlement is known.
            let status = (!archived).then(|| store.agent_status(&meta.id)).flatten();
            let (execution, delivery) = match status {
                Some(status) => (Some(status.execution), status.delivery),
                // Nor does a settled one. An unsettled one is listed with
                // the running agents, its state unknown until the host
                // reports it.
                None => (None, AgentDelivery::of(meta, AgentExecution::Working)),
            };
            let row = AgentRow {
                key: meta.id.clone(),
                title: meta.title.clone(),
                provider: Some(meta.provider),
                profile_id: meta.profile_id.clone(),
                model: meta.model.clone(),
                effort: None,
                execution,
                attention: (!archived)
                    .then(|| attention(Some(meta.id.as_str())))
                    .flatten(),
                branch: meta
                    .worktree
                    .as_ref()
                    .map(|worktree| worktree.branch.clone())
                    .filter(|branch| lead_branch.as_ref() != Some(branch)),
                delivery: Some(delivery),
                report: status.map_or_else(|| meta.report(), |status| status.report),
                archived,
                activity: status
                    .and_then(|status| running_activity(Some(meta.id.as_str()), status.execution)),
                preview: None,
                open: Some(meta.id.clone()),
                agent: Some(meta.id.clone()),
                stop_task: None,
                managed_by: None,
                agent_type: None,
                started_at: status.and_then(|status| status.run_started_at),
                ended_at: status.and_then(|status| status.run_completed_at),
            };
            match delivery {
                AgentDelivery::AwaitingSettle => agents.needs_settle.push(row),
                AgentDelivery::Running => agents.running.push(row),
                AgentDelivery::NotDelivered => agents.done.push(row),
                AgentDelivery::Settled => agents.settled.push(row),
            }
        }
        agents.settled.extend(settled_native);
        agents
    }

    /// Add each dispatched agent's result: the report its latest callback
    /// delivered to the lead, as the child wrote it.
    pub(crate) fn with_results(
        mut self,
        store: &WorkspaceStore,
        parent_id: &str,
        cx: &App,
    ) -> Self {
        let mut results = std::collections::HashMap::new();
        store.with_entries_for(
            Some(parent_id),
            |timeline| {
                for entry in &timeline.entries {
                    let Some(HostNotice::Orchestrate(callbacks)) =
                        crate::chat::model::user_notice(&entry.content)
                    else {
                        continue;
                    };
                    for callback in callbacks {
                        let report = compact(callback.report().unwrap_or_default());
                        if !report.is_empty() {
                            results.insert(callback.child_id, report);
                        }
                    }
                }
            },
            cx,
        );
        for row in self
            .needs_settle
            .iter_mut()
            .chain(&mut self.done)
            .chain(&mut self.settled)
        {
            row.preview = results.remove(&row.key);
        }
        self
    }

    fn any_running(&self) -> bool {
        !self.running.is_empty()
            || self
                .native
                .iter()
                .any(|row| row.execution.is_some_and(AgentExecution::running))
    }

    /// Whether the lead has no agents at all, settled ones included.
    pub(crate) fn is_empty(&self) -> bool {
        self.count() == 0 && self.settled.is_empty()
    }

    /// How many agents are not settled: those the rail lists.
    pub(crate) fn count(&self) -> usize {
        self.needs_settle.len() + self.running.len() + self.native.len() + self.done.len()
    }

    /// Dispatched agents still running and finished ones awaiting settle.
    pub(crate) fn outstanding(&self) -> usize {
        self.running.len() + self.needs_settle.len()
    }

    /// The cue on the panel toggle and the phone pill: running work first,
    /// then results awaiting settle.
    pub(crate) fn cue(&self, cx: &App) -> Option<Hsla> {
        if self.any_running() {
            Some(cx.theme().primary)
        } else if !self.needs_settle.is_empty() {
            Some(cx.theme().warning)
        } else {
            None
        }
    }

    /// Every agent not settled as one line each, in the panel's order: those
    /// awaiting settle, the running ones, those not delivered, then the
    /// provider's own. A finished agent whose result awaits settle says that
    /// in place of how its run ended.
    pub(crate) fn lines(&self) -> Vec<AgentLine> {
        self.needs_settle
            .iter()
            .chain(&self.running)
            .chain(&self.done)
            .chain(&self.native)
            .map(|row| {
                let running = row.execution.is_some_and(AgentExecution::running);
                let state = match row.state() {
                    Some(AgentState::Execution(execution))
                        if !execution.running()
                            && row.delivery == Some(AgentDelivery::AwaitingSettle) =>
                    {
                        Some(AgentState::AwaitingSettle)
                    }
                    state => state,
                };
                AgentLine {
                    key: row.key.clone(),
                    title: row.title.clone(),
                    execution: row.execution,
                    state,
                    model: row.model.clone(),
                    effort: row.effort.clone(),
                    branch: row.branch.clone(),
                    activity: row.activity.clone(),
                    preview: row.result(),
                    running_since: row.started_at.filter(|started| {
                        running && !row.ended_at.is_some_and(|ended| ended >= *started)
                    }),
                    elapsed: row.elapsed(),
                    open: row.open.clone(),
                }
            })
            .collect()
    }

    /// What the lead's agents wait on the user for, then the running work
    /// and the results awaiting settle.
    pub(crate) fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        let waiting = |attention| {
            self.needs_settle
                .iter()
                .chain(&self.running)
                .chain(&self.native)
                .chain(&self.done)
                .filter(|row| row.attention == Some(attention))
                .count()
        };
        match waiting(Attention::Approval) {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_approval_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_approval", count = count).into_owned()),
        }
        match waiting(Attention::Answer) {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_answer_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_answer", count = count).into_owned()),
        }
        match self.running.len() {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_running_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_running", count = count).into_owned()),
        }
        match self.needs_settle.len() {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_unsettled_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_unsettled", count = count).into_owned()),
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// What the agent whose thread is `open` goes by: its row's title, or
    /// its thread's.
    pub(crate) fn title(&self, store: &WorkspaceStore, open: &str) -> String {
        self.heading(open)
            .map(|heading| heading.title)
            .or_else(|| store.thread_meta(open).map(|meta| meta.title.clone()))
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| crate::tr!("agents.untitled").into_owned())
    }

    /// The row of the agent whose thread is `open`, as its lead's Agents
    /// view shows it.
    pub(crate) fn heading(&self, open: &str) -> Option<AgentHeading> {
        self.needs_settle
            .iter()
            .chain(&self.running)
            .chain(&self.native)
            .chain(&self.done)
            .chain(&self.settled)
            .find(|row| row.open.as_deref() == Some(open))
            .map(|row| AgentHeading {
                key: row.key.clone(),
                title: row.title.clone(),
                execution: row.execution,
            })
    }
}

/// What an agent's row says of it, for a surface that heads its thread.
pub(crate) struct AgentHeading {
    /// The row's key, which its status mark's motion is kept under.
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) execution: Option<AgentExecution>,
}

/// The line that says what an agent is doing, folding in under its row (each
/// `surface` keeps its own) and cross-fading to what it does next.
pub(crate) fn activity_line(
    surface: &str,
    key: &str,
    activity: Option<LatestActivity>,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let id = format!("{surface}-activity-{key}");
    crate::motion::shown(SharedString::from(id.clone()), activity, SHOW, window, cx).map(|shown| {
        let text = crate::motion::cross_fade_part(
            SharedString::from(format!("{surface}-activity-text-{key}")),
            shown.value.clone(),
            CHANGE,
            window,
            cx,
            |activity: &LatestActivity, _, _| {
                div()
                    .w_full()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(activity_label(activity)))
                    .into_any_element()
            },
        );
        shown.fold(
            div()
                .debug_selector(move || id)
                .w_full()
                .pt_0p5()
                .text_secondary(false, cx)
                .text_color(cx.theme().muted_foreground)
                .child(text)
                .into_any_element(),
        )
    })
}

fn activity_label(activity: &LatestActivity) -> String {
    let label = match activity {
        LatestActivity::Thinking => crate::tr!("agents.activity_thinking"),
        LatestActivity::Writing => crate::tr!("agents.activity_writing"),
        LatestActivity::Command { text } => {
            crate::tr!("agents.activity_command", command = text.as_str())
        }
        LatestActivity::Edit { path } => crate::tr!(
            "agents.activity_edit",
            path = path.as_deref().unwrap_or_default()
        ),
        LatestActivity::Read { path } => crate::tr!("agents.activity_read", path = path.as_str()),
        LatestActivity::WebSearch { query } => {
            crate::tr!("agents.activity_search", query = query.as_str())
        }
        LatestActivity::Tool { name, target } => {
            let tool = crate::chat::tools::subject(agent::ToolName::parse(name));
            match target {
                Some(target) => crate::tr!(
                    "agents.activity_tool_target",
                    tool = tool.as_str(),
                    target = target.as_str()
                ),
                None => crate::tr!("agents.activity_tool", tool = tool.as_str()),
            }
        }
        LatestActivity::Subagent { description } => crate::tr!(
            "agents.activity_subagent",
            description = description.as_str()
        ),
        LatestActivity::Other { kind, summary } => {
            return match summary {
                Some(summary) => format!("{kind} · {summary}"),
                None => kind.clone(),
            };
        }
    };
    label.trim_end().to_owned()
}

/// What an agent waits on the user for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attention {
    Approval,
    Answer,
}

/// The word an agent's row says of where it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentState {
    Attention(Attention),
    /// Finished, and its result not yet settled into its lead.
    AwaitingSettle,
    Execution(AgentExecution),
}

impl AgentState {
    pub(crate) fn label(self) -> SharedString {
        match self {
            Self::Attention(Attention::Approval) => {
                crate::tr!("mobile.approval").into_owned().into()
            }
            Self::Attention(Attention::Answer) => crate::tr!("mobile.answer").into_owned().into(),
            Self::AwaitingSettle => crate::tr!("agents.delivery_awaiting").into_owned().into(),
            Self::Execution(execution) => execution_label(execution),
        }
    }

    pub(crate) fn color(self, cx: &App) -> Hsla {
        match self {
            Self::Attention(Attention::Approval) | Self::AwaitingSettle => cx.theme().warning,
            Self::Attention(Attention::Answer) => cx.theme().primary,
            Self::Execution(execution) => execution_color(execution, cx),
        }
    }
}

/// One of a lead's agents, as a list beside its thread names it.
#[derive(Clone, PartialEq)]
pub(crate) struct AgentLine {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) execution: Option<AgentExecution>,
    /// Where it stands, in a word.
    pub(crate) state: Option<AgentState>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    /// The branch it works on, when that is not its lead's.
    pub(crate) branch: Option<String>,
    /// What it is doing, while it runs.
    pub(crate) activity: Option<LatestActivity>,
    /// Its result, once it has one, on one line.
    pub(crate) preview: Option<String>,
    /// When the run under way started, while it runs.
    pub(crate) running_since: Option<u64>,
    /// How long its last run took, once it has finished.
    pub(crate) elapsed: Option<String>,
    /// The thread that shows it.
    pub(crate) open: Option<String>,
}

/// The key the panel's row opening `id` has among the lead's origins.
fn panel_origin(id: String) -> SharedString {
    format!("agents-panel-{id}").into()
}

fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Open a thread reached from its lead: on the desktop, beside the thread
/// `host` shows, in `host` itself, that thread's right panel shut; as a page
/// over the lead in the compact layout, which has no host, no room beside
/// it, and goes back with its navigation. A thread a pane already shows
/// takes the keyboard there instead. An archived agent is reached the way
/// Settings → Archived Threads reaches every archived thread: unarchived.
/// Beside the lead, it comes out of `origin`, what was pressed to open it,
/// as the host records it ([`ChatView::side_origins`]).
///
/// `host` is the chat that drew what was pressed, not one found by the
/// lead's id: a thread can show in one pane and beside its own lead in
/// another, and the agent belongs in the pane the press came from.
pub(crate) fn open_agent(
    store: &Entity<WorkspaceStore>,
    id: String,
    host: Option<Entity<ChatView>>,
    origin: Option<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    store.update(cx, |store, _| {
        if store
            .archived_sessions()
            .iter()
            .any(|meta| meta.id == id && meta.archived_at.is_some())
        {
            store.unarchive_session(id.clone());
        }
    });
    let thread = store.read(cx).thread_key(&id);
    let host = host.filter(|_| !crate::shell::shown_in_pane(&thread, cx));
    match host {
        Some(host) => {
            // A right panel covering the host's pane lies over the chat,
            // where the agent would show beneath it. One docked beside the
            // chat stays: the agent is in the chat's body.
            if host.read(cx).panel_lies_over(cx)
                && let Some(lead) = host.read(cx).session_id().map(str::to_owned)
            {
                store.update(cx, |store, cx| store.close_right_panel_for(Some(&lead), cx));
            }
            host.update(cx, |chat, cx| chat.open_side(id, origin, window, cx));
        }
        None => crate::shell::open_agent_page(store, id, window, cx),
    }
}

/// Go back from `agent`, opened in its lead's place, to the lead, in the
/// pane it was drawn in, wherever the keyboard is.
pub(crate) fn back_to_lead(
    store: &Entity<WorkspaceStore>,
    lead: String,
    agent: Option<String>,
    cx: &mut App,
) {
    store.update(cx, |store, cx| store.show_thread_replacing(lead, agent, cx));
}

#[derive(Clone, Copy)]
enum BadgeStyle {
    Plain,
    Outline,
    Warning,
    Danger,
}

pub(crate) fn execution_color(execution: AgentExecution, cx: &App) -> Hsla {
    match execution {
        // Running in the background is running: it reads as Working does.
        AgentExecution::Working | AgentExecution::Background => cx.theme().primary,
        AgentExecution::Waiting => cx.theme().warning,
        AgentExecution::Finished => cx.theme().success,
        AgentExecution::Failed => cx.theme().danger,
        AgentExecution::Cancelled | AgentExecution::Interrupted => cx.theme().muted_foreground,
    }
}

pub(crate) fn execution_label(execution: AgentExecution) -> SharedString {
    crate::tr!(match execution {
        AgentExecution::Working => "agents.state_working",
        AgentExecution::Waiting => "agents.state_waiting",
        AgentExecution::Background => "agents.state_background",
        AgentExecution::Finished => "agents.state_finished",
        AgentExecution::Failed => "agents.state_failed",
        AgentExecution::Cancelled => "agents.state_cancelled",
        AgentExecution::Interrupted => "agents.state_interrupted",
    })
    .into_owned()
    .into()
}

/// The spinner while the agent `key` works, a dot in its state's colour
/// otherwise: one cross-fades into the other and the dot's colour glides.
pub(crate) fn status_mark(
    key: &str,
    execution: Option<AgentExecution>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let working = matches!(
        execution,
        Some(AgentExecution::Working | AgentExecution::Background)
    );
    let color = gpui_base::motion::transition(
        SharedString::from(format!("agent-status-color-{key}")),
        execution
            .filter(|_| !working)
            .map_or(cx.theme().muted_foreground, |execution| {
                execution_color(execution, cx)
            }),
        gpui_base::motion::Transition::new(GLIDE),
        window,
        cx,
    );
    crate::motion::cross_fade_part(
        SharedString::from(format!("agent-status-{key}")),
        working,
        CHANGE,
        window,
        cx,
        move |working, _, cx| {
            if *working {
                Spinner::new().color(cx.theme().primary).into_any_element()
            } else {
                crate::material::status_dot(color).into_any_element()
            }
        },
    )
}

pub struct AgentsPanel {
    store: Entity<WorkspaceStore>,
    session_id: Option<String>,
    /// Whether the panel is on screen and so holds the store's archived
    /// threads, from which archived agents come.
    shown: bool,
    /// Whether the Settled shelf is open.
    settled_expanded: bool,
    /// The row the pointer is over, whose actions fade in over its time.
    hover: crate::motion::ListHover,
    /// The phone sheet this panel fills, dismissed when an agent opens.
    sheet: Option<WeakEntity<PopoverState>>,
    /// What the entries that left the list showed, as they fold away.
    left: Rc<RefCell<crate::scroll::LeftRows<Entry>>>,
    focus: Rc<RefCell<ActionFocus>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsPanel {
    /// The agents of `session_id`'s thread, shown through [`Self::set_shown`]
    /// by the host: the right panel's Agents tab, or the phone's sheet.
    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.on_release(|this, cx| {
            if this.shown {
                this.store
                    .update(cx, |store, _| store.release_archived_sessions());
            }
        })];
        Self {
            store,
            session_id,
            shown: false,
            settled_expanded: false,
            hover: crate::motion::ListHover::new(crate::material::HOVER),
            sheet: None,
            left: Rc::new(RefCell::new(crate::scroll::LeftRows::over(ROWS))),
            focus: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            cx.notify();
        }
    }

    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        self.store.update(cx, |store, cx| {
            if shown {
                store.hold_archived_sessions(cx);
            } else {
                store.release_archived_sessions();
            }
        });
    }

    pub(crate) fn in_sheet(&mut self, sheet: WeakEntity<PopoverState>) {
        self.sheet = Some(sheet);
    }

    fn on_open(&mut self, action: &AgentOpen, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = self.sheet.as_ref().and_then(WeakEntity::upgrade) {
            sheet.update(cx, |sheet, cx| sheet.dismiss(window, cx));
        }
        let host = self.session_id.as_deref().and_then(|lead| {
            crate::shell::side_host_for(&self.store.read(cx).thread_key(lead), cx)
        });
        open_agent(
            &self.store,
            action.0.clone(),
            host,
            Some(panel_origin(action.0.clone())),
            window,
            cx,
        );
    }

    fn on_stop(&mut self, action: &AgentStop, _: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.interrupt_session(action.0.clone()));
    }

    fn on_stop_background(
        &mut self,
        action: &AgentStopBackground,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lead) = self.session_id.clone() else {
            return;
        };
        self.store.update(cx, |store, _| {
            store.stop_background_task(lead, Some(action.0.clone()))
        });
    }

    fn on_settle(&mut self, action: &AgentSettle, _: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.settle_session(action.0.clone()));
    }

    fn on_cancel(&mut self, action: &AgentCancel, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let id = action.0.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let id = id.clone();
            alert
                .title(crate::tr!("agents.cancel_title"))
                .description(crate::tr!("agents.cancel_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("agents.cancel"))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(crate::tr!("agents.cancel_keep"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _| store.cancel_agent(id.clone()));
                    true
                })
        });
    }

    /// The actions a row offers, with their icons and the action each sends.
    fn row_actions(row: &AgentRow) -> Vec<(IconName, SharedString, Box<dyn Action>)> {
        let mut actions: Vec<(IconName, SharedString, Box<dyn Action>)> = Vec::new();
        if let Some(task) = row.stop_task.clone() {
            actions.push((
                IconName::Square,
                crate::tr!("background.stop_task").into_owned().into(),
                Box::new(AgentStopBackground(task)),
            ));
        }
        let Some(agent) = row.agent.clone().filter(|_| !row.archived) else {
            return actions;
        };
        match row.delivery {
            Some(AgentDelivery::Running) => {
                actions.push((
                    IconName::Square,
                    crate::tr!("agents.stop").into_owned().into(),
                    Box::new(AgentStop(agent.clone())),
                ));
                actions.push((
                    IconName::CircleX,
                    crate::tr!("agents.cancel").into_owned().into(),
                    Box::new(AgentCancel(agent)),
                ));
            }
            Some(AgentDelivery::AwaitingSettle) => {
                actions.push((
                    IconName::CircleCheck,
                    crate::tr!("agents.settle").into_owned().into(),
                    Box::new(AgentSettle(agent.clone())),
                ));
                actions.push((
                    IconName::CircleX,
                    crate::tr!("agents.cancel").into_owned().into(),
                    Box::new(AgentCancel(agent)),
                ));
            }
            _ => {}
        }
        actions
    }

    fn badge(label: SharedString, style: BadgeStyle, cx: &App) -> gpui::Div {
        let theme = cx.theme();
        let none = gpui::transparent_black();
        let (bg, fg) = match style {
            BadgeStyle::Plain | BadgeStyle::Outline => (none, theme.muted_foreground),
            BadgeStyle::Warning => (theme.warning.opacity(0.12), theme.warning),
            BadgeStyle::Danger => (theme.danger.opacity(0.12), theme.danger),
        };
        material::semantic_chip(label, bg, fg, cx)
            .flex()
            .gap_1()
            .items_center()
            .when(matches!(style, BadgeStyle::Outline), |badge| {
                badge.border_1().border_color(theme.border)
            })
    }

    /// A row's badges, keyed by what each says: (key, style, checked).
    fn delivery_badges(row: &AgentRow) -> Vec<(SharedString, (BadgeStyle, bool))> {
        let mut badges = Vec::new();
        match row.delivery {
            Some(AgentDelivery::AwaitingSettle) => badges.push((
                "agents.delivery_awaiting".into(),
                (BadgeStyle::Warning, false),
            )),
            Some(AgentDelivery::NotDelivered) => badges.push((
                "agents.delivery_not_delivered".into(),
                (BadgeStyle::Plain, false),
            )),
            // A settled row is listed under Settled, which says it.
            Some(AgentDelivery::Running | AgentDelivery::Settled) | None => {}
        }
        match row.report {
            AgentReport::Reported => {
                badges.push(("agents.report_reported".into(), (BadgeStyle::Plain, true)))
            }
            AgentReport::Unreported => badges.push((
                "agents.report_unreported".into(),
                (BadgeStyle::Warning, false),
            )),
            AgentReport::Failed => {
                badges.push(("agents.report_failed".into(), (BadgeStyle::Danger, false)))
            }
            AgentReport::None | AgentReport::Expected => {}
        }
        if row.archived {
            badges.push(("agents.archived".into(), (BadgeStyle::Outline, false)));
        }
        badges
    }

    fn render_empty(cx: &App) -> AnyElement {
        material::empty_state(
            Icon::new(IconName::Users),
            crate::tr!("agents.empty_title"),
            String::new(),
            cx,
        )
        .into_any_element()
    }
}

/// What drawing the list's entries takes of the panel. The list draws them
/// as it lays out, after the panel has rendered: updating the panel then
/// would count as changing it, and build it again on the next frame.
#[derive(Clone)]
struct Entries {
    panel: WeakEntity<AgentsPanel>,
    store: Entity<WorkspaceStore>,
    session_id: Option<String>,
    hover: crate::motion::ListHover,
    focus: Rc<RefCell<ActionFocus>>,
    compact: bool,
}

/// The focus handles of each row's actions, by row, kept while the row is
/// listed whether the list draws it or not.
#[derive(Default)]
struct ActionFocus(std::collections::HashMap<String, Vec<gpui::FocusHandle>>);

impl ActionFocus {
    /// The handles of row `key`'s `count` actions.
    fn handles(&mut self, key: &str, count: usize, cx: &mut App) -> Vec<gpui::FocusHandle> {
        let handles = self.0.entry(key.to_owned()).or_default();
        while handles.len() < count {
            handles.push(cx.focus_handle());
        }
        handles[..count].to_vec()
    }

    /// Whether one of row `key`'s actions holds the keyboard's focus.
    fn holds_keyboard(&self, key: &str, window: &Window) -> bool {
        window.last_input_was_keyboard()
            && self
                .0
                .get(key)
                .is_some_and(|handles| handles.iter().any(|handle| handle.is_focused(window)))
    }
}

impl Entries {
    /// One entry of the list, carrying the gap under it, so the gap goes
    /// with it.
    fn render_entry(&self, entry: &Entry, window: &mut Window, cx: &mut App) -> AnyElement {
        let compact = self.compact;
        let inset = px(if compact {
            material::COMPACT_PAGE_INSET
        } else {
            material::CARD_INSET
        });
        let entry = match entry {
            Entry::SettleAll { disabled } => {
                let parent_id = self.session_id.clone();
                div()
                    .px(inset)
                    .py_2()
                    .debug_selector(|| "agents-settle-all".into())
                    .child(
                        Button::new("agents-settle-all")
                            .w_full()
                            .outline()
                            .small()
                            .label(crate::tr!("agents.settle_all"))
                            .touch(compact)
                            .disabled(*disabled)
                            .on_click({
                                let store = self.store.clone();
                                move |_, _, cx| {
                                    if let Some(id) = parent_id.clone() {
                                        store.update(cx, |store, _| store.settle_children(id));
                                    }
                                }
                            }),
                    )
                    .into_any_element()
            }
            Entry::Summary(summary) => div()
                .px(inset)
                .py_2()
                .text_secondary(compact, cx)
                .debug_selector(|| "agents-summary".into())
                .child(summary.clone())
                .into_any_element(),
            Entry::Row(row) => self.render_row(row, compact, window, cx),
            Entry::NativeCaption(provider) => material::list_caption(
                crate::tr!(
                    "agents.provider_subagents",
                    provider = provider.map_or_else(
                        || crate::tr!("providers.harness.unnamed").into_owned(),
                        str::to_owned
                    )
                )
                .into_owned()
                .into(),
                compact,
                cx,
            )
            .into_any_element(),
            Entry::ReadOnly => div()
                .px(inset)
                .py_1()
                .text_secondary(compact, cx)
                .child(crate::tr!("agents.readonly"))
                .into_any_element(),
            Entry::SettledHeader { count, expanded } => {
                let label: SharedString = crate::tr!("agents.settled_count", count = *count)
                    .into_owned()
                    .into();
                gpui_base::Button::new("agents-settled")
                    .debug_selector(|| "agents-settled".into())
                    .accessibility_label(label.clone())
                    .aria_expanded(*expanded)
                    .w_full()
                    .h(px(material::bar_height(material::Bar::Section, compact)))
                    .px(inset)
                    .flex()
                    .items_center()
                    .rounded(material::radius_card(cx))
                    .when(!crate::window_seam::is_mobile(cx), |header| {
                        header
                            .relative()
                            .child(material::row_hover(material::radius_card(cx), cx))
                    })
                    .on_click({
                        let panel = self.panel.clone();
                        move |_, _, cx| {
                            panel
                                .update(cx, |this, cx| {
                                    this.settled_expanded = !this.settled_expanded;
                                    cx.notify();
                                })
                                .ok();
                        }
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .items_center()
                            .text_caption(cx)
                            .child(crate::widgets::Chevron::new(
                                "agents-settled-chevron",
                                *expanded,
                            ))
                            .child(label),
                    )
                    .into_any_element()
            }
        };
        div().w_full().pb_0p5().child(entry).into_any_element()
    }

    fn render_row(
        &self,
        row: &AgentRow,
        compact: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let group = SharedString::from(format!("agent-row-{}", row.key));
        let title = if row.title.trim().is_empty() {
            crate::tr!("agents.untitled").into_owned()
        } else {
            row.title.clone()
        };
        let actions = AgentsPanel::row_actions(row);
        // Hover reveals the actions over the elapsed time; a touch screen has
        // no hover, so there they stand in its place.
        let coarse = crate::window_seam::is_mobile(cx);
        let inline_actions = !compact && !actions.is_empty();
        // A running clock ticks in a view of its own, only while it is drawn.
        let elapsed = match row.started_at.filter(|_| row.clock_runs()) {
            Some(started) => Some(
                crate::widgets::clock::Clock::since(
                    SharedString::from(format!("agent-elapsed-{}", row.key)),
                    started,
                    |elapsed| crate::chat::format_elapsed_seconds(elapsed).into(),
                )
                .into_any_element(),
            ),
            None => row.elapsed().map(IntoElement::into_any_element),
        }
        .filter(|_| !(inline_actions && coarse));
        // The actions fade in over the time as the pointer comes onto the
        // row, and the time fades out under them.
        // Each action button tracks a handle the panel keeps, so the actions
        // also fade in while the keyboard holds focus on one of them, and
        // the focus outlives the row scrolled out of the list.
        let focus = self.focus.borrow_mut().handles(&row.key, actions.len(), cx);
        let keyboard = self.focus.borrow().holds_keyboard(&row.key, window);
        let reveal = if inline_actions && !coarse {
            let focused = gpui_base::motion::transition(
                (group.clone(), "action-focus-reveal"),
                if keyboard { 1f32 } else { 0. },
                gpui_base::motion::Transition::new(GLIDE),
                window,
                cx,
            );
            self.hover.reveal(&row.key, window, cx).max(focused)
        } else {
            0.
        };
        let trailing = (inline_actions || elapsed.is_some()).then(|| {
            let cluster_width =
                actions.len() as f32 * 20. + actions.len().saturating_sub(1) as f32 * 2.;
            div()
                .relative()
                .flex_none()
                .h(px(20.))
                .when(inline_actions, |slot| slot.min_w(px(cluster_width)))
                .when_some(elapsed, |slot, elapsed| {
                    slot.child(
                        h_flex()
                            .h_full()
                            .justify_end()
                            .items_center()
                            .whitespace_nowrap()
                            .text_secondary(compact, cx)
                            .when(inline_actions, |time| time.opacity(1. - reveal))
                            .child(elapsed),
                    )
                })
                .when(inline_actions, |slot| {
                    slot.child(
                        h_flex()
                            .absolute()
                            .right_0()
                            .top_0()
                            .gap_0p5()
                            // Opacity, not visibility, keeps the buttons tab
                            // stops so keyboard focus can reveal them.
                            .when(!coarse, |cluster| cluster.opacity(reveal))
                            .children(actions.iter().enumerate().map(
                                |(index, (icon, label, action))| {
                                    let action = action.boxed_clone();
                                    Button::new(SharedString::from(format!(
                                        "agent-action-{}-{index}",
                                        row.key
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(icon.clone()).text_color(muted))
                                    .aria_label(label.clone())
                                    .tooltip(label.clone())
                                    .track_focus(&focus[index])
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            window.dispatch_action(action.boxed_clone(), cx);
                                        },
                                    )
                                },
                            )),
                    )
                })
        });
        // A badge that comes or goes widens in or narrows away, carrying
        // the gap before it.
        let badges = crate::motion::keyed_across(
            SharedString::from(format!("agent-badges-{}", row.key)),
            AgentsPanel::delivery_badges(row),
            ROWS,
            window,
            cx,
            |key, (style, checked), _, cx| {
                div()
                    .pl_2()
                    .child(
                        AgentsPanel::badge(
                            SharedString::from(crate::tr!(key.as_ref()).into_owned()),
                            *style,
                            cx,
                        )
                        .when(*checked, |badge| badge.child(Icon::new(IconName::Check))),
                    )
                    .into_any_element()
            },
        )
        .render(h_flex().flex_none().items_center());
        let line_one = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .text_body(compact, cx)
            .child(status_mark(&row.key, row.execution, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_2()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(title.clone())),
            )
            .child(badges)
            .children(trailing.map(|trailing| div().pl_2().child(trailing)));
        let line_two = h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .items_center()
            .text_secondary(compact, cx)
            .when_some(row.provider, |line, provider| {
                line.child(
                    crate::provider_card::profile_glyph(
                        self.store.read(cx),
                        provider,
                        row.profile_id.as_deref(),
                    )
                    .flex_none(),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(row.detail())),
            )
            .when_some(row.state(), |line, state| {
                line.child(div().flex_none().child("·")).child(
                    div()
                        .flex_none()
                        .text_color(state.color(cx))
                        .child(state.label()),
                )
            });
        let activity = activity_line("agent", &row.key, row.activity.clone(), window, cx);
        // The result's preview folds in under the row as it arrives.
        let failed = row.execution == Some(AgentExecution::Failed);
        let preview = crate::motion::shown(
            SharedString::from(format!("agent-preview-{}", row.key)),
            row.result(),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold(
                div().w_full().pt_0p5().child(
                    div()
                        .w_full()
                        .text_secondary(false, cx)
                        .line_clamp(2)
                        .overflow_hidden()
                        .text_color(if failed { cx.theme().danger } else { muted })
                        .child(crate::widgets::fade_text(shown.value.clone())),
                ),
            )
        });
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .child(line_one)
            .child(line_two.pt_0p5())
            .children(activity)
            .children(preview);
        let label = SharedString::from(title);
        let open = row.open.clone();
        // The list says once, under the native rows, that they are read-only.
        let readonly = row.managed_by.and(row.agent_type.clone());
        let base = material::panel_row(
            SharedString::from(format!("agent-{}", row.key)),
            label,
            compact,
            cx,
        )
        .child(self.hover.hitbox(row.key.clone()))
        .child(content)
        .when(compact && !actions.is_empty(), |row_el| {
            let actions: Vec<_> = row
                .open
                .iter()
                .map(|id| {
                    (
                        SharedString::from(crate::tr!("agents.open").into_owned()),
                        Box::new(AgentOpen(id.clone())) as Box<dyn Action>,
                    )
                })
                .chain(
                    actions
                        .iter()
                        .map(|(_, label, action)| (label.clone(), action.boxed_clone())),
                )
                .collect();
            row_el.child(
                material::toolbar_icon_button(
                    SharedString::from(format!("agent-more-{}", row.key)),
                    IconName::Ellipsis,
                    crate::tr!("mobile.more_actions"),
                    true,
                )
                .dropdown_menu(move |menu, _, _| {
                    actions.iter().fold(menu, |menu, (label, action)| {
                        menu.menu(label.clone(), action.boxed_clone())
                    })
                }),
            )
        });
        // What the agent beside the lead grows out of: the lead's chat
        // records where the row is.
        let origins = self
            .session_id
            .as_deref()
            .and_then(|lead| crate::shell::side_host_for(&self.store.read(cx).thread_key(lead), cx))
            .map(|chat| chat.read(cx).side_origins().clone());
        let origin = open.clone().map(panel_origin);
        let row_el = base
            .group(group)
            .debug_selector({
                let key = row.key.clone();
                move || format!("agent-row-{key}")
            })
            .when_some(open, |row_el, id| {
                row_el.cursor_pointer().on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(AgentOpen(id.clone())), cx);
                })
            })
            .when_some(readonly, |row_el, text| {
                row_el.with_tooltip(move |_, _| Tooltip::new(text.clone()))
            })
            .into_any_element();
        match (origins, origin) {
            (Some(origins), Some(origin)) => origins
                .mark(origin, material::radius_card(cx), row_el)
                .into_any_element(),
            _ => row_el,
        }
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let agents = {
            let store = self.store.read(cx);
            self.session_id
                .as_deref()
                .map(|id| Agents::of(store, id, cx).with_results(store, id, cx))
                .unwrap_or_default()
        };
        let root = v_flex()
            .id("agents-panel")
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_stop))
            .on_action(cx.listener(Self::on_stop_background))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_cancel));
        // The empty state and the list cross-fade as the first agent starts
        // or the last one goes. The current one stays in flow: the phone
        // sheet is as tall as what it holds, and gives layers that only
        // fill it no height.
        let fade = crate::motion::CrossFade::new(
            "agents-empty-or-list",
            usize::from(!agents.is_empty()),
            2,
            CHANGE,
            window,
            cx,
        )
        .column()
        .span();
        let mut layers = Vec::new();
        if fade.shows(0) {
            layers.push((0, Self::render_empty(cx)));
        }
        if fade.shows(1) {
            layers.push((1, self.render_list(&agents, compact, window, cx)));
        }
        root.child(fade.render(layers).flex_1().min_h_0().w_full())
    }
}

/// One entry of the agents list.
#[derive(Clone)]
enum Entry {
    SettleAll {
        disabled: bool,
    },
    Summary(String),
    Row(Box<AgentRow>),
    /// Names the provider that runs the native subagents listed under it.
    NativeCaption(Option<&'static str>),
    ReadOnly,
    SettledHeader {
        count: usize,
        expanded: bool,
    },
}

impl AgentsPanel {
    /// The list keyed by entry, so a row that moves from one group to
    /// another glides there, rows that come or go fold in or away, and the
    /// settled rows fold open and shut under their shelf's header at the end. It lays out only the
    /// entries in view: a lead can run scores of agents.
    fn render_list(
        &mut self,
        agents: &Agents,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut entries: Vec<(SharedString, Entry)> = vec![(
            "settle-all".into(),
            Entry::SettleAll {
                disabled: agents.needs_settle.is_empty(),
            },
        )];
        if let Some(summary) = agents.summary() {
            entries.push(("summary".into(), Entry::Summary(summary)));
        }
        let row = |row: &AgentRow| {
            (
                SharedString::from(format!("row-{}", row.key)),
                Entry::Row(Box::new(row.clone())),
            )
        };
        entries.extend(
            agents
                .needs_settle
                .iter()
                .chain(&agents.running)
                .chain(&agents.done)
                .map(row),
        );
        if !agents.native.is_empty() {
            let provider = agents.native.first().and_then(|row| row.managed_by);
            entries.push(("native-caption".into(), Entry::NativeCaption(provider)));
            entries.extend(agents.native.iter().map(row));
            if provider.is_some() {
                entries.push(("native-readonly".into(), Entry::ReadOnly));
            }
        }
        if !agents.settled.is_empty() {
            let expanded = self.settled_expanded;
            entries.push((
                "settled-header".into(),
                Entry::SettledHeader {
                    count: agents.settled.len(),
                    expanded,
                },
            ));
            if expanded {
                entries.extend(agents.settled.iter().map(row));
            }
        }
        let keys: Rc<[u64]> = entries
            .iter()
            .map(|(key, _)| crate::scroll::row_key(key))
            .collect();
        let entries: Rc<[Entry]> = entries.into_iter().map(|(_, entry)| entry).collect();
        let listed: std::collections::HashSet<&str> = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Row(row) => Some(row.key.as_str()),
                _ => None,
            })
            .collect();
        let mut focus = self.focus.borrow_mut();
        focus.0.retain(|key, _| listed.contains(key.as_str()));
        // The row the keyboard is on is scrolled into view, where it can
        // be seen and stays drawn.
        let focused = entries.iter().position(|entry| match entry {
            Entry::Row(row) => focus.holds_keyboard(&row.key, window),
            _ => false,
        });
        drop(focus);
        self.left
            .borrow_mut()
            .update(keys.iter().copied().zip(entries.iter().cloned()), cx);
        let drawer = Entries {
            panel: cx.weak_entity(),
            store: self.store.clone(),
            session_id: self.session_id.clone(),
            hover: self.hover.clone(),
            focus: self.focus.clone(),
            compact,
        };
        let left = self.left.clone();
        let draw = {
            let drawer = drawer.clone();
            move |index: usize, window: &mut Window, cx: &mut App| {
                drawer.render_entry(&entries[index], window, cx)
            }
        };
        let leaving = move |key: u64, window: &mut Window, cx: &mut App| {
            let entry = left.borrow().get(key)?.clone();
            Some(drawer.render_entry(&entry, window, cx))
        };
        crate::scroll::VirtualList::measured("agents-list", keys.len(), draw)
            .keyed_live(keys, ROWS, leaving)
            .measure_all()
            .page()
            .reveal(focused)
            .size_full()
            .min_w_0()
            .px(if compact {
                px(0.)
            } else {
                cx.theme().tokens.spacing.sm
            })
            .pb_3()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use agent::ProviderKind;
    use gpui::{AppContext as _, TestAppContext};
    use tcode_core::project::SessionMeta;
    use tcode_protocol::{EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic};

    use super::Agents;
    use crate::store::{WorkspaceAttachment, WorkspaceStore};

    /// A settled child is not resident, so the host reports no activity for
    /// it: it is settled and the badge leaves it out, while an unsettled
    /// child the host has not reported yet is running.
    #[gpui::test]
    fn a_settled_child_without_reported_activity_is_settled(cx: &mut TestAppContext) {
        let (to_host, _requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.parent_session_id = parent.map(str::to_string);
            meta
        };
        let mut settled = thread("settled", Some("lead"));
        settled.settled_at = Some(1);
        let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                projects: Vec::new(),
                sessions: vec![
                    thread("lead", None),
                    settled,
                    thread("unsettled", Some("lead")),
                ],
                summary: Default::default(),
            }),
        }))
        .unwrap();
        crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        cx.run_until_parked();

        store.read_with(cx, |store, cx| {
            let agents = Agents::of(store, "lead", cx);
            let keys = |rows: &[super::AgentRow]| {
                rows.iter().map(|row| row.key.clone()).collect::<Vec<_>>()
            };
            assert_eq!(keys(&agents.settled), ["settled"]);
            assert_eq!(keys(&agents.running), ["unsettled"]);
            assert_eq!(agents.outstanding(), 1);
        });
        link.close();
    }

    /// A child the host reports waiting on an approval says so on its line,
    /// ahead of how the lead's agents stand, and names its own branch.
    #[gpui::test]
    fn a_child_awaiting_approval_says_so_first(cx: &mut TestAppContext) {
        use tcode_core::project::WorktreeInfo;
        use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
        use tcode_protocol::{AgentStatus, SessionActivity};
        let (to_host, _requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.parent_session_id = parent.map(str::to_owned);
            meta
        };
        let mut asking = thread("asking", Some("lead"));
        asking.model = Some("gpt-6".into());
        asking.worktree = Some(WorktreeInfo {
            root_project_path: "/project".into(),
            base: "main".into(),
            branch: "tcode/asking".into(),
        });
        let mut index = IndexSnapshot {
            sessions: vec![thread("lead", None), asking],
            projects: vec![],
            summary: Default::default(),
        };
        index.summary.activity.insert(
            "asking".into(),
            SessionActivity {
                agent: Some(AgentStatus {
                    execution: AgentExecution::Waiting,
                    delivery: AgentDelivery::Running,
                    report: AgentReport::Expected,
                    run_started_at: None,
                    run_completed_at: None,
                }),
                working: false,
                turn_running: true,
                background: Default::default(),
                unsettled: 0,
                wakes: Default::default(),
                waiting_for_approval: true,
                waiting_for_input: false,
                settle_blocked: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                latest_activity: None,
            },
        );
        let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(index),
        }))
        .unwrap();
        crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
        driver.wait_until(cx, "child status", |cx| {
            store.read_with(cx, |store, _| store.agent_status("asking").is_some())
        });

        store.read_with(cx, |store, cx| {
            let agents = Agents::of(store, "lead", cx);
            let lines = agents.lines();
            assert_eq!(
                lines[0].state,
                Some(super::AgentState::Attention(super::Attention::Approval))
            );
            assert_eq!(lines[0].model.as_deref(), Some("gpt-6"));
            assert_eq!(lines[0].branch.as_deref(), Some("tcode/asking"));
            assert_eq!(
                agents.summary(),
                Some(format!(
                    "{} · {}",
                    crate::tr!("agents.summary_approval_one"),
                    crate::tr!("agents.summary_running_one")
                ))
            );
        });
        link.close();
    }

    /// What a running agent is doing comes from the host's index, on the
    /// dispatched agent's thread or a native subagent's mirror, and gives way
    /// to its result as it settles, even before the host's index catches up
    /// with a native subagent the lead's transcript has seen end.
    #[gpui::test]
    fn a_running_agents_activity_gives_way_to_its_result(cx: &mut TestAppContext) {
        use agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem};
        use gpui::{px, size};
        use tcode_core::session::LatestActivity;
        use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
        use tcode_protocol::{AgentStatus, SessionActivity};
        cx.update(crate::theme::init);
        let (to_host, _requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.title = id.into();
            meta.parent_session_id = parent.map(str::to_owned);
            meta
        };
        let mut mirror = thread("mirror", Some("lead"));
        mirror.native_subagent = Some("spawn".into());
        let activity =
            |agent: Option<AgentStatus>, latest_activity: Option<LatestActivity>| SessionActivity {
                agent,
                working: latest_activity.is_some(),
                turn_running: latest_activity.is_some(),
                background: Default::default(),
                unsettled: 0,
                wakes: Default::default(),
                waiting_for_approval: false,
                waiting_for_input: false,
                settle_blocked: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                latest_activity,
            };
        let status = |execution, delivery, report| AgentStatus {
            execution,
            delivery,
            report,
            run_started_at: None,
            run_completed_at: None,
        };
        let command = LatestActivity::Command {
            text: "cargo test".into(),
        };
        let mut index = IndexSnapshot {
            sessions: vec![thread("lead", None), thread("worker", Some("lead")), mirror],
            projects: vec![],
            summary: Default::default(),
        };
        index.summary.activity.insert(
            "worker".into(),
            activity(
                Some(status(
                    AgentExecution::Working,
                    AgentDelivery::Running,
                    AgentReport::Expected,
                )),
                Some(command.clone()),
            ),
        );
        index.summary.activity.insert(
            "mirror".into(),
            activity(None, Some(LatestActivity::Thinking)),
        );
        let line = |topic, event| {
            tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic,
                event,
            }))
            .unwrap()
        };
        crate::store::tests::seed_full_scope(
            &store,
            &incoming,
            vec![line(Topic::Index, ServerEvent::IndexSnapshot(index))],
            cx,
        );
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
        driver.wait_until(cx, "child status", |cx| {
            store.read_with(cx, |store, _| store.agent_status("worker").is_some())
        });
        store.update(cx, |store, cx| store.show_alone("lead".into(), cx));

        let events = || Topic::SessionEvents {
            session_id: "lead".into(),
        };
        let spawn = |status, summary: Option<&str>| ThreadItem {
            id: "spawn".into(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: "explore".into(),
                description: "Survey".into(),
                status,
                summary: summary.map(str::to_owned),
                model: None,
                effort: None,
            },
        };
        let records = vec![
            AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            }
            .into(),
            AgentEvent::ItemStarted(spawn(ItemStatus::InProgress, Some("Partway"))).into(),
        ];
        incoming
            .try_send(line(
                events(),
                ServerEvent::SessionSnapshot {
                    total: 2,
                    total_turns: 1,
                    truncated: false,
                    from: 0,
                    end: 2,
                    records,
                },
            ))
            .unwrap();
        let lines_of = |store: &WorkspaceStore, cx: &gpui::App| {
            let lines = Agents::of(store, "lead", cx)
                .with_results(store, "lead", cx)
                .lines();
            let line = |key: &str| {
                let line = lines.iter().find(|line| line.key == key).unwrap();
                (line.activity.clone(), line.preview.clone())
            };
            [line("worker"), line("spawn")]
        };
        driver.wait_until(cx, "the lead's transcript", |cx| {
            store.read_with(cx, |store, cx| {
                lines_of(store, cx)[1].0 == Some(LatestActivity::Thinking)
            })
        });
        assert_eq!(
            store.read_with(cx, |store, cx| lines_of(store, cx)),
            [
                (Some(command), None),
                (Some(LatestActivity::Thinking), None)
            ]
        );

        let (_, cx) = cx.add_window_view(|window, cx| {
            let panel =
                cx.new(|cx| super::AgentsPanel::new(store.clone(), Some("lead".into()), cx));
            gpui_base::Root::new(panel, window, cx)
        });
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(cx.debug_bounds("agent-activity-worker").is_some());
        assert!(cx.debug_bounds("agent-activity-spawn").is_some());

        let callback = format!(
            "{} reported.\nThe tests pass.",
            tcode_core::notice::callback_header("worker", "worker")
        );
        for message in [
            line(
                events(),
                ServerEvent::SessionEvent(
                    AgentEvent::ItemCompleted(spawn(ItemStatus::Completed, Some("Surveyed")))
                        .into(),
                ),
            ),
            line(
                events(),
                ServerEvent::SessionEvent(
                    AgentEvent::ItemCompleted(ThreadItem {
                        id: "callback".into(),
                        parent_item_id: None,
                        content: ItemContent::UserMessage {
                            text: callback,
                            attachments: vec![],
                        },
                    })
                    .into(),
                ),
            ),
            line(
                Topic::Index,
                ServerEvent::IndexActivitiesChanged {
                    upserts: [(
                        "worker".to_owned(),
                        activity(
                            Some(status(
                                AgentExecution::Finished,
                                AgentDelivery::AwaitingSettle,
                                AgentReport::Reported,
                            )),
                            None,
                        ),
                    )]
                    .into(),
                    removed: vec![],
                },
            ),
        ] {
            incoming.try_send(message).unwrap();
        }
        driver.wait_until(cx, "the agents to settle", |cx| {
            store.read_with(cx, |store, cx| lines_of(store, cx)[0].1.is_some())
        });
        assert_eq!(
            store.read_with(cx, |store, cx| lines_of(store, cx)),
            [
                (None, Some("The tests pass.".to_owned())),
                (None, Some("Surveyed".to_owned()))
            ]
        );
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        assert!(cx.debug_bounds("agent-activity-worker").is_none());
        assert!(cx.debug_bounds("agent-activity-spawn").is_none());
        link.close();
    }

    /// A tool reads as its window names it, with what it acts on.
    #[test]
    fn a_tool_call_reads_as_its_window_names_it() {
        let label = super::activity_label(&tcode_core::session::LatestActivity::Tool {
            name: "mcp__tcode_control__read".into(),
            target: Some("src/lib.rs".into()),
        });
        assert!(label.contains("tcode_control · read"), "{label}");
        assert!(label.contains("src/lib.rs"), "{label}");
    }

    #[gpui::test]
    fn settle_all_button_sends_the_lead_command(cx: &mut TestAppContext) {
        use gpui::{px, size};
        use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
        use tcode_protocol::{
            AgentStatus, ClientPayload, Command, SessionActivity, decode_client_line,
        };
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.title = id.into();
            meta.parent_session_id = parent.map(str::to_owned);
            meta
        };
        let mut index = IndexSnapshot {
            sessions: vec![thread("lead", None), thread("finished", Some("lead"))],
            projects: vec![],
            summary: Default::default(),
        };
        index.summary.activity.insert(
            "finished".into(),
            SessionActivity {
                agent: Some(AgentStatus {
                    execution: AgentExecution::Finished,
                    delivery: AgentDelivery::AwaitingSettle,
                    report: AgentReport::Reported,
                    run_started_at: None,
                    run_completed_at: None,
                }),
                working: false,
                turn_running: false,
                background: Default::default(),
                unsettled: 0,
                wakes: Default::default(),
                waiting_for_approval: false,
                waiting_for_input: false,
                settle_blocked: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                latest_activity: None,
            },
        );
        let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(index),
        }))
        .unwrap();
        crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
        driver.wait_until(cx, "child status", |cx| {
            store.read_with(cx, |store, _| store.agent_status("finished").is_some())
        });
        store.update(cx, |store, cx| store.show_alone("lead".into(), cx));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let panel =
                cx.new(|cx| super::AgentsPanel::new(store.clone(), Some("lead".into()), cx));
            gpui_base::Root::new(panel, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        // The list lays its rows out again at the width it is resized to on
        // the frame it asks for, which the app draws before a click lands.
        crate::test_frames::draw_settled(cx);
        let button = cx
            .debug_bounds("agents-settle-all")
            .expect("settle-all button is drawn");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        let mut commands = Vec::new();
        driver.wait_until(cx, "settle-all command", |_| {
            while let Ok(line) = outgoing.try_recv() {
                if let ClientPayload::Command(command) = decode_client_line(&line).unwrap().payload
                {
                    commands.push(command);
                }
            }
            commands
                .iter()
                .any(|command| matches!(command, Command::SettleChildren { .. }))
        });
        assert_eq!(
            commands
                .into_iter()
                .filter(|command| matches!(command, Command::SettleChildren { .. }))
                .collect::<Vec<_>>(),
            vec![Command::SettleChildren {
                session_id: "lead".into()
            }]
        );
        link.close();
    }

    /// The agents a lead started stand in its rail until each is settled: a
    /// settled one leaves the rail, which goes once none is left, for the
    /// Agents view's Settled shelf, which opens on it; un-settled, it comes
    /// back to the rail.
    #[gpui::test]
    fn a_settled_agent_leaves_the_rail_for_the_settled_shelf(cx: &mut TestAppContext) {
        use gpui::{Modifiers, VisualContext as _, VisualTestContext, px, size};
        use tcode_core::control::StartedBy;
        use tcode_core::project::{ChildExpectation, Project};
        use tcode_core::settlement::AgentReport;
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        use crate::chat::ChatView;
        use crate::test_frames::draw_settled;

        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("settled-agents");
        let project = Project::from_root(root.clone());
        let disk = SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project).unwrap();
        let thread = |id: &str| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
            meta.id = id.into();
            meta.title = id.into();
            meta.project_id = Some(project.id.clone());
            meta
        };
        disk.upsert_meta(&thread("lead")).unwrap();
        // As `start_thread` with `subagent: true` leaves a child, once it
        // has reported.
        for (id, created_at) in [("child-a", 2), ("child-b", 1)] {
            let mut child = thread(id);
            child.parent_session_id = Some("lead".into());
            child.started_by = Some(StartedBy {
                thread_id: "lead".into(),
                request_id: id.into(),
            });
            child.expectation = Some(ChildExpectation {
                state: AgentReport::Reported,
                ..ChildExpectation::opened(created_at)
            });
            child.created_at = created_at;
            disk.upsert_meta(&child).unwrap();
        }
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        driver.wait_until(cx, "the lead's agents", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && store.thread_meta("child-b").is_some()
            })
        });
        store.update(cx, |store, cx| store.show_alone("lead".into(), cx));
        driver.wait_until(cx, "the lead", |cx| {
            store.read_with(cx, |store, cx| !store.chat_loading_for(Some("lead"), cx))
        });

        let window_state = cx.new(|_| crate::window_state::WindowState::default());
        let chat = {
            let (_, chat) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(store.clone(), window_state, Some("lead".into()), window, cx)
            });
            chat.window_handle()
        };
        let panel = {
            let (_, panel) = cx.add_window_view(|window, cx| {
                let panel =
                    cx.new(|cx| super::AgentsPanel::new(store.clone(), Some("lead".into()), cx));
                gpui_base::Root::new(panel, window, cx)
            });
            panel.window_handle()
        };
        let chat = VisualTestContext::from_window(chat, cx).into_mut();
        let panel = VisualTestContext::from_window(panel, cx).into_mut();
        chat.simulate_resize(size(px(1800.), px(1000.)));
        panel.simulate_resize(size(px(400.), px(800.)));
        let shown = |cx: &mut VisualTestContext, selectors: &[&'static str]| {
            draw_settled(cx);
            selectors
                .iter()
                .map(|selector| cx.debug_bounds(selector).is_some())
                .collect::<Vec<_>>()
        };
        let rail = [
            "rail-card-agents",
            "rail-agent-child-a",
            "rail-agent-child-b",
        ];
        assert_eq!(shown(chat, &rail), [true, true, true]);
        let mut set = |cx: &mut TestAppContext, id: &str, settle: bool| {
            store.update(cx, |store, _| {
                if settle {
                    store.settle_session(id.into());
                } else {
                    store.make_session_active(id.into());
                }
            });
            driver.wait_until(cx, "the agent's settlement", |cx| {
                store.read_with(cx, |store, _| {
                    store
                        .thread_meta(id)
                        .is_some_and(|meta| meta.is_settled() == settle)
                })
            });
        };

        set(cx, "child-a", true);
        assert_eq!(shown(chat, &rail), [true, false, true]);
        let shelf = ["agents-settled", "agent-row-child-a", "agent-row-child-b"];
        assert_eq!(shown(panel, &shelf), [true, false, true]);
        let header = panel.debug_bounds("agents-settled").unwrap();
        panel.simulate_click(header.center(), Modifiers::default());
        assert_eq!(shown(panel, &shelf), [true, true, true]);

        set(cx, "child-b", true);
        assert_eq!(shown(chat, &rail), [false, false, false]);

        set(cx, "child-a", false);
        assert_eq!(shown(chat, &rail), [true, true, false]);
        driver.shutdown(cx, &host);
        std::fs::remove_dir_all(root).ok();
    }
}
