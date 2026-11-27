//! In-process MCP server through which a thread's model observes and operates Tcode.
//! Every tool is offered to every thread the server is given to: what a call may do without
//! asking is the harness's permission mode to decide, as for any other tool, and each tool's
//! description says when it is the user's to ask for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub mod params;
mod tools;

pub use params::*;
pub use tools::{instructions, tool_descriptions};

/// How a call names the thread making it. Which thread that is comes from the connection: the
/// model cannot be told its id, since a fork's conversation is its source's and a harness
/// shows a server's instructions and its own system prompt once per conversation.
pub const OWN_THREAD: &str = "self";

/// One call of a `tcode_control` tool, with its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlOp {
    ListProjects(ListProjectsParams),
    ListThreads(ListThreadsParams),
    GetThread(GetThreadParams),
    Search(SearchParams),
    Outline(OutlineParams),
    Read(ReadParams),
    SearchInItem(SearchInItemParams),
    FileHistory(FileHistoryParams),
    GetGit(GetGitParams),
    GetPullRequest(GetPullRequestParams),
    ListProviders(ListProvidersParams),
    GetSettings(GetSettingsParams),
    ReadTerminal(ReadTerminalParams),
    ReviewCandidates(ReviewCandidatesParams),
    UpdateThreads(UpdateThreadsParams),
    SettleSelf(SettleSelfParams),
    StartThread(StartThreadParams),
    SendMessage(SendMessageParams),
    ControlThread(ControlThreadParams),
    PullRequest(PullRequestParams),
    Git(GitParams),
    CreateProject(CreateProjectParams),
    MoveThread(MoveThreadParams),
    NotifyWhen(NotifyWhenParams),
    CancelNotify(CancelNotifyParams),
    RespondInput(RespondInputParams),
    RespondApproval(RespondApprovalParams),
    ReportResult(ReportResultParams),
    Terminal(TerminalParams),
    DeleteThreads(DeleteThreadsParams),
    Rewind(RewindParams),
    PatchSettings(PatchSettingsParams),
    SetModelGuide(SetModelGuideParams),
    ListMachines(ListMachinesParams),
    StartThreadOnMachine(StartThreadOnMachineParams),
}

/// What decides which tools a thread is offered: whether threads are given `tcode_control`
/// at all, whether another thread started this one as a subagent, whether the user opened
/// it to research a model's guide, and whether its machine lets it start threads on the
/// other machines of its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadAccess {
    pub enabled: bool,
    pub subagent: bool,
    pub model_guide: bool,
    pub machines: bool,
}

impl ThreadAccess {
    pub fn none() -> Self {
        Self {
            enabled: false,
            subagent: false,
            model_guide: false,
            machines: false,
        }
    }

    /// Every tool, but `report_result` only for a subagent, its one channel to the thread
    /// that started it, `set_model_guide` only for the thread that researches a guide, and
    /// the tools about the group's machines only where the machine lets its threads reach
    /// them.
    pub fn offers(&self, tool: &str) -> bool {
        self.enabled
            && match tool {
                REPORT_RESULT => self.subagent,
                SET_MODEL_GUIDE => self.model_guide,
                LIST_MACHINES | START_THREAD_ON_MACHINE => self.machines,
                _ => true,
            }
    }
}

const REPORT_RESULT: &str = "report_result";
const SET_MODEL_GUIDE: &str = "set_model_guide";
const LIST_MACHINES: &str = "list_machines";
const START_THREAD_ON_MACHINE: &str = "start_thread_on_machine";

impl ControlOp {
    /// The call of `tool` with `args`, read as the server reads a call's arguments.
    pub fn parse(tool: &str, args: serde_json::Value) -> Result<Self, String> {
        fn read<P: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<P, String> {
            serde_json::from_value(args).map_err(|error| format!("invalid arguments: {error}"))
        }
        Ok(match tool {
            "list_projects" => Self::ListProjects(read(args)?),
            "list_threads" => Self::ListThreads(read(args)?),
            "get_thread" => Self::GetThread(read(args)?),
            "search" => Self::Search(read(args)?),
            "outline" => Self::Outline(read(args)?),
            "read" => Self::Read(read(args)?),
            "search_in_item" => Self::SearchInItem(read(args)?),
            "file_history" => Self::FileHistory(read(args)?),
            "get_git" => Self::GetGit(read(args)?),
            "get_pull_request" => Self::GetPullRequest(read(args)?),
            "list_providers" => Self::ListProviders(read(args)?),
            "get_settings" => Self::GetSettings(read(args)?),
            "read_terminal" => Self::ReadTerminal(read(args)?),
            "review_candidates" => Self::ReviewCandidates(read(args)?),
            "update_threads" => Self::UpdateThreads(read(args)?),
            "settle_self" => Self::SettleSelf(read(args)?),
            "start_thread" => Self::StartThread(read(args)?),
            "send_message" => Self::SendMessage(read(args)?),
            "control_thread" => Self::ControlThread(read(args)?),
            "pull_request" => Self::PullRequest(read(args)?),
            "git" => Self::Git(read(args)?),
            "create_project" => Self::CreateProject(read(args)?),
            "move_thread" => Self::MoveThread(read(args)?),
            "notify_when" => Self::NotifyWhen(read(args)?),
            "cancel_notify" => Self::CancelNotify(read(args)?),
            "respond_input" => Self::RespondInput(read(args)?),
            "respond_approval" => Self::RespondApproval(read(args)?),
            REPORT_RESULT => Self::ReportResult(read(args)?),
            "terminal" => Self::Terminal(read(args)?),
            "delete_threads" => Self::DeleteThreads(read(args)?),
            "rewind" => Self::Rewind(read(args)?),
            "patch_settings" => Self::PatchSettings(read(args)?),
            SET_MODEL_GUIDE => Self::SetModelGuide(read(args)?),
            LIST_MACHINES => Self::ListMachines(read(args)?),
            START_THREAD_ON_MACHINE => Self::StartThreadOnMachine(read(args)?),
            other => return Err(format!("unknown tool: {other}")),
        })
    }

    /// The call's arguments, as [`Self::parse`] reads them back.
    pub fn args(&self) -> serde_json::Value {
        let args = match self {
            Self::ListProjects(p) => serde_json::to_value(p),
            Self::ListThreads(p) => serde_json::to_value(p),
            Self::GetThread(p) => serde_json::to_value(p),
            Self::Search(p) => serde_json::to_value(p),
            Self::Outline(p) => serde_json::to_value(p),
            Self::Read(p) => serde_json::to_value(p),
            Self::SearchInItem(p) => serde_json::to_value(p),
            Self::FileHistory(p) => serde_json::to_value(p),
            Self::GetGit(p) => serde_json::to_value(p),
            Self::GetPullRequest(p) => serde_json::to_value(p),
            Self::ListProviders(p) => serde_json::to_value(p),
            Self::GetSettings(p) => serde_json::to_value(p),
            Self::ReadTerminal(p) => serde_json::to_value(p),
            Self::ReviewCandidates(p) => serde_json::to_value(p),
            Self::UpdateThreads(p) => serde_json::to_value(p),
            Self::SettleSelf(p) => serde_json::to_value(p),
            Self::StartThread(p) => serde_json::to_value(p),
            Self::SendMessage(p) => serde_json::to_value(p),
            Self::ControlThread(p) => serde_json::to_value(p),
            Self::PullRequest(p) => serde_json::to_value(p),
            Self::Git(p) => serde_json::to_value(p),
            Self::CreateProject(p) => serde_json::to_value(p),
            Self::MoveThread(p) => serde_json::to_value(p),
            Self::NotifyWhen(p) => serde_json::to_value(p),
            Self::CancelNotify(p) => serde_json::to_value(p),
            Self::RespondInput(p) => serde_json::to_value(p),
            Self::RespondApproval(p) => serde_json::to_value(p),
            Self::ReportResult(p) => serde_json::to_value(p),
            Self::Terminal(p) => serde_json::to_value(p),
            Self::DeleteThreads(p) => serde_json::to_value(p),
            Self::Rewind(p) => serde_json::to_value(p),
            Self::PatchSettings(p) => serde_json::to_value(p),
            Self::SetModelGuide(p) => serde_json::to_value(p),
            Self::ListMachines(p) => serde_json::to_value(p),
            Self::StartThreadOnMachine(p) => serde_json::to_value(p),
        };
        args.expect("tool parameters serialize")
    }

    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::ListProjects(_) => "list_projects",
            Self::ListThreads(_) => "list_threads",
            Self::GetThread(_) => "get_thread",
            Self::Search(_) => "search",
            Self::Outline(_) => "outline",
            Self::Read(_) => "read",
            Self::SearchInItem(_) => "search_in_item",
            Self::FileHistory(_) => "file_history",
            Self::GetGit(_) => "get_git",
            Self::GetPullRequest(_) => "get_pull_request",
            Self::ListProviders(_) => "list_providers",
            Self::GetSettings(_) => "get_settings",
            Self::ReadTerminal(_) => "read_terminal",
            Self::ReviewCandidates(_) => "review_candidates",
            Self::UpdateThreads(_) => "update_threads",
            Self::SettleSelf(_) => "settle_self",
            Self::StartThread(_) => "start_thread",
            Self::SendMessage(_) => "send_message",
            Self::ControlThread(_) => "control_thread",
            Self::PullRequest(_) => "pull_request",
            Self::Git(_) => "git",
            Self::CreateProject(_) => "create_project",
            Self::MoveThread(_) => "move_thread",
            Self::NotifyWhen(_) => "notify_when",
            Self::CancelNotify(_) => "cancel_notify",
            Self::RespondInput(_) => "respond_input",
            Self::RespondApproval(_) => "respond_approval",
            Self::ReportResult(_) => REPORT_RESULT,
            Self::Terminal(_) => "terminal",
            Self::DeleteThreads(_) => "delete_threads",
            Self::Rewind(_) => "rewind",
            Self::PatchSettings(_) => "patch_settings",
            Self::SetModelGuide(_) => SET_MODEL_GUIDE,
            Self::ListMachines(_) => LIST_MACHINES,
            Self::StartThreadOnMachine(_) => START_THREAD_ON_MACHINE,
        }
    }

    /// Whether the call answers per target, so a target that is unknown or closed to the
    /// caller skips that item instead of refusing the whole call.
    pub fn answers_per_target(&self) -> bool {
        matches!(
            self,
            Self::UpdateThreads(_)
                | Self::SendMessage(_)
                | Self::ControlThread(_)
                | Self::DeleteThreads(_)
                | Self::NotifyWhen(_)
        )
    }

    /// Threads the call acts on or reads, as named in its parameters.
    pub fn target_thread_ids(&self) -> Vec<&str> {
        match self {
            Self::GetThread(p) => vec![p.thread_id.as_str()],
            Self::Search(p) => p.thread_ids.iter().flatten().map(String::as_str).collect(),
            Self::Outline(p) => vec![p.thread_id.as_str()],
            Self::Read(p) => p
                .anchors
                .iter()
                .flatten()
                .map(String::as_str)
                .chain(p.from.as_deref())
                .chain(p.to.as_deref())
                .filter_map(|anchor| parse_anchor(anchor).map(|(thread, _)| thread))
                .chain(
                    p.cursor
                        .as_deref()
                        .and_then(|cursor| cursor.rsplit_once('#'))
                        .map(|(thread, _)| thread),
                )
                .collect(),
            Self::SearchInItem(p) => parse_anchor(&p.anchor)
                .map(|(thread, _)| vec![thread])
                .unwrap_or_default(),
            Self::FileHistory(p) => vec![p.thread_id.as_str()],
            Self::GetGit(p) => p.thread_id.iter().map(String::as_str).collect(),
            Self::GetPullRequest(p) => vec![p.thread_id.as_str()],
            Self::UpdateThreads(p) => p.changes.iter().map(|c| c.thread_id.as_str()).collect(),
            Self::SendMessage(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::ControlThread(p) => p.changes.iter().map(|c| c.thread_id.as_str()).collect(),
            Self::PullRequest(p) => vec![p.thread_id.as_str()],
            Self::Git(p) => vec![p.thread_id.as_str()],
            Self::MoveThread(p) => vec![p.thread_id.as_str()],
            Self::NotifyWhen(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::RespondInput(p) => vec![p.thread_id.as_str()],
            Self::RespondApproval(p) => vec![p.thread_id.as_str()],
            Self::Terminal(p) => vec![p.thread_id.as_str()],
            Self::DeleteThreads(p) => p.thread_ids.iter().map(String::as_str).collect(),
            Self::Rewind(p) => vec![p.thread_id.as_str()],
            Self::ListProjects(_)
            | Self::ListThreads(_)
            | Self::ListProviders(_)
            | Self::GetSettings(_)
            | Self::ReadTerminal(_)
            | Self::ReviewCandidates(_)
            | Self::SettleSelf(_)
            | Self::StartThread(_)
            | Self::CreateProject(_)
            | Self::CancelNotify(_)
            | Self::PatchSettings(_)
            | Self::SetModelGuide(_)
            | Self::ReportResult(_)
            | Self::ListMachines(_)
            | Self::StartThreadOnMachine(_) => Vec::new(),
        }
    }

    /// Put the calling thread's id wherever the call names it as [`OWN_THREAD`].
    pub fn name_caller(&mut self, caller: &str) {
        let ids: Vec<&mut String> = match self {
            Self::GetThread(p) => vec![&mut p.thread_id],
            Self::Search(p) => p.thread_ids.iter_mut().flatten().collect(),
            Self::Outline(p) => vec![&mut p.thread_id],
            Self::FileHistory(p) => vec![&mut p.thread_id],
            Self::GetGit(p) => p.thread_id.iter_mut().collect(),
            Self::GetPullRequest(p) => vec![&mut p.thread_id],
            Self::UpdateThreads(p) => p.changes.iter_mut().map(|c| &mut c.thread_id).collect(),
            Self::SendMessage(p) => p.thread_ids.iter_mut().collect(),
            Self::ControlThread(p) => p.changes.iter_mut().map(|c| &mut c.thread_id).collect(),
            Self::PullRequest(p) => vec![&mut p.thread_id],
            Self::Git(p) => vec![&mut p.thread_id],
            Self::MoveThread(p) => vec![&mut p.thread_id],
            Self::NotifyWhen(p) => p.thread_ids.iter_mut().collect(),
            Self::RespondInput(p) => vec![&mut p.thread_id],
            Self::RespondApproval(p) => vec![&mut p.thread_id],
            Self::Terminal(p) => vec![&mut p.thread_id],
            Self::DeleteThreads(p) => p.thread_ids.iter_mut().collect(),
            Self::Rewind(p) => vec![&mut p.thread_id],
            Self::Read(_)
            | Self::SearchInItem(_)
            | Self::ListProjects(_)
            | Self::ListThreads(_)
            | Self::ListProviders(_)
            | Self::GetSettings(_)
            | Self::ReadTerminal(_)
            | Self::ReviewCandidates(_)
            | Self::SettleSelf(_)
            | Self::StartThread(_)
            | Self::CreateProject(_)
            | Self::CancelNotify(_)
            | Self::PatchSettings(_)
            | Self::SetModelGuide(_)
            | Self::ReportResult(_)
            | Self::ListMachines(_)
            | Self::StartThreadOnMachine(_) => Vec::new(),
        };
        for id in ids {
            if id == OWN_THREAD {
                *id = caller.to_owned();
            }
        }
    }
}

#[derive(Debug)]
pub struct BrokerRequest {
    /// The calling thread, baked into its bearer token.
    pub thread_id: String,
    pub op: ControlOp,
    /// The machine of the group the call is for; `None` for this one.
    pub machine: Option<String>,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}

pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;

/// Answers a thread id with its current access; `None` for a thread the host does not know,
/// which is offered nothing.
pub type AccessResolver = Arc<dyn Fn(&str) -> Option<ThreadAccess> + Send + Sync>;

/// Tool names with the descriptions a connection listed them with.
pub type ListedTools = Vec<(String, String)>;

/// The resolver the runtime installs when it attaches the server, and the tools each
/// thread's latest connection was actually given. Until a resolver is set, and for a thread
/// it does not know, a connection is offered nothing.
#[derive(Clone, Default)]
pub struct AccessSource {
    resolver: Arc<Mutex<Option<AccessResolver>>>,
    offered: Arc<Mutex<HashMap<String, ListedTools>>>,
}

impl AccessSource {
    pub fn set(&self, resolver: AccessResolver) {
        *self.resolver.lock().unwrap() = Some(resolver);
    }

    pub fn resolve(&self, thread_id: &str) -> ThreadAccess {
        let resolver = self.resolver.lock().unwrap().clone();
        resolver
            .and_then(|resolve| resolve(thread_id))
            .unwrap_or_else(ThreadAccess::none)
    }

    /// The tools, with descriptions, the thread's latest connection listed; `None` before
    /// its provider first connected. Access changed since then shows here only once the
    /// provider connects again.
    pub fn offered(&self, thread_id: &str) -> Option<ListedTools> {
        self.offered.lock().unwrap().get(thread_id).cloned()
    }

    pub fn forget(&self, thread_id: &str) {
        self.offered.lock().unwrap().remove(thread_id);
    }

    fn record_offered(&self, thread_id: &str, tools: ListedTools) {
        self.offered
            .lock()
            .unwrap()
            .insert(thread_id.to_owned(), tools);
    }
}

pub struct ControlMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
    pub access: AccessSource,
}

pub fn start(host: &mut mcp_host::Host) -> ControlMcpServer {
    let url = host.url("/control");
    let (req_tx, req_rx) = async_channel::unbounded();
    let broker = Broker::new(
        req_tx,
        mcp_host::BrokerErrors {
            unavailable: "tcode control is not available",
            dropped: "tcode control dropped the request",
        },
    );
    let access = AccessSource::default();
    let resolver = access.clone();
    let tokens = TokenRegistry::new(move |thread_id| {
        tools::service(broker.clone(), thread_id, resolver.clone())
    });
    host.mount(mcp_host::route("/control", &tokens));
    log::info!("control-mcp: serving at {url}");
    ControlMcpServer {
        url,
        tokens,
        requests: req_rx,
        access,
    }
}
