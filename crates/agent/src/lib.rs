//! Provider-agnostic agent session layer.
//!
//! Each native provider module spawns its CLI as a child process,
//! speaks the provider-native transport, and normalizes everything
//! into the canonical [`AgentEvent`] stream. The UI only ever sees this module's
//! types; nothing provider-shaped leaks past this crate except [`ResumeCursor`],
//! which is intentionally opaque.

#[cfg(feature = "process")]
pub mod acp;
#[cfg(feature = "process")]
mod acp_session;
#[cfg(feature = "process")]
mod actor;
#[cfg(feature = "process")]
pub mod claude;
mod claude_context;
#[cfg(feature = "process")]
mod claude_goal;
mod claude_manifest;
#[cfg(feature = "process")]
mod claude_plugins;
#[cfg(feature = "process")]
mod clock;
#[cfg(feature = "process")]
pub use clock::{Clock, Timekeeper};
mod plugin_command;
#[cfg(feature = "process")]
mod source_plugins;
pub use plugin_command::plugin_op_command;
pub mod context_window;
#[cfg(not(feature = "process"))]
pub mod claude {
    pub use crate::claude_context::*;
}
#[cfg(feature = "process")]
mod cmd_shim;
pub mod harness;
pub use harness::{Endpoint, EndpointPreset, HarnessFacts, NativeMcpServer};
#[cfg(feature = "process")]
pub mod codex;
#[cfg(feature = "process")]
pub mod cursor;
#[cfg(feature = "process")]
pub mod grok;
#[cfg(feature = "process")]
pub mod native_mcp;
#[cfg(feature = "process")]
pub mod opencode;
#[cfg(feature = "process")]
pub mod pi;
#[cfg(feature = "process")]
pub use cmd_shim::CmdShim;
#[cfg(feature = "process")]
mod process;
#[cfg(feature = "process")]
pub use process::{SessionProcesses, command, stop_every_tree, tree_output};
#[cfg(feature = "process")]
mod subagent_tail;
mod tool_name;
pub use tool_name::ToolName;
mod tool_output;
pub use tool_output::ToolOutput;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
#[cfg(feature = "process")]
use smol::channel::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    ClaudeCode,
    Pi,
    OpenCode,
    Cursor,
    Grok,
    /// Any agent speaking the Agent Client Protocol. Which one is carried
    /// separately ([`SessionOptions::acp`]) so this stays `Copy`: the native
    /// providers keep their own clients, and ACP covers the rest of the
    /// ecosystem.
    Acp,
}

/// Work a harness runs in the background of a thread, as it names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: BackgroundKind,
    /// The call that started it, when the harness says which.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// When it started (unix ms): as the harness reports it when
    /// `started_reported`, else when the adapter first saw it running.
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub started_reported: bool,
}

/// What a piece of background work is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    Command,
    Agent,
    #[default]
    Other,
}

/// What a harness lets a client do with the background work it lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundControl {
    pub stop: BackgroundStop,
    /// The harness moves a running command or agent call into the
    /// background on request ([`SessionCommand::MoveToBackground`]).
    pub move_running: bool,
}

/// Which of its background work a harness stops on request
/// ([`SessionCommand::StopBackgroundTask`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStop {
    None,
    /// One piece at a time; a stop for all of it is one stop for each.
    Each,
    /// One piece at a time, or all of it in one request of its own.
    EachAndAll,
}

impl BackgroundStop {
    pub fn stops(self) -> bool {
        self != Self::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub supports_steering: bool,
    pub supports_fork: bool,
    /// Whether the harness forks through a finished turn itself
    /// ([`ForkPoint::Before`] the next prompt, [`ForkPoint::Tail`] from the
    /// last turn).
    pub fork_at: bool,
    /// What the harness's own rewind does ([`SessionCommand::Rewind`]), when
    /// it has one. A thread can withdraw it ([`AgentEvent::RewindAvailability`])
    /// and a preview narrows it for one message ([`AgentEvent::RewindPreviewed`]).
    pub rewind: Option<RewindControl>,
    /// The harness compacts its context on request ([`SessionCommand::Compact`]).
    pub manual_compaction: bool,
    /// How the harness's standing goal is driven ([`SessionCommand::SetGoal`]),
    /// when it has one.
    pub goals: Option<GoalControl>,
    pub per_turn_effort: bool,
    pub options_apply_live: bool,
    pub live_option_push: OptionIds,
    /// The options whose value in effect the harness reports
    /// ([`AgentEvent::ProviderOptions`]'s selections); of the others a client
    /// knows only what Tcode sent.
    pub confirmed_options: OptionIds,
    /// The harness lists its background work ([`AgentEvent::BackgroundTaskList`]),
    /// and what it lets a client do with it.
    pub background: Option<BackgroundControl>,
    /// The running process can be asked to take a new model and options
    /// ([`SessionCommand::ApplySettings`]) rather than restart for them.
    pub live_settings: bool,
    /// Whether this adapter can attach tcode's HTTP MCP servers to a session.
    /// Preview attachment rides the same capability as every other MCP server.
    pub mcp_servers: bool,
    /// The harness drives the desktop itself, so tcode's computer-use MCP
    /// server is not attached to its sessions.
    pub native_computer_use: bool,
    pub launch_args: bool,
    pub option_descriptors: OptionDescriptors,
    pub home_path: bool,
    pub trust_project_extensions: bool,
    pub plugin_management: PluginManagement,
    /// Where [`SessionOptions::instructions`] reach the model.
    pub instruction_channel: InstructionChannel,
    /// The CLI signs a profile in and out of its native account itself.
    pub account_login: bool,
    /// A session resumes under another account profile of this provider once
    /// [`transfer_session`] has copied its native files into that profile's home.
    pub session_transfer: bool,
    /// The same native session goes on in another working directory: resumed
    /// there ([`SessionOptions::cwd`]), or sent there while it runs
    /// ([`SessionCommand::MoveTo`]).
    pub session_move: bool,
}

/// Where a session started from [`SessionOptions::resume`] goes on instead of
/// where that session stands. A value the provider's [`Caps`] do not declare
/// fails the start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPoint {
    /// A new session holding all of the resumed one ([`Caps::supports_fork`],
    /// and [`Caps::fork_at`] from its last turn).
    Tail,
    /// A new session holding what came before the message of the restore
    /// point `checkpoint_id` ([`Caps::fork_at`]).
    Before { checkpoint_id: String },
    /// Not a fork: the branch a rewind left, by the `undo_token` of its
    /// [`AgentEvent::RewindCompleted`] ([`LaterHistory::Branch`]).
    Branch { token: String },
}

/// What a harness's own rewind does. Tcode offers exactly this and never
/// emulates a missing part with Git or transcript edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewindControl {
    /// The scopes it restores, in the order the panel offers them.
    pub modes: &'static [RewindMode],
    /// The conversation can be restored to before the thread's first turn.
    pub first_turn_conversation: bool,
    /// A steer (a message added to a running turn) is a target for the
    /// conversation alone; it has no restore point for files.
    pub steer_targets: bool,
    pub later_history: LaterHistory,
    pub preview: RewindPreviewKind,
    /// Files the agent changed by running commands are restored too, not only
    /// those its own edit tools wrote.
    pub restores_command_edits: bool,
}

impl RewindControl {
    /// Whether `mode` can restore turn `turn`, counted from the thread's first,
    /// or a steer inside it.
    pub fn offers(self, mode: RewindMode, turn: usize, steer: bool) -> bool {
        self.modes.contains(&mode)
            && (!steer || (self.steer_targets && mode == RewindMode::Conversation))
            && (turn > 0 || steer || self.first_turn_conversation || !mode.includes_conversation())
    }
}

/// What becomes of the conversation after a rewind's target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaterHistory {
    /// The later turns leave the harness's conversation for good.
    #[default]
    Truncated,
    /// The later turns are hidden, and the rewind can be undone until the
    /// next message is sent, which makes it final.
    HiddenUntilNextSend,
    /// The conversation goes on in a new branch; the original is kept and can
    /// be returned to.
    Branch,
}

/// What a harness can show of a rewind before it is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindPreviewKind {
    None,
    /// The files a restore would change, with line counts where it has them.
    Files,
    /// The files a restore would change, and those changed outside the agent
    /// since, which the restore would overwrite.
    Conflicts,
}

/// What a harness lets a client do with its standing goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalControl {
    /// The goal can be paused and resumed without clearing it.
    pub pause: bool,
    /// A change applies out of band, even while a turn runs. Otherwise the
    /// change is a turn of its own, as Claude Code's `/goal` slash command is.
    pub live: bool,
}

/// The provider's own channel for instructions a host gives the model alongside the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionChannel {
    /// Part of the session's system instructions, set at every start, on creation and resume
    /// alike (Claude Code's appended system prompt, an OpenCode session instruction entry).
    SessionAppend,
    /// Developer context Codex re-sends when it changes; restored after a compaction.
    DeveloperContext,
    /// A content block of its own after the user's text on every prompt that is not a native
    /// `/command`, while the session attached Tcode's MCP servers.
    PerPromptUserBlock,
    None,
}

/// A set of option ids, as a provider fact names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionIds {
    All,
    AllBut(&'static [&'static str]),
    Only(&'static [&'static str]),
    None,
}

impl OptionIds {
    pub fn supports(self, option_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::AllBut(option_ids) => !option_ids.contains(&option_id),
            Self::Only(option_ids) => option_ids.contains(&option_id),
            Self::None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionDescriptors {
    Wire,
    Catalog,
}

impl ProviderKind {
    /// The natively maintained providers, in the order provider lists present
    /// them.
    pub const NATIVE: [ProviderKind; 6] = [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Pi,
        ProviderKind::OpenCode,
        ProviderKind::Cursor,
        ProviderKind::Grok,
    ];

    /// Provider behavior consumed by runtime and UI policy.
    ///
    /// This is deliberately one exhaustive table so adding a provider forces a
    /// review of every capability fact in one place.
    pub fn caps(&self) -> Caps {
        match self {
            ProviderKind::ClaudeCode => Caps {
                supports_steering: true,
                supports_fork: true,
                // `--resume-session-at` keeps the entries up to the message the
                // prompt was answering; the first prompt has none.
                fork_at: true,
                rewind: Some(RewindControl {
                    modes: &[
                        RewindMode::FilesAndConversation,
                        RewindMode::Conversation,
                        RewindMode::Files,
                    ],
                    // A cut before the first prompt persists as a leaf-less
                    // `last-prompt` entry, which a resume reads as an empty
                    // conversation (2.1.318).
                    first_turn_conversation: true,
                    // A steer has no file checkpoint ("No code restore").
                    steer_targets: true,
                    later_history: LaterHistory::Truncated,
                    // `rewind_files` with `dry_run`.
                    preview: RewindPreviewKind::Files,
                    // `rewind_conversation`'s `interrupt_if_running`.
                    // File history tracks the edit tools' writes only.
                    restores_command_edits: false,
                }),
                manual_compaction: true,
                goals: Some(GoalControl {
                    pause: false,
                    live: false,
                }),
                per_turn_effort: false,
                options_apply_live: false,
                live_option_push: OptionIds::None,
                // `system/init` and `system/status` carry `permissionMode`, the
                // init and each `result` `fast_mode_state`, and `get_settings`
                // the effort it `applied` (2.1.322). Thinking is in none of
                // them, and the window only as the model's, not the selection.
                confirmed_options: OptionIds::Only(&[
                    "permissionMode",
                    "fastMode",
                    EFFORT_OPTION_ID,
                ]),
                // `background_tasks_changed` and `stop_task`, shells and
                // subagents alike (2.1.318); `stop_task` takes one id. The
                // `background_tasks` control request with a `tool_use_id` is
                // Ctrl+B for that call (2.1.319).
                background: Some(BackgroundControl {
                    stop: BackgroundStop::Each,
                    move_running: true,
                }),
                // `set_model` and `apply_flag_settings` (2.1.318).
                live_settings: true,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Enable,
                        PluginActionKind::Disable,
                        PluginActionKind::Update,
                    ],
                    marketplaces: true,
                    scopes: &[PluginScope::User, PluginScope::Project, PluginScope::Local],
                    source_install: false,
                    unmanaged: None,
                },
                instruction_channel: InstructionChannel::SessionAppend,
                account_login: true,
                session_transfer: true,
                session_move: false,
            },
            ProviderKind::Codex => Caps {
                supports_steering: true,
                supports_fork: true,
                // `thread/fork {lastTurnId}`, the turn before the prompt's: the
                // first prompt has none.
                fork_at: true,
                // `thread/revert`: conversation only, never files (its undo
                // was removed); a turn it reverts while running is stopped.
                rewind: Some(RewindControl {
                    modes: &[RewindMode::Conversation],
                    first_turn_conversation: true,
                    steer_targets: false,
                    later_history: LaterHistory::Truncated,
                    preview: RewindPreviewKind::None,
                    restores_command_edits: false,
                }),
                manual_compaction: true,
                goals: Some(GoalControl {
                    pause: true,
                    live: true,
                }),
                per_turn_effort: true,
                options_apply_live: false,
                live_option_push: OptionIds::None,
                // The `thread/start|resume|fork` answer and
                // `thread/settings/updated` carry the effort, the service tier
                // and the permissions (0.167.1). The window comes only as
                // `modelContextWindow`, the part of it the model may use.
                confirmed_options: OptionIds::Only(&[
                    EFFORT_OPTION_ID,
                    "serviceTier",
                    "permissions",
                ]),
                // `thread/backgroundTerminals/*`: `terminate` and `clean`,
                // experimental (0.167.0).
                background: Some(BackgroundControl {
                    stop: BackgroundStop::EachAndAll,
                    move_running: false,
                }),
                live_settings: true,
                mcp_servers: true,
                // `codex features list` reports `computer_use stable` (0.159.3).
                native_computer_use: true,
                launch_args: false,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Enable,
                        PluginActionKind::Disable,
                    ],
                    marketplaces: true,
                    scopes: &[PluginScope::User],
                    source_install: false,
                    unmanaged: None,
                },
                instruction_channel: InstructionChannel::DeveloperContext,
                account_login: true,
                session_transfer: false,
                // `thread/resume {cwd}`, and `turn/start {cwd}` for the turn
                // after a move and those after it (0.167.0).
                session_move: true,
            },
            ProviderKind::Acp => Caps {
                supports_steering: false,
                supports_fork: false,
                fork_at: false,
                rewind: None,
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: OptionIds::All,
                // Every option is the agent's own, with its `currentValue`.
                confirmed_options: OptionIds::All,
                background: None,
                live_settings: false,
                // ACP support is negotiated per agent; capable agents receive
                // every tcode HTTP MCP registration in session/new or load.
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement::NONE,
                instruction_channel: InstructionChannel::PerPromptUserBlock,
                account_login: false,
                session_transfer: false,
                session_move: false,
            },
            ProviderKind::Pi => Caps {
                supports_steering: true,
                supports_fork: false,
                // RPC `fork`, in a process of the fork's own so the source's
                // running turn, which that request aborts, is left alone.
                fork_at: true,
                // RPC `fork` goes on in a new session file from before a user
                // message and keeps the old one, which `switch_session` returns
                // to; files are not pi's to restore.
                rewind: Some(RewindControl {
                    modes: &[RewindMode::Conversation],
                    first_turn_conversation: true,
                    steer_targets: false,
                    later_history: LaterHistory::Branch,
                    preview: RewindPreviewKind::None,
                    restores_command_edits: false,
                }),
                manual_compaction: true,
                goals: None,
                per_turn_effort: false,
                options_apply_live: false,
                live_option_push: OptionIds::Only(&[EFFORT_OPTION_ID]),
                // `get_state`'s `thinkingLevel` and `thinking_level_changed`
                // (1.5.2).
                confirmed_options: OptionIds::Only(&[EFFORT_OPTION_ID]),
                background: None,
                live_settings: true,
                mcp_servers: false,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: true,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Update,
                    ],
                    scopes: &[PluginScope::User, PluginScope::Project],
                    source_install: true,
                    ..PluginManagement::NONE
                },
                instruction_channel: InstructionChannel::None,
                account_login: false,
                session_transfer: false,
                session_move: false,
            },
            ProviderKind::OpenCode => Caps {
                supports_steering: true,
                supports_fork: true,
                // `POST /api/session/{id}/fork {before}`.
                fork_at: true,
                // A revert restores the conversation, with or without the
                // files its steps changed; there is no files-only revert. It
                // stays staged until the next prompt or compaction commits it,
                // and the files come back from snapshots of the whole
                // worktree, whatever changed them.
                rewind: Some(RewindControl {
                    modes: &[RewindMode::FilesAndConversation, RewindMode::Conversation],
                    first_turn_conversation: true,
                    steer_targets: false,
                    later_history: LaterHistory::HiddenUntilNextSend,
                    preview: RewindPreviewKind::None,
                    restores_command_edits: true,
                }),
                manual_compaction: true,
                goals: None,
                per_turn_effort: true,
                options_apply_live: false,
                live_option_push: OptionIds::Only(&["agent"]),
                // The session's and each step's `model.variant` and `agent`
                // (2.2.7); the permission mode is the adapter's own answer to
                // each `permission.asked`.
                confirmed_options: OptionIds::Only(&[EFFORT_OPTION_ID, "agent", "permission_mode"]),
                background: None,
                live_settings: true,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: false,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Update,
                    ],
                    scopes: &[PluginScope::User],
                    source_install: true,
                    ..PluginManagement::NONE
                },
                instruction_channel: InstructionChannel::SessionAppend,
                account_login: false,
                session_transfer: false,
                // `POST /session/{id}/move` (2.2.6), which the model's own
                // `session_move` tool also calls.
                session_move: true,
            },
            ProviderKind::Cursor => Caps {
                supports_steering: false,
                supports_fork: false,
                fork_at: false,
                rewind: None,
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: OptionIds::All,
                // The approval mode is how Tcode launched the agent, which
                // reports none.
                confirmed_options: OptionIds::AllBut(&["approvalMode"]),
                background: None,
                live_settings: false,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    unmanaged: Some(UnmanagedPlugins {
                        kind: UnmanagedPluginsKind::EditorOnly,
                    }),
                    ..PluginManagement::NONE
                },
                instruction_channel: InstructionChannel::PerPromptUserBlock,
                account_login: false,
                session_transfer: false,
                session_move: false,
            },
            ProviderKind::Grok => Caps {
                supports_steering: true,
                supports_fork: true,
                // `_x.ai/session/fork` with the prompt's number.
                fork_at: true,
                // `_x.ai/rewind/execute`: a prompt's number, in every mode its
                // /rewind offers. The files are the snapshots its own tools
                // took; a dry run lists them and those changed since.
                rewind: Some(RewindControl {
                    modes: &[
                        RewindMode::FilesAndConversation,
                        RewindMode::Conversation,
                        RewindMode::Files,
                    ],
                    first_turn_conversation: true,
                    steer_targets: false,
                    later_history: LaterHistory::Truncated,
                    preview: RewindPreviewKind::Conflicts,
                    restores_command_edits: false,
                }),
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: OptionIds::All,
                // `--permission-mode` is a launch argument the agent does not
                // report back.
                confirmed_options: OptionIds::AllBut(&["permissionMode"]),
                // `background_tasks` lists them; nothing stops one.
                background: Some(BackgroundControl {
                    stop: BackgroundStop::None,
                    move_running: false,
                }),
                live_settings: false,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Update,
                    ],
                    marketplaces: true,
                    scopes: &[PluginScope::User],
                    source_install: false,
                    unmanaged: None,
                },
                instruction_channel: InstructionChannel::PerPromptUserBlock,
                account_login: false,
                session_transfer: false,
                session_move: false,
            },
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "Codex",
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "OpenCode",
            ProviderKind::Cursor => "Cursor",
            ProviderKind::Grok => "Grok",
            ProviderKind::Acp => "ACP agent",
        }
    }
}

/// Which native plugin operations an adapter drives. Whether one is offered
/// for a given entry is computed per catalog ([`ProviderPluginEntry::actions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginManagement {
    pub actions: &'static [PluginActionKind],
    pub marketplaces: bool,
    /// The native scopes plugins are installed or enabled in. User scope is
    /// each account's own configuration directory; project and local scopes
    /// are files in the project, shared by every account.
    pub scopes: &'static [PluginScope],
    /// There is no catalog: an install takes a package source a person
    /// types.
    pub source_install: bool,
    /// Set for a harness that extends itself natively in a way Tcode does
    /// not manage.
    pub unmanaged: Option<UnmanagedPlugins>,
}

impl PluginManagement {
    pub const NONE: Self = Self {
        actions: &[],
        marketplaces: false,
        scopes: &[],
        source_install: false,
        unmanaged: None,
    };

    /// Whether Tcode lists and changes this harness's plugins.
    pub fn manages(&self) -> bool {
        !self.actions.is_empty() || self.marketplaces
    }

    pub fn supports(&self, action: PluginActionKind) -> bool {
        self.actions.contains(&action)
    }
}

/// How a harness Tcode does not manage plugins for extends itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnmanagedPlugins {
    pub kind: UnmanagedPluginsKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmanagedPluginsKind {
    /// Plugins installed only from the harness's own editor.
    EditorOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionKind {
    Install,
    Uninstall,
    Enable,
    Disable,
    Update,
}

/// A native settings scope a plugin is installed or enabled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    User,
    Project,
    Local,
    Managed,
    Session,
}

/// A yes/no fact the native CLI may not report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    No,
    #[default]
    Unknown,
}

impl From<bool> for Tri {
    fn from(value: bool) -> Self {
        if value { Tri::Yes } else { Tri::No }
    }
}

/// One native operation the host computed as available for an entry in the
/// catalog's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginAction {
    Install { scope: PluginScope },
    Uninstall { scope: PluginScope },
    Enable { scope: PluginScope },
    Disable { scope: PluginScope },
    Update { scope: PluginScope },
}

impl PluginAction {
    pub fn kind(self) -> PluginActionKind {
        match self {
            Self::Install { .. } => PluginActionKind::Install,
            Self::Uninstall { .. } => PluginActionKind::Uninstall,
            Self::Enable { .. } => PluginActionKind::Enable,
            Self::Disable { .. } => PluginActionKind::Disable,
            Self::Update { .. } => PluginActionKind::Update,
        }
    }

    pub fn scope(self) -> PluginScope {
        match self {
            Self::Install { scope }
            | Self::Uninstall { scope }
            | Self::Enable { scope }
            | Self::Disable { scope }
            | Self::Update { scope } => scope,
        }
    }
}

/// A marketplace-level native operation available in the catalog's context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MarketplaceAction {
    Add,
    /// Removing the marketplace also uninstalls these installed plugins in
    /// every scope, so the host asks before running it.
    Remove {
        marketplace: String,
        uninstalls: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSourceKind {
    Official,
    ThirdParty,
    LocalPath,
    Git,
    Npm,
    Command,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSource {
    #[serde(default)]
    pub marketplace: Option<String>,
    pub kind: PluginSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallation {
    pub scope: PluginScope,
    /// Where the installed copy lives on the host.
    #[serde(default)]
    pub location: Option<PathBuf>,
    #[serde(default)]
    pub version: Option<String>,
    /// The scope's own enablement where the native CLI reports it; the
    /// effective value is [`ProviderPluginEntry::enabled`].
    #[serde(default)]
    pub scope_enabled: Option<bool>,
    /// A project or local installation of another project than the one the
    /// catalog was listed from: the CLI lists those home-wide, but this
    /// project's scope does not hold it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub other_project: bool,
}

/// Components a plugin declares, by name. A `None` field was not enumerated,
/// which is not the same as declaring none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredComponents {
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub agents: Option<Vec<String>>,
    /// Hook event names.
    #[serde(default)]
    pub hooks: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<String>>,
    /// Hook event names the native CLI reports as not yet trusted (Codex
    /// skips such hooks until they are reviewed natively). `None` when the
    /// provider has no hook-trust state.
    #[serde(default)]
    pub untrusted_hooks: Option<Vec<String>>,
    #[serde(default)]
    pub lsp_servers: Option<Vec<String>>,
    #[serde(default)]
    pub apps: Option<Vec<String>>,
}

/// One plugin in a provider's native catalog: installed in any scope, offered
/// by a marketplace, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginEntry {
    /// The native id the CLI accepts, `name@marketplace`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// A newer version the native catalog reports for an installed plugin.
    /// `None` when it reports none, including a CLI that never tells.
    #[serde(default)]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub source: PluginSource,
    #[serde(default)]
    pub installations: Vec<PluginInstallation>,
    /// Effective enablement for the catalog's context directory.
    #[serde(default)]
    pub enabled: Tri,
    #[serde(default)]
    pub declared: Option<DeclaredComponents>,
    /// The native estimate of what the plugin adds to every session's
    /// context, as the CLI prints it (`~32 tok`).
    #[serde(default)]
    pub always_on: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub actions: Vec<PluginAction>,
    /// Read-only `(key, native text)` facts, keyed by stable identifiers the
    /// client localizes.
    #[serde(default)]
    pub diagnostics: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginMarketplace {
    pub name: String,
    /// The repository, URL or path the marketplace was added from.
    pub source: String,
    pub kind: PluginSourceKind,
    #[serde(default)]
    pub location: Option<PathBuf>,
}

/// A provider's native plugin catalog as seen from one directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginListing {
    pub entries: Vec<ProviderPluginEntry>,
    pub marketplaces: Vec<ProviderPluginMarketplace>,
    pub marketplace_actions: Vec<MarketplaceAction>,
    /// Native errors for parts of the catalog that could not be read, such
    /// as a marketplace that failed to load.
    pub errors: Vec<String>,
}

/// Where and as whom a plugin-management command runs: the profile's CLI and
/// environment, and the directory whose project scopes it addresses.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub binary_path: Option<PathBuf>,
    pub launch_env: LaunchEnv,
    pub cwd: PathBuf,
    /// False when `cwd` is only a neutral directory, so project and local
    /// scopes are not offered.
    pub project: bool,
}

/// One native plugin mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOp {
    Install {
        id: String,
        scope: PluginScope,
        /// The `sha256` of a command the user accepted from an earlier
        /// [`PluginOpOutcome::AcceptCommand`].
        accept_command: Option<String>,
    },
    Uninstall {
        id: String,
        scope: PluginScope,
    },
    SetEnabled {
        id: String,
        scope: PluginScope,
        enabled: bool,
    },
    Update {
        id: String,
        scope: PluginScope,
        accept_command: Option<String>,
    },
    AddMarketplace {
        source: String,
    },
    RemoveMarketplace {
        name: String,
    },
}

/// What a native plugin mutation reported. Native failures are errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOpOutcome {
    Done {
        diagnostics: Vec<(String, String)>,
    },
    /// The CLI refused to run a marketplace-declared command until a person
    /// accepts exactly this command.
    AcceptCommand(CommandAcceptance),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAcceptance {
    pub command: String,
    pub sha256: String,
    #[serde(default)]
    pub mode: Option<String>,
    /// The CLI's own explanation.
    pub native_text: String,
}

/// List a provider's native plugins and marketplaces as seen from `context`.
#[cfg(feature = "process")]
pub async fn list_plugins(
    provider: ProviderKind,
    context: &PluginContext,
) -> Result<PluginListing, AgentError> {
    match provider {
        ProviderKind::ClaudeCode => claude_plugins::list(context).await,
        ProviderKind::Codex => codex::plugins::list(context).await,
        ProviderKind::Grok => grok::plugins::list(context).await,
        ProviderKind::Pi => source_plugins::list_pi(context).await,
        ProviderKind::OpenCode => source_plugins::list_opencode(context).await,
        ProviderKind::Cursor | ProviderKind::Acp => Err(no_plugin_management(provider)),
    }
}

/// Run one native plugin mutation in `context`.
#[cfg(feature = "process")]
pub async fn run_plugin_op(
    provider: ProviderKind,
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    match provider {
        ProviderKind::ClaudeCode => claude_plugins::run(context, op).await,
        ProviderKind::Codex => codex::plugins::run(context, op).await,
        ProviderKind::Grok => grok::plugins::run(context, op).await,
        ProviderKind::Pi => source_plugins::run_pi(context, op).await,
        ProviderKind::OpenCode => source_plugins::run_opencode(context, op).await,
        ProviderKind::Cursor | ProviderKind::Acp => Err(no_plugin_management(provider)),
    }
}

/// Copy the native session `resume` names out of the home a profile launched
/// with `from` keeps it in, into the one `to` launches with, so that profile
/// resumes the same session. Only a provider with [`Caps::session_transfer`]
/// can. Blocking.
#[cfg(feature = "process")]
pub fn transfer_session(
    provider: ProviderKind,
    from: &LaunchEnv,
    to: &LaunchEnv,
    resume: &ResumeCursor,
) -> std::io::Result<()> {
    match provider {
        ProviderKind::ClaudeCode => claude::transfer_session(from, to, resume),
        ProviderKind::Codex
        | ProviderKind::Pi
        | ProviderKind::OpenCode
        | ProviderKind::Cursor
        | ProviderKind::Grok
        | ProviderKind::Acp => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "{} cannot carry a session to another profile",
                provider.display_name()
            ),
        )),
    }
}

/// A native failure in the CLI's own words.
#[cfg(feature = "process")]
fn native_message(error: &AgentError) -> String {
    match error {
        AgentError::Provider(message) => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(feature = "process")]
fn no_plugin_management(provider: ProviderKind) -> AgentError {
    AgentError::Provider(format!(
        "{} has no plugin management",
        provider.display_name()
    ))
}

/// Registry agents never offered for new sessions because they duplicate a
/// native integration.
pub const HIDDEN_ACP_AGENT_IDS: [&str; 7] = [
    "claude-acp",
    "codex-acp",
    "pi-acp",
    "opencode",
    "opencode-v2",
    "cursor",
    "grok-build",
];

/// One ACP agent a session can run: its registry identity plus how to launch it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpAgent {
    /// Registry id (`"gemini"`, `"cursor"`, …), or a user-chosen id for a custom agent.
    pub id: String,
    pub name: String,
    pub launch: AcpLaunch,
}

/// How to start an ACP agent process (ACP is JSON-RPC over the child's stdio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpLaunch {
    /// Registry `npx` distribution: `npm exec --yes -- <package> <args…>`.
    Npx {
        package: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Registry `binary` distribution, already downloaded and extracted.
    Binary {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// A user-defined command (the escape hatch for agents not in the registry).
    Custom {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
}

/// Provider-shaped opaque state needed to resume a session later
/// (Codex: `{"thread_id": ...}`; Claude/pi/OpenCode use their native ids).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeCursor(pub serde_json::Value);

impl ResumeCursor {
    #[cfg(feature = "process")]
    pub(crate) fn str_field(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .find_map(|key| self.0.get(*key).and_then(serde_json::Value::as_str))
    }
}

#[cfg(feature = "process")]
pub(crate) fn processed_tokens(usage: TokenUsage) -> u64 {
    usage.used_tokens.unwrap_or_else(|| {
        usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0))
            .saturating_add(usage.cached_input_tokens.unwrap_or(0))
    })
}

#[cfg(feature = "process")]
pub(crate) fn start_mapped_turn(
    prefix: &str,
    counter: &mut u64,
    current: &mut Option<String>,
    usage: &mut Option<TokenUsage>,
    failed: &mut bool,
) -> Vec<AgentEvent> {
    if current.is_some() {
        return Vec::new();
    }
    *counter += 1;
    let turn_id = format!("{prefix}-turn-{counter}");
    *current = Some(turn_id.clone());
    *usage = None;
    *failed = false;
    vec![AgentEvent::TurnStarted { turn_id }]
}

#[cfg(feature = "process")]
pub(crate) fn complete_mapped_turn(
    current: &mut Option<String>,
    interrupted: &mut bool,
    failed: bool,
    usage: Option<TokenUsage>,
) -> Vec<AgentEvent> {
    let Some(turn_id) = current.take() else {
        return Vec::new();
    };
    let status = if *interrupted {
        TurnStatus::Interrupted
    } else if failed {
        TurnStatus::Failed
    } else {
        TurnStatus::Completed
    };
    *interrupted = false;
    vec![AgentEvent::TurnCompleted {
        turn_id,
        status,
        usage,
    }]
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Provider-native model id; `None` = provider default.
    pub model: Option<String>,
    pub resume: Option<ResumeCursor>,
    /// Go on somewhere other than where the resumed session stands. This is
    /// meaningful only when `resume` is present.
    pub fork: Option<ForkPoint>,
    /// Override for the CLI binary; `None` = resolve from PATH.
    pub binary_path: Option<PathBuf>,
    /// Chosen values for the provider's [`OptionDescriptor`]s (permission,
    /// reasoning effort,
    /// context window, service tier, fast mode, thinking, …). Each provider
    /// reads the ids it understands and ignores the rest.
    pub option_selections: Vec<OptionSelection>,
    /// Enabled tcode HTTP MCP servers to register with this session.
    pub mcp_servers: Vec<McpRegistration>,
    /// Per-provider environment (Settings → Harnesses): extra variables merged
    /// into the child's environment, plus the config-directory override. See
    /// [`LaunchEnv`].
    pub launch_env: LaunchEnv,
    /// Extra CLI arguments appended at spawn for providers whose settings card
    /// exposes "Launch arguments".
    pub extra_args: Vec<String>,
    /// Which ACP agent to launch. Required when `provider == ProviderKind::Acp`,
    /// ignored (and `None`) for native providers.
    pub acp: Option<AcpAgent>,
    /// Tcode's instructions for the model, delivered through the provider's
    /// [`Caps::instruction_channel`]; never part of a user turn's text.
    pub instructions: Option<String>,
    /// Where the session's harness processes are held, so its starter can
    /// stop them whatever the session is waiting on.
    #[cfg(feature = "process")]
    pub processes: SessionProcesses,
    /// The worktrees Tcode made for the thread, which a resumed session may
    /// report it works in.
    pub worktrees: Vec<PathBuf>,
    /// The host's time, which the session's waits on its harness read.
    #[cfg(feature = "process")]
    pub clock: Clock,
}

/// The per-provider environment configured in Settings → Harnesses, applied to
/// every child process we spawn for that provider (sessions, `list_models`, and
/// version/auth probes alike).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv {
    /// Extra `KEY=VALUE` pairs merged into the child's environment. Later
    /// entries win, and these override anything inherited from the parent.
    pub env: Vec<(String, String)>,
    /// Config-directory override. Provider-specific: Claude gets
    /// `CLAUDE_CONFIG_DIR`, Codex gets `CODEX_HOME`, pi gets
    /// `PI_CODING_AGENT_DIR`, Cursor gets `CURSOR_CONFIG_DIR` and
    /// `CURSOR_DATA_DIR` and Grok gets `GROK_HOME`.
    /// OpenCode has no supported single-directory override, so its value is
    /// ignored. Cursor reads its login from `$XDG_CONFIG_HOME/cursor` or
    /// `~/.config/cursor` whatever these say; moving either would also move
    /// the configuration of every tool the agent runs (`gh`, git), so Cursor
    /// profiles share one login.
    pub home: Option<PathBuf>,
}

impl LaunchEnv {
    /// The complete list of variables to set on a child of `provider`: the
    /// configured pairs, plus the provider's home variable when an override is
    /// set (appended last, so it wins over an equivalent hand-written pair).
    pub fn pairs(&self, provider: ProviderKind) -> Vec<(String, String)> {
        let mut pairs = self.env.clone();
        if let Some(home) = &self.home {
            let keys: &[&str] = match provider {
                ProviderKind::ClaudeCode => &["CLAUDE_CONFIG_DIR"],
                ProviderKind::Codex => &["CODEX_HOME"],
                ProviderKind::Pi => &["PI_CODING_AGENT_DIR"],
                ProviderKind::Cursor => &["CURSOR_CONFIG_DIR", "CURSOR_DATA_DIR"],
                ProviderKind::Grok => &["GROK_HOME"],
                ProviderKind::OpenCode => &[],
                // ACP agents carry their own env in the launch recipe; there is
                // no protocol-level home concept to override.
                ProviderKind::Acp => &[],
            };
            for key in keys {
                pairs.push(((*key).to_string(), home.to_string_lossy().into_owned()));
            }
        }
        pairs
    }

    /// The path `key` names in a child of `provider` launched with this
    /// environment: the last pair that sets it, else this process's own.
    pub fn child_path(&self, provider: ProviderKind, key: &str) -> Option<PathBuf> {
        self.pairs(provider)
            .into_iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| std::env::var_os(key).map(PathBuf::from))
    }

    /// The home directory a child of `provider` launched with this
    /// environment resolves `~` against: the `HOME` (`USERPROFILE` on
    /// Windows) it is given, else the account's, as the CLIs' own lookups
    /// fall back to it.
    #[cfg(feature = "process")]
    pub fn child_home(&self, provider: ProviderKind) -> Option<PathBuf> {
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        self.child_path(provider, key).or_else(dirs::home_dir)
    }
}

/// Connection details for the tcode preview MCP server (a streamable-HTTP
/// endpoint on loopback, guarded by a bearer token). Provider-agnostic; each
/// provider maps it onto its native MCP-server config shape.
#[derive(Debug, Clone)]
pub struct McpRegistration {
    /// MCP server name (and tool namespace).
    pub name: String,
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token presented on every request (`Authorization: Bearer <token>`).
    pub bearer_token: String,
}

impl McpRegistration {
    pub const SERVER_NAME_PREVIEW: &'static str = "tcode_preview";
    pub const SERVER_NAME_COMPUTER_USE: &'static str = "tcode_computer_use";
    pub const SERVER_NAME_PULL_REQUESTS: &'static str = "tcode_pull_requests";
    pub const SERVER_NAME_CONTROL: &'static str = "tcode_control";

    /// Codex `-c` override value: an inline TOML table for a streamable-HTTP MCP
    /// server. Codex rejects a literal `bearer_token` for HTTP, so the token
    /// rides in `http_headers.Authorization` instead (verified against
    /// codex `config/src/mcp_types.rs`). Returns the full `key=value` argument.
    pub fn codex_config_override(&self) -> String {
        format!(
            "mcp_servers.{name}={{url=\"{url}\",http_headers={{Authorization=\"Bearer {token}\"}}}}",
            name = self.name,
            url = self.url,
            token = self.bearer_token,
        )
    }
}

/// Claude Code `--mcp-config` JSON containing every supplied registration.
pub fn claude_mcp_config_json<'a>(
    registrations: impl IntoIterator<Item = &'a McpRegistration>,
) -> String {
    let servers: serde_json::Map<String, serde_json::Value> = registrations
        .into_iter()
        .map(|registration| {
            (
                registration.name.clone(),
                serde_json::json!({
                    "type": "http",
                    "url": registration.url,
                    "headers": {
                        "Authorization": format!("Bearer {}", registration.bearer_token),
                    }
                }),
            )
        })
        .collect();
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// One model a provider offers, with its selectable options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String, // provider-native id sent on the wire
    pub display_name: String,
    pub is_default: bool,
    pub options: Vec<OptionDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OptionDescriptor {
    Select {
        id: String,
        label: String,
        options: Vec<SelectOption>,
        default_value: Option<String>,
        #[serde(default)]
        role: OptionRole,
        #[serde(default)]
        apply: ApplyTiming,
        #[serde(default)]
        recommended: Option<String>,
        #[serde(default)]
        permissive: Option<String>,
        /// The smallest value the harness takes beyond the presets, for a
        /// select that also takes one of its own (a context window).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum: Option<u64>,
    },
    Boolean {
        id: String,
        label: String,
        default_value: bool,
        #[serde(default)]
        role: OptionRole,
        #[serde(default)]
        apply: ApplyTiming,
        #[serde(default)]
        recommended: Option<bool>,
        #[serde(default)]
        permissive: Option<bool>,
    },
}

/// What a control is, as its provider says: a client places and draws it by
/// this, never by its id. Carries no permission semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionRole {
    /// A trait of the model with nothing more said about it.
    #[default]
    Model,
    /// What runs without asking the user.
    Permission,
    /// How hard the model reasons: a select of levels, least first.
    Effort,
    /// The context window the session runs at: a select of presets whose
    /// values follow [`context_window::parse_tokens`], which also takes a
    /// custom size up to the largest preset.
    ContextWindow,
    /// Fast mode: a boolean that is on when fast, or a select whose fast
    /// choices name their [`Speed`].
    Speed,
    /// Extended thinking, on or off.
    Thinking,
}

/// How fast a choice of an [`OptionRole::Speed`] select runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    Fast,
    Ultrafast,
}

/// What a harness's permission settings amount to, where it reports them as
/// parts rather than as one of its named values. `None` is a part the
/// harness did not report, or reported in a form Tcode does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSummary {
    /// The named permission profile the settings come from.
    pub profile: Option<String>,
    pub sandbox: Option<PermissionSandbox>,
    pub approval: Option<ApprovalPolicy>,
    pub reviewer: Option<ApprovalReviewer>,
}

/// Where commands run (Codex `SandboxPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSandbox {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
    External,
}

/// When the harness stops to ask (Codex `AskForApproval`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    OnRequest,
    Never,
    Untrusted,
    Granular,
}

/// Who answers when it asks (Codex `ApprovalsReviewer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewer {
    User,
    AutoReview,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyTiming {
    #[default]
    Live,
    NextTurn,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// Present when this value cannot be selected; explains why.
    #[serde(default)]
    pub unavailable: Option<String>,
    /// How fast this choice of an [`OptionRole::Speed`] select runs; `None`
    /// for a choice that is not fast.
    #[serde(default)]
    pub speed: Option<Speed>,
    /// The parts of a permission value the harness reported as parts, in
    /// place of a description.
    #[serde(default)]
    pub summary: Option<PermissionSummary>,
    /// Who settles what the harness would stop to ask under this value of
    /// its permission control; `None` for a value of any other option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalClass>,
}

type PermissionTable = (
    &'static str,
    ApplyTiming,
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str, &'static str, ApprovalClass)],
);

/// Each provider's native permission control: its option id, when a change
/// applies, its default, recommended and most permissive values, and every
/// value with its label, description and [`ApprovalClass`].
fn permission_table(provider: ProviderKind) -> Option<PermissionTable> {
    Some(match provider {
        ProviderKind::ClaudeCode => (
            "permissionMode",
            ApplyTiming::Live,
            "default",
            "auto",
            "bypassPermissions",
            &[
                (
                    "default",
                    "Manual",
                    "Reads and built-in read-only commands run without asking; edits, other commands and network access ask you.",
                    ApprovalClass::Manual,
                ),
                (
                    "acceptEdits",
                    "Accept edits",
                    "Reads, file edits and common filesystem commands run without asking; other commands ask.",
                    ApprovalClass::Manual,
                ),
                (
                    "auto",
                    "Auto",
                    "A background safety classifier reviews each action; safe ones run, risky ones are blocked or ask. Unavailable when the account, model or organization does not support it.",
                    ApprovalClass::Reviewed,
                ),
                (
                    "dontAsk",
                    "Don't ask",
                    "Only pre-approved tools run; anything that would ask is denied instead.",
                    ApprovalClass::Reviewed,
                ),
                (
                    "bypassPermissions",
                    "Bypass permissions",
                    "Skips permission prompts. Deny/ask rules and protected paths still apply.",
                    ApprovalClass::Unrestricted,
                ),
            ] as &[(&str, &str, &str, ApprovalClass)],
        ),
        ProviderKind::Codex => (
            "permissions",
            ApplyTiming::NextTurn,
            "ask",
            "auto_review",
            "full_access",
            &[
                (
                    "ask",
                    "Ask for approval",
                    "Reads, edits and commands run inside the workspace sandbox; access to the internet or files outside the workspace asks you.",
                    ApprovalClass::Manual,
                ),
                (
                    "auto_review",
                    "Approve for me",
                    "Same workspace sandbox; requests that would ask are reviewed by Codex's auto-reviewer and only actions it finds risky ask you.",
                    ApprovalClass::Reviewed,
                ),
                (
                    "read_only",
                    "Read Only",
                    "Reads run in a read-only sandbox; editing files or network access asks you.",
                    ApprovalClass::Manual,
                ),
                (
                    "full_access",
                    "Full Access",
                    "No sandbox and no prompts.",
                    ApprovalClass::Unrestricted,
                ),
            ] as &[(&str, &str, &str, ApprovalClass)],
        ),
        ProviderKind::Grok => (
            "permissionMode",
            ApplyTiming::Restart,
            "default",
            "auto",
            "bypassPermissions",
            &[
                (
                    "default",
                    "Ask",
                    "Reads run without asking; edits and commands ask you.",
                    ApprovalClass::Manual,
                ),
                (
                    "acceptEdits",
                    "Accept edits",
                    "Edits run without asking; commands still ask.",
                    ApprovalClass::Manual,
                ),
                (
                    "auto",
                    "Auto",
                    "A safety classifier approves routine actions; flagged ones ask you.",
                    ApprovalClass::Reviewed,
                ),
                (
                    "bypassPermissions",
                    "Always approve",
                    "Every tool call runs; deny rules still apply.",
                    ApprovalClass::Unrestricted,
                ),
                (
                    "dontAsk",
                    "Don't ask",
                    "Only pre-approved tools run; anything that would ask is denied.",
                    ApprovalClass::Reviewed,
                ),
            ] as &[(&str, &str, &str, ApprovalClass)],
        ),
        // Cursor exposes approvalMode only in user config, with no CLI override; --force honors unrestricted.
        ProviderKind::Cursor => (
            "approvalMode",
            ApplyTiming::Restart,
            "unrestricted",
            "unrestricted",
            "unrestricted",
            &[(
                "unrestricted",
                "Run everything",
                "Every tool call runs without asking",
                ApprovalClass::Unrestricted,
            )] as &[(&str, &str, &str, ApprovalClass)],
        ),
        ProviderKind::OpenCode => (
            "permission_mode",
            ApplyTiming::Live,
            "normal",
            "auto",
            "auto",
            &[
                (
                    "normal",
                    "Normal",
                    "Follows your OpenCode permission rules; actions marked ask wait for you.",
                    ApprovalClass::Manual,
                ),
                (
                    "auto",
                    "Auto-approve",
                    "Answers every ask with once, as OpenCode's own clients do; explicit deny rules still deny.",
                    ApprovalClass::Unrestricted,
                ),
            ] as &[(&str, &str, &str, ApprovalClass)],
        ),
        ProviderKind::Pi | ProviderKind::Acp => return None,
    })
}

/// Pre-session controls; a session's ProviderOptions is authoritative.
pub fn permission_control(provider: ProviderKind) -> Option<OptionDescriptor> {
    let (id, apply, default, recommended, permissive, values) = permission_table(provider)?;
    Some(OptionDescriptor::Select {
        id: id.into(),
        label: "Permissions".into(),
        options: values
            .iter()
            .map(|(value, label, description, class)| SelectOption {
                value: (*value).into(),
                label: (*label).into(),
                description: Some((*description).into()),
                unavailable: None,
                speed: None,
                summary: None,
                approval: Some(*class),
            })
            .collect(),
        default_value: Some(default.into()),
        role: OptionRole::Permission,
        apply,
        recommended: Some(recommended.into()),
        permissive: Some(permissive.into()),
        minimum: None,
    })
}

/// The id every adapter gives the option of [`OptionRole::Effort`], an ACP
/// agent's thought level included. A stored [`OptionSelection`] carries no
/// role, so the host knows a thread's effort, to inherit it, mirror it and
/// send it with each turn, by this id.
pub const EFFORT_OPTION_ID: &str = "reasoningEffort";

/// The ACP session option that carries the agent's mode, which ACP agents use for their
/// approval modes.
pub const ACP_MODE_OPTION_ID: &str = "acp:mode";

/// Whether option `id` of `provider` decides what runs without the user's approval: the
/// provider's permission control, or an ACP session mode, which ACP agents use for their
/// approval modes alongside anything else.
pub fn option_governs_approvals(provider: ProviderKind, id: &str) -> bool {
    id == ACP_MODE_OPTION_ID
        || permission_control(provider).is_some_and(|descriptor| match descriptor {
            OptionDescriptor::Select { id: own, .. }
            | OptionDescriptor::Boolean { id: own, .. } => own == id,
        })
}

pub fn permission_notice(provider: ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Pi => Some("No permission control"),
        _ => None,
    }
}

/// Who settles what a harness would otherwise stop to ask, under one value of
/// its permission control, from least to most permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalClass {
    /// The person driving the turn answers.
    Manual,
    /// The harness settles it itself: a reviewer or classifier decides, or
    /// the request is denied.
    Reviewed,
    /// It runs without anyone deciding.
    Unrestricted,
}

/// The class of `value` for option `id` of `provider`. Every ACP session
/// mode counts as [`ApprovalClass::Unrestricted`]: what an agent's mode lets
/// run is the agent's own and cannot be known here. `None` for an option that
/// does not govern approvals.
pub fn approval_class(provider: ProviderKind, id: &str, value: &str) -> Option<ApprovalClass> {
    if id == ACP_MODE_OPTION_ID {
        return Some(ApprovalClass::Unrestricted);
    }
    let (own, .., values) = permission_table(provider)?;
    (own == id).then(|| {
        values
            .iter()
            .find(|(listed, ..)| *listed == value)
            .map_or(ApprovalClass::Unrestricted, |(.., class)| *class)
    })
}

/// The class a session with `selections` runs under: its permission
/// control's selected value or its default. A provider without one (pi runs
/// every call; an ACP agent's modes are its own) and a value the provider does
/// not list count as [`ApprovalClass::Unrestricted`].
pub fn session_approval_class(
    provider: ProviderKind,
    selections: &[OptionSelection],
) -> ApprovalClass {
    let Some((id, _, default, ..)) = permission_table(provider) else {
        return ApprovalClass::Unrestricted;
    };
    let value = selections
        .iter()
        .find(|selection| selection.id == id)
        .map_or(Some(default), |selection| selection.value.as_str());
    value
        .and_then(|value| approval_class(provider, id, value))
        .unwrap_or(ApprovalClass::Unrestricted)
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn permission_controls_publish_native_choices_and_decode_old_model_options() {
        for (provider, id, apply, default, recommended, permissive) in [
            (
                ProviderKind::ClaudeCode,
                "permissionMode",
                ApplyTiming::Live,
                "default",
                "auto",
                "bypassPermissions",
            ),
            (
                ProviderKind::Codex,
                "permissions",
                ApplyTiming::NextTurn,
                "ask",
                "auto_review",
                "full_access",
            ),
            (
                ProviderKind::Grok,
                "permissionMode",
                ApplyTiming::Restart,
                "default",
                "auto",
                "bypassPermissions",
            ),
            (
                ProviderKind::OpenCode,
                "permission_mode",
                ApplyTiming::Live,
                "normal",
                "auto",
                "auto",
            ),
            (
                ProviderKind::Cursor,
                "approvalMode",
                ApplyTiming::Restart,
                "unrestricted",
                "unrestricted",
                "unrestricted",
            ),
        ] {
            let OptionDescriptor::Select {
                id: actual_id,
                default_value,
                role,
                apply: actual_apply,
                recommended: actual_recommended,
                permissive: actual_permissive,
                ..
            } = permission_control(provider).unwrap()
            else {
                panic!("native select control")
            };
            assert_eq!(actual_id, id);
            assert_eq!(role, OptionRole::Permission);
            assert_eq!(actual_apply, apply);
            assert_eq!(default_value.as_deref(), Some(default));
            assert_eq!(actual_recommended.as_deref(), Some(recommended));
            assert_eq!(actual_permissive.as_deref(), Some(permissive));
        }
        assert_eq!(permission_control(ProviderKind::Pi), None);
        assert_eq!(permission_control(ProviderKind::Acp), None);
        let old: OptionDescriptor = serde_json::from_str(r#"{"kind":"select","id":"reasoningEffort","label":"Reasoning","options":[{"value":"high","label":"High","description":null}],"default_value":"high"}"#).unwrap();
        assert!(
            matches!(old, OptionDescriptor::Select { role:OptionRole::Model, apply:ApplyTiming::Live, recommended:None, permissive:None, options, .. } if options[0].unavailable.is_none() && options[0].speed.is_none() && options[0].summary.is_none())
        );
    }
}

/// Resolve a provider binary to an absolute path before spawning.
///
/// Every provider spawn sets `current_dir(cwd)`, and a bare program name plus a
/// working-directory change makes PATH resolution unreliable (it fails outright
/// when PATH holds unexpanded entries such as `~/.dotnet/tools`). Resolving the
/// binary ourselves against the parent's PATH keeps the lookup deterministic.
#[cfg(feature = "process")]
pub(crate) fn resolve_binary(
    binary_path: Option<&std::path::Path>,
    default_name: &str,
) -> Result<PathBuf, AgentError> {
    if let Some(path) = binary_path {
        return Ok(path.to_path_buf());
    }
    // An explicit path component means "use this as given" (no PATH search).
    // Both separators are checked: Windows accepts `/` in paths too.
    if default_name.contains(['/', '\\']) {
        return Ok(PathBuf::from(default_name));
    }
    if std::env::var_os("PATH").is_none() {
        return Err(AgentError::Spawn(format!(
            "`{default_name}` not found: PATH is unset"
        )));
    }
    find_on_path(default_name).ok_or_else(|| {
        AgentError::Spawn(format!(
            "`{default_name}` not found on PATH (set its path in Settings → Harnesses)"
        ))
    })
}

/// Locate `name` on the parent's `PATH`, returning its absolute path.
///
/// On Windows this is `PATHEXT`-aware: the provider CLIs (and npm/pnpm/bun) are
/// installed as `claude.cmd` / `codex.exe` shims and *never* exist under their
/// bare name, so a plain `dir.join(name)` probe finds nothing. Each PATH entry
/// is tried with every `PATHEXT` extension, in `PATHEXT` order, before the bare
/// name. Elsewhere it is the classic "first executable file on PATH" search.
///
/// Shared with the app crate (`crate::process`), which routes every bare-name
/// spawn (npm, pnpm, bun, git, …) through it on Windows.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    find_in_dirs(
        std::env::split_paths(&path_var),
        name,
        &path_extensions(),
        is_executable,
    )
}

/// Text without its ECMA-48 control sequences (`ESC [`) and operating
/// system commands (`ESC ]`).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // A control sequence ends at its final byte, 0x40..=0x7E.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // An operating system command, such as an OSC 8 hyperlink,
            // ends at BEL or at the string terminator ESC \.
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod strip_ansi_tests {
    /// Claude Code 2.1.322 prints its sign-in URL as an OSC 8 hyperlink
    /// (`ESC ]8;; url BEL text ESC ]8;; BEL`) whenever it judges the
    /// terminal to support links, which it does from TERM_PROGRAM and the
    /// like even when its stdout is a pipe.
    #[test]
    fn a_hyperlinked_sign_in_url_reads_as_the_url() {
        let printed = "If the browser didn't open, visit: \u{1b}]8;;https://example.invalid/oauth?code=1\u{7}\u{1b}[94mhttps://example.invalid/oauth?code=1\u{1b}[39m\u{1b}]8;;\u{7}\n";
        assert_eq!(
            super::strip_ansi(printed),
            "If the browser didn't open, visit: https://example.invalid/oauth?code=1\n"
        );
    }
}

/// The executable extensions to try for a bare name: `PATHEXT` on Windows
/// (falling back to its documented default), and nothing anywhere else.
fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect()
}

/// The file names to probe for `name` in one PATH directory, in priority order.
///
/// A name that already carries an extension (`claude.cmd`, `codex.exe`) is used
/// as given. A bare name gets each `pathext` entry appended, with the
/// extensionless name tried last (so a Unix-style extensionless binary dropped
/// on a Windows PATH still resolves). With an empty `pathext` (every non-Windows
/// target) this is just `[name]`.
fn candidate_names(name: &str, pathext: &[String]) -> Vec<String> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let mut names: Vec<String> = pathext.iter().map(|ext| format!("{name}{ext}")).collect();
    names.push(name.to_string());
    names
}

/// The PATH walk itself, parameterized on the directories, the extension list
/// and the "is this executable" predicate so both platform branches are testable
/// from any host (see `resolve_binary_tests`).
fn find_in_dirs(
    dirs: impl IntoIterator<Item = PathBuf>,
    name: &str,
    pathext: &[String],
    is_exec: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let candidates = candidate_names(name, pathext);
    for dir in dirs {
        // Skip unexpanded/relative entries — they are meaningless to us and
        // would resolve against the child's cwd.
        if !dir.is_absolute() {
            continue;
        }
        for candidate in &candidates {
            let path = dir.join(candidate);
            if is_exec(&path) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no exec bit: a regular file whose name matched a `PATHEXT`
/// candidate *is* the executable.
#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// A chosen option value, persisted per session as a select string or boolean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: serde_json::Value,
}

#[cfg(feature = "process")]
fn selection_str(selections: &[OptionSelection], id: &str) -> Option<String> {
    selections
        .iter()
        .find(|selection| selection.id == id)
        .and_then(|selection| selection.value.as_str().map(str::to_owned))
}

#[cfg(feature = "process")]
fn selection_bool(selections: &[OptionSelection], id: &str) -> Option<bool> {
    selections
        .iter()
        .find(|selection| selection.id == id)
        .and_then(|selection| selection.value.as_bool())
}

/// Whether a [`AgentEvent::UserInputRequested`] holds the agent until it is
/// answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserInputDelivery {
    /// The agent is blocked until a matching
    /// [`SessionCommand::RespondUserInput`] arrives.
    #[default]
    Blocking,
    /// The agent keeps working on the turn and reads the answer when it is
    /// delivered (Codex `request_user_input_async`). The request stays open
    /// until the provider reports it resolved; the composer stays usable
    /// meanwhile.
    Async,
}

impl UserInputDelivery {
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Blocking)
    }
}

/// A structured question the agent asks the user (Claude `AskUserQuestion`,
/// Codex `item/tool/requestUserInput`). Rendered as a multiple-choice (or
/// free-text) prompt; answers ride back through [`SessionCommand::RespondUserInput`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInputQuestion {
    /// The answer key. Claude and Grok: the complete question text (both index
    /// answers by question text). Codex and Cursor: the native question id.
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<UserInputOption>,
    pub multi_select: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    pub description: String,
}

/// An image attachment sent alongside a turn's text. `data_base64` is the raw
/// image bytes, standard-base64 encoded (no data-URL prefix); each provider maps
/// it onto its native content-block shape (Claude `image`/`base64` source; Codex
/// `image` input with a `data:` URL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Standard-base64 of the raw bytes (no `data:...;base64,` prefix).
    pub data_base64: String,
    /// Local path of the persisted copy backing this attachment, when one
    /// exists. Never sent to providers (their payloads are built field by
    /// field); the runtime threads it into the recorded user-message event so
    /// the timeline can render the image without re-encoding the base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// A provider-native command or skill surfaced to the composer's `/` and `$`
/// menus. Each native provider contributes what its command/skill discovery
/// surface exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCommand {
    pub name: String,
    pub description: Option<String>,
    pub kind: ProviderCommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCommandKind {
    /// A `/`-command, sent as `/name …` at the start of the turn text (Claude
    /// `slash_commands`, which include its skills).
    Command,
    /// A `$`-skill, sent as a `$name` mention in the turn text (Codex skills).
    /// A skill the provider also lists as a command is sent as that command,
    /// the only form Claude Code expands.
    Skill,
}

/// Per-turn overrides layered on top of the session's persisted options.
/// Codex and OpenCode apply effort/variant per turn; Claude and pi use their
/// session-level option selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOptions {
    pub effort: Option<String>,
}

/// Where a static model catalog may refresh its data from. Claude's catalog is
/// the t3code model manifest (see `claude_manifest`); the other providers
/// discover their models from the CLI and ignore this.
#[derive(Debug, Clone, Default)]
pub struct CatalogRefresh {
    /// Directory holding the last good remote manifest (the tcode data dir).
    pub cache_dir: Option<PathBuf>,
    /// Whether a network fetch is allowed (the provider update-check setting).
    pub network: bool,
}

/// List the provider's models (spawn, query, teardown). `launch_env` carries the
/// provider's configured environment/home so the catalog reflects the same CLI
/// (and account) a session would actually run against.
#[cfg(feature = "process")]
///
/// The harness processes the listing starts are held in `processes`, which
/// stops them, and so ends the listing, whatever it is waiting on.
pub async fn list_models(
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    refresh: CatalogRefresh,
    processes: &SessionProcesses,
) -> Result<ModelListing, AgentError> {
    let models = match provider {
        ProviderKind::Codex => codex::list_models(binary_path, launch_env, processes).await,
        ProviderKind::ClaudeCode => {
            return claude::list_models(binary_path, launch_env, refresh, processes).await;
        }
        ProviderKind::Pi => pi::list_models(binary_path, launch_env, processes).await,
        ProviderKind::OpenCode => opencode::list_models(binary_path, launch_env, processes).await,
        ProviderKind::Cursor => cursor::list_models(binary_path, launch_env, processes).await,
        ProviderKind::Grok => grok::list_models(binary_path, launch_env, processes).await,
        // ACP agents advertise their models over the wire at session start
        // (`AgentEvent::ProviderOptions`), so there is no catalog to pre-fetch.
        ProviderKind::Acp => Ok(Vec::new()),
    }?;
    Ok(ModelListing {
        models,
        problems: Vec::new(),
    })
}

/// A provider's models, and what of the catalog they came from could not be
/// read and was left out, in Tcode's words.
#[cfg(feature = "process")]
#[derive(Debug, Clone, Default)]
pub struct ModelListing {
    pub models: Vec<ModelSpec>,
    pub problems: Vec<String>,
}

#[derive(Debug)]
pub enum AgentError {
    Spawn(String),
    /// The CLI is a Windows batch file that is not a recognised package
    /// manager shim, and an argument spans lines, which `cmd.exe` cannot pass.
    BatchLineBreak {
        program: PathBuf,
    },
    Protocol(String),
    Provider(String),
    Io(std::io::Error),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(message) => {
                write!(formatter, "failed to spawn provider process: {message}")
            }
            Self::BatchLineBreak { program } => write!(
                formatter,
                "failed to spawn provider process: `{}` is a batch file, which cannot be given an argument that spans lines; install the CLI's native build or set its executable in Settings → Harnesses",
                program.display()
            ),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Provider(message) => write!(formatter, "provider reported error: {message}"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AgentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Commands the UI sends into a live session's actor loop.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    SendTurn {
        /// Runtime queue id echoed by [`AgentEvent::TurnAccepted`] after the
        /// adapter has crossed its provider-specific submission boundary.
        delivery_id: u64,
        text: String,
        /// Per-turn overrides (Codex/OpenCode apply effort per turn; Claude/pi
        /// use session options). `None` = use the persisted options.
        options: Option<TurnOptions>,
        /// Image attachments carried alongside the text (empty for text-only
        /// turns). Each provider maps these onto its native content blocks.
        attachments: Vec<Attachment>,
    },
    RetryTurn {
        turn_id: String,
    },
    Interrupt,
    RespondApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Answer a pending user-input request (Claude `AskUserQuestion`, Codex
    /// `item/tool/requestUserInput` and `request_user_input_async`). Each value
    /// is a string (single-select / free text) or an array of strings
    /// (multi-select), keyed by the matching [`UserInputQuestion::id`].
    RespondUserInput {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    /// Inject a message into the turn that is ALREADY running, so the model
    /// picks it up at its next opportunity to accept input (typically the next
    /// tool call). Distinct from queueing, which is an app-level concept: a
    /// queued message is held and sent as an ordinary [`Self::SendTurn`] once
    /// the current turn completes.
    ///
    /// Only providers whose [`ProviderKind::caps`] reports steering accept
    /// this; the others log and ignore it (the UI must not offer it there).
    Steer {
        /// Stable id correlating the persisted request with provider acceptance.
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Set one of the agent's self-described options (see
    /// [`AgentEvent::ProviderOptions`]). An agent speaking ACP gets
    /// `session/set_mode` or `session/set_config_option` by the descriptor's
    /// origin; the other providers ignore ids they do not know.
    SetOption {
        id: String,
        value: serde_json::Value,
    },
    /// Stop one piece of background work, or all of it with `None`, on a
    /// harness whose [`Caps::background`] stops it. Its next
    /// [`AgentEvent::BackgroundTaskList`] shows the result.
    StopBackgroundTask {
        task_id: Option<String>,
    },
    /// Go on in `cwd`, the directory the thread moved to, keeping the native
    /// session, on a harness whose [`Caps::session_move`] says it can. Each
    /// harness moves its own way, by the turn after this at the latest; the
    /// others ignore it.
    MoveTo {
        cwd: PathBuf,
    },
    /// Answer the harness's [`AgentEvent::WorktreeRequested`]; the adapter
    /// confirms it with [`AgentEvent::WorktreeResolved`].
    RespondWorktree {
        request_id: String,
        answer: WorktreeAnswer,
    },
    /// Move the running call `item_id` into the background, on a harness
    /// whose [`Caps::background`] can: the call returns to the model and its
    /// work goes on ([`AgentEvent::ItemBackgrounded`]).
    MoveToBackground {
        item_id: String,
    },
    /// Bring the running process to these settings where it stands, on a
    /// harness whose [`Caps::live_settings`] says it can be asked. Answered by
    /// [`AgentEvent::SettingsApplied`].
    ApplySettings {
        model: Option<String>,
        option_selections: Vec<OptionSelection>,
    },
    /// Ask the provider to restore one of its own message checkpoints. The
    /// provider remains the sole owner of both the checkpoint and the restore
    /// semantics; adapters without a native implementation reject this. The
    /// adapter stops a running turn first the way its harness does. Answered by
    /// [`AgentEvent::RewindCompleted`] or [`AgentEvent::RewindFailed`].
    Rewind {
        checkpoint_id: String,
        mode: RewindMode,
    },
    /// Ask what [`Self::Rewind`] would restore, without restoring anything,
    /// where [`RewindControl::preview`] says the harness can tell. Answered by
    /// [`AgentEvent::RewindPreviewed`].
    PreviewRewind {
        checkpoint_id: String,
        mode: RewindMode,
    },
    /// Undo a rewind the harness keeps undoable: the `undo_token` of its
    /// [`AgentEvent::RewindCompleted`]. Answered by
    /// [`AgentEvent::RewindUndone`] or [`AgentEvent::RewindUndoFailed`].
    UndoRewind {
        checkpoint_id: String,
        undo_token: String,
    },
    /// Ask the provider to compact its context now, the way its own `/compact`
    /// does (Codex `thread/compact/start`, pi `compact`, Claude Code's
    /// `/compact` slash command). The provider reports progress as
    /// [`AgentEvent::ContextCompacted`]; adapters without a native operation
    /// warn and ignore it, and the UI offers it only where
    /// [`Caps::manual_compaction`] is set.
    Compact,
    /// Set, replace or change the status of the harness's standing goal the
    /// way its own `/goal` does (Codex `thread/goal/set`, Claude Code's `/goal`
    /// slash command). `None` keeps the current value. The provider reports
    /// the result as [`AgentEvent::GoalUpdated`]; the UI offers it only where
    /// [`Caps::goals`] is set.
    SetGoal {
        objective: Option<String>,
        status: Option<GoalStatus>,
    },
    /// Clear the standing goal (Codex `thread/goal/clear`, `/goal clear`).
    ClearGoal,
    /// Read the signed-in account's usage windows from the running harness
    /// (Claude Code's `get_usage` control request, Codex
    /// `account/rateLimits/read`), answered on `reply` with the payload a
    /// short-lived process would give ([`claude::read_usage`],
    /// [`codex::read_rate_limits`]). A harness without such a read drops
    /// `reply`.
    #[cfg(feature = "process")]
    ReadAccountUsage {
        reply: smol::channel::Sender<Result<serde_json::Value, String>>,
    },
    Shutdown,
}

/// A live provider session: send commands in, read canonical events out.
/// Dropping both channels (or sending `Shutdown`) tears the child process down.
#[cfg(feature = "process")]
pub struct SessionHandle {
    pub provider: ProviderKind,
    pub commands: Sender<SessionCommand>,
    pub events: Receiver<AgentEvent>,
}

#[cfg(feature = "process")]
fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(feature = "process")]
async fn spawn_session<F, Fut>(
    provider: ProviderKind,
    opts: SessionOptions,
    actor: F,
    exited_message: &'static str,
) -> Result<SessionHandle, AgentError>
where
    F: FnOnce(
        SessionOptions,
        Receiver<SessionCommand>,
        Sender<AgentEvent>,
        Sender<Result<(), AgentError>>,
    ) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (commands, command_rx) = smol::channel::unbounded();
    let (event_tx, events) = smol::channel::unbounded();
    let (ready_tx, ready) = smol::channel::bounded(1);
    smol::spawn(actor(opts, command_rx, event_tx, ready_tx)).detach();
    ready
        .recv()
        .await
        .map_err(|_| AgentError::Protocol(exited_message.into()))??;
    Ok(SessionHandle {
        provider,
        commands,
        events,
    })
}

/// Start a new (or resumed) session with the given provider.
#[cfg(feature = "process")]
pub async fn start_session(
    provider: ProviderKind,
    opts: SessionOptions,
) -> Result<SessionHandle, AgentError> {
    // Spawning into a missing directory fails with the OS's "not found" (or
    // Windows' "invalid directory"), which every provider then reports as its
    // binary being missing. Worktrees are routinely removed under old threads.
    if !opts.cwd.is_dir() {
        return Err(AgentError::Spawn(format!(
            "working directory `{}` no longer exists",
            opts.cwd.display()
        )));
    }
    match provider {
        ProviderKind::Codex => codex::start(opts).await,
        ProviderKind::ClaudeCode => claude::start(opts).await,
        ProviderKind::Pi => pi::start(opts).await,
        ProviderKind::OpenCode => opencode::start(opts).await,
        ProviderKind::Cursor => cursor::start(opts).await,
        ProviderKind::Grok => grok::start(opts).await,
        ProviderKind::Acp => acp::start(opts).await,
    }
}

/// A failed native turn can be continued without adding user input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRetry {
    pub turn_id: String,
    /// Unix seconds; None leaves only the manual retry available.
    pub retry_at: Option<u64>,
}

/// The native session a [`AgentEvent::ProviderRelay`] left behind, with what
/// starts its harness on it again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeftSession {
    /// Names the handoff in [`AgentEvent::RelayUndone`].
    pub id: String,
    /// The provider profile whose home holds it; `None` is the built-in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    /// The option values the thread had chosen there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<OptionSelection>,
    pub resume: ResumeCursor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnRetry {
        retry: Option<TurnRetry>,
    },
    /// A tcode-level handoff between providers. This is never emitted by an
    /// adapter; the runtime persists it before the first relayed user turn.
    ProviderRelay {
        from_provider: ProviderKind,
        from_model: Option<String>,
        to_provider: ProviderKind,
        to_model: Option<String>,
        /// The handoff as sent to `to_provider`: the transcript of the
        /// conversation so far wrapped around the user's message. Records from
        /// before it was kept carry `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// Set when the handoff follows the thread's move to another directory: the
        /// directory the native session it replaces belonged to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        moved_from: Option<PathBuf>,
        /// The native session `from_provider` held, where the thread can go
        /// on in it again: a fork through a turn it ran, or a rewind to one,
        /// returns to it. Absent where it cannot (the thread moved, the
        /// session was never the thread's own) and in records from before it
        /// was kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left: Option<LeftSession>,
    },
    /// The thread went back to the session the [`Self::ProviderRelay`] whose
    /// `left` is named `relay` had left, as it stood there: the message that
    /// handoff carried and everything after it leave the conversation. Like
    /// the handoff this is the runtime's record; no harness is asked for it.
    RelayUndone {
        relay: String,
    },
    /// The agent's self-described options (ACP `modes` / `models` /
    /// `configOptions`), pushed at session start and on every change. Reuses
    /// [`OptionDescriptor`] so the composer's existing picker renders them
    /// verbatim; answer with [`SessionCommand::SetOption`].
    ///
    /// `selections` are the values in effect as the harness reported them,
    /// for the options its [`Caps::confirmed_options`] names; a `null` value
    /// is the harness saying it has none to report for that option now.
    /// Transient: the runtime keeps them on the session, not in its history.
    ProviderOptions {
        descriptors: Vec<OptionDescriptor>,
        selections: Vec<OptionSelection>,
    },
    /// Emitted once the provider session is ready. `resume` must round-trip
    /// through `SessionOptions::resume` to continue this thread later.
    /// `model` is the model the session is set to as the harness reports it,
    /// in the spelling Tcode asked for when that names the same model; `None`
    /// when it reports none.
    SessionStarted {
        provider_session_id: String,
        resume: ResumeCursor,
        model: Option<String>,
    },
    /// The model the running session is set to, as the harness reported it
    /// after [`SessionStarted`](Self::SessionStarted), spelled as there.
    /// Transient, like [`Self::TurnAccepted`]. [`Self::ServedModel`] is the
    /// model that answered one response, which a harness may reroute.
    SessionModel {
        model: String,
    },
    /// The model that actually served the response. Providers may reroute away
    /// from the model selected when the session started.
    ServedModel {
        model: String,
        /// The model the harness named before this one in the same session:
        /// what it says it rerouted from, or the last it reported serving.
        /// `None` when it has named none; records from before it was kept
        /// carry `None` too.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        reason: Option<String>,
    },
    TurnStarted {
        turn_id: String,
    },
    /// Transient delivery acknowledgement for [`SessionCommand::SendTurn`].
    /// The runtime consumes this before persistence: it is control-plane state,
    /// not part of the canonical conversation history.
    TurnAccepted {
        delivery_id: u64,
    },
    /// The agent took none of [`SessionOptions::mcp_servers`] (an ACP agent without HTTP MCP),
    /// so neither the tools nor [`SessionOptions::instructions`] reach the model. Transient, like
    /// [`Self::TurnAccepted`].
    McpServersNotAttached,
    /// Claude-owned background work which must keep its provider process alive
    /// after the model turn itself has completed. Other adapters currently do
    /// not emit this transient liveness signal.
    BackgroundTasksChanged {
        count: usize,
    },
    /// The background work a harness with [`Caps::background`] is running
    /// now, replacing the last list. Transient, like
    /// [`Self::BackgroundTasksChanged`], which goes on carrying the liveness a
    /// list that just emptied may not yet mean.
    BackgroundTaskList {
        tasks: Vec<BackgroundTask>,
    },
    /// The answer to [`SessionCommand::ApplySettings`]; `false` when the change
    /// takes a restart. Transient, like [`Self::TurnAccepted`].
    SettingsApplied {
        applied: bool,
    },
    /// The provider's current net file changes for one turn. Unlike the
    /// per-tool [`ItemContent::FileChange`] stream, this is a replacement
    /// snapshot: later events with the same `turn_id` supersede earlier ones.
    /// Providers that supply the exact net result of their tracked mutations
    /// mark it [`ChangeCompleteness::Exact`]; adapters built from structured
    /// edit fragments use [`ChangeCompleteness::Partial`].
    TurnChangesUpdated {
        turn_id: String,
        changes: Vec<FileChange>,
        completeness: ChangeCompleteness,
    },
    /// A provider-owned restore point attached to one user turn. The id is
    /// opaque and must only be returned to the provider that emitted it.
    TurnCheckpoint {
        turn_id: String,
        checkpoint_id: String,
    },
    /// A provider-owned restore point for a steer: the conversation alone
    /// can be rewound to before it ([`RewindControl::steer_targets`]).
    SteerCheckpoint {
        request_id: String,
        checkpoint_id: String,
    },
    /// The restore points recorded so far no longer address the harness's
    /// session (the thread was forked into a new one); none of them is a
    /// rewind target any more.
    CheckpointsCleared,
    /// The latest rewind can no longer be undone here, though nothing was
    /// sent after it: its log was carried into a thread that goes on in
    /// another of the Harness's sessions, so the branch it left is reached
    /// only as a thread of its own, which one thread at a time can go on in.
    RewindUndoClosed,
    /// Whether this thread's harness session can be rewound at all, where
    /// that differs between sessions of one harness (Codex's legacy history).
    RewindAvailability {
        /// Why it cannot; `None` once it can.
        unavailable: Option<RewindFailureReason>,
    },
    /// What a [`SessionCommand::PreviewRewind`] found. Transient, like
    /// [`Self::TurnAccepted`]: the runtime answers the client that asked.
    RewindPreviewed {
        checkpoint_id: String,
        mode: RewindMode,
        result: RewindPreviewResult,
    },
    /// The provider applied a native restore operation. `mode` is what it
    /// restored; when that is less than was asked, `incomplete` says what
    /// failed. A conversation restore also returns the selected prompt for
    /// composer prefill, matching the provider's own edit-after-rewind
    /// behavior.
    RewindCompleted {
        checkpoint_id: String,
        mode: RewindMode,
        prefill: Option<String>,
        /// Local paths of the images the restored prompt carried.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prefill_attachments: Vec<String>,
        #[serde(default)]
        later_history: LaterHistory,
        /// The files the restore changed, when the harness said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<RewindFiles>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        incomplete: Option<RewindIncomplete>,
        /// Present while the harness keeps this rewind undoable; returned in
        /// [`SessionCommand::UndoRewind`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo_token: Option<String>,
    },
    /// A native restore request was rejected without changing anything.
    RewindFailed {
        checkpoint_id: String,
        mode: RewindMode,
        failure: RewindFailure,
    },
    /// The harness undid the rewind to `checkpoint_id`: the later turns are
    /// its conversation again.
    RewindUndone {
        checkpoint_id: String,
    },
    RewindUndoFailed {
        checkpoint_id: String,
        failure: RewindFailure,
    },
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        usage: Option<TokenUsage>,
    },
    ItemStarted(ThreadItem),
    ItemUpdated(ThreadItem),
    ItemCompleted(ThreadItem),
    /// The call `item_id` returned to the model while the work it started
    /// goes on — a command or an agent launched in the background — so the
    /// turn no longer waits on it. The item stays in progress until a later
    /// snapshot settles it, which is when that work ends. `parent_item_id`
    /// is the item's own, as on [`ThreadItem`].
    ItemBackgrounded {
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_item_id: Option<String>,
    },
    /// A runtime-synthesized steering request, persisted before provider delivery.
    SteerRequested {
        request_id: String,
        text: String,
        /// Local paths of the image attachments steered with the text (see
        /// [`ItemContent::UserMessage::attachments`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Emitted only after the provider has consumed the correlated steer into
    /// its model context (or provided the strongest available consumption
    /// signal when its protocol has no explicit acknowledgement). Codex echoes
    /// the drained input as a `userMessage` item before its next model request;
    /// the `turn/steer` reply alone only means the input was enqueued. Claude
    /// uses its next `status: requesting` checkpoint; a steer written
    /// microseconds before that status can still miss the request, but this is
    /// the best available signal without CLI protocol support.
    SteerAccepted {
        request_id: String,
    },
    /// The provider could not fold the correlated steer into a running turn:
    /// the turn had already ended by the time the request reached it. The
    /// text was never seen by the model, so the runtime re-sends it as an
    /// ordinary turn.
    SteerRejected {
        request_id: String,
    },
    /// Streaming text growth for an in-progress item. The item may not have
    /// been announced via `ItemStarted` yet (providers differ); the UI creates
    /// the item lazily on first delta.
    Delta {
        item_id: String,
        kind: DeltaKind,
        text: String,
    },
    /// A turn ending does not settle it; [`AgentEvent::ApprovalResolved`] does.
    ApprovalRequested(ApprovalRequest),
    /// A pending approval has been settled: the user's decision, or
    /// [`ApprovalDecision::Cancel`] when it ended without one (the harness
    /// withdrew it, or an interrupt cancelled it).
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// The agent is asking the user one or more structured questions.
    /// `delivery` says whether it waits for the matching
    /// [`SessionCommand::RespondUserInput`] or keeps working meanwhile. A turn
    /// ending does not settle it; [`AgentEvent::UserInputResolved`] does.
    UserInputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default, skip_serializing_if = "UserInputDelivery::is_blocking")]
        delivery: UserInputDelivery,
    },
    /// A pending user-input request has been settled: answered, or ended
    /// without an answer (the harness withdrew it, an interrupt or teardown
    /// cancelled it), in which case `answers` is empty.
    UserInputResolved {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    TokenUsage(TokenUsage),
    /// Provider-native commands / skills discovered for this session. The
    /// composer's `/` and `$` menus consume these. Session metadata — not folded
    /// into the timeline / persisted to the JSONL log.
    ProviderCommands {
        commands: Vec<ProviderCommand>,
    },
    /// The provider compacted its context window (Claude `system/compact_boundary`;
    /// Codex `contextCompaction` item). Rendered as a "Context compacted" work-log row.
    ContextCompacted(Compaction),
    /// A tcode-level context-window change. This is never emitted by an adapter;
    /// the runtime persists it after the user message that selected the window.
    ContextWindowChanged {
        window: u64,
    },
    /// The message before this ran on the settings the live process started
    /// with: the restart that takes the thread's new ones waits for its
    /// background work. Never emitted by an adapter; the runtime persists it
    /// after the user message it describes.
    SettingsKept,
    /// The native session goes on in `to` instead of `from` ([`Caps::session_move`]).
    /// The runtime records it when it moves the thread; an adapter reports one its
    /// harness made, after a [`SessionCommand::MoveTo`] or on its own (Claude Code
    /// entering or leaving a worktree), and the thread follows a move to anywhere
    /// it is not. With `parent_item_id`, it is that native subagent (its Subagent
    /// item) that works in `to`, and the session stays where it was.
    SessionMoved {
        from: PathBuf,
        to: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_item_id: Option<String>,
    },
    /// The harness waits for the host to create or remove a git worktree, as
    /// Claude Code does through its `WorktreeCreate` and `WorktreeRemove` hooks
    /// for every worktree it works in. Answered by
    /// [`SessionCommand::RespondWorktree`]. Transient, like
    /// [`Self::TurnAccepted`].
    WorktreeRequested {
        request_id: String,
        request: WorktreeRequest,
    },
    /// The harness was given `answer` to its [`Self::WorktreeRequested`].
    WorktreeResolved {
        request_id: String,
        request: WorktreeRequest,
        answer: WorktreeAnswer,
    },
    /// Another thread's model acted on this thread through `tcode_control`. This is
    /// never emitted by an adapter; the runtime records it in the target thread.
    ControlAction(ControlAction),
    /// A row of a thread's log that this build cannot read, as it is stored:
    /// a record of a type or shape Tcode no longer has. Never emitted by an
    /// adapter nor written; the store reads it in the row's place.
    UnreadableRecord {
        line: String,
    },
    /// A record of `bytes` bytes, more than one reply to the client it was
    /// for can carry, kept on the host in a history reply. Never emitted by
    /// an adapter nor written; the host sends it in the record's place.
    /// `row` is the record's log cursor, by which the client reads it whole
    /// through an original (`OriginalTarget::Record`): a history window
    /// names it, and a live event, whose cursor the sender does not know, is
    /// the record at the end of the log the client holds.
    RecordWithheld {
        bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row: Option<u64>,
    },
    /// Structured plan / task list for the sidebar (Codex `turn/plan/updated`,
    /// Claude `TodoWrite`). Replaces the current turn's plan wholesale.
    PlanUpdated {
        turn_id: Option<String>,
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    /// The harness's standing goal changed (Codex `thread/goal/updated` and
    /// `thread/goal/cleared`, Claude Code's `goal_status` transcript records).
    /// Replaces the session's goal wholesale; `None` means there is none.
    GoalUpdated {
        goal: Option<Goal>,
    },
    Warning {
        message: String,
    },
    /// A runtime-synthesized fatal provider startup failure persisted for replay.
    #[rustfmt::skip]
    ProviderStartFailed { error: String },
    Error {
        message: String,
        fatal: bool,
    },
    /// Emitted after the [`AgentEvent::Error`] of a turn Claude Code rejected
    /// for an exhausted usage window. `resets_at` is unix seconds.
    UsageLimitReached {
        resets_at: u64,
    },
    SessionClosed {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// An objective the harness keeps working toward across turns until it is
/// met, paused or cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_used_secs: Option<u64>,
    /// How many times the harness has checked the objective against the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<u32>,
    /// The latest check's verdict, when the harness explains it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    /// The harness judged it cannot make progress (Codex `blocked`, a Claude
    /// Code check that found the objective impossible).
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

/// Provider-native restore scopes. These describe what the upstream agent
/// changes; tcode never emulates a missing scope with Git or transcript edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    Files,
    Conversation,
    FilesAndConversation,
}

impl SessionCommand {
    /// The answer to a rewind command the adapter cannot carry out: the
    /// failure event that command expects, or `None` for any other command.
    pub fn rewind_refusal(&self, failure: RewindFailure) -> Option<AgentEvent> {
        Some(match self {
            Self::Rewind {
                checkpoint_id,
                mode,
            } => AgentEvent::RewindFailed {
                checkpoint_id: checkpoint_id.clone(),
                mode: *mode,
                failure,
            },
            Self::PreviewRewind {
                checkpoint_id,
                mode,
            } => AgentEvent::RewindPreviewed {
                checkpoint_id: checkpoint_id.clone(),
                mode: *mode,
                result: RewindPreviewResult::Refused(failure),
            },
            Self::UndoRewind { checkpoint_id, .. } => AgentEvent::RewindUndoFailed {
                checkpoint_id: checkpoint_id.clone(),
                failure,
            },
            _ => return None,
        })
    }
}

/// Why a harness did not rewind, in the terms the UI explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindFailureReason {
    /// A turn is running and the harness does not stop it for a rewind.
    TurnRunning,
    /// Input the harness has not taken yet is waiting.
    Busy,
    /// The target is no longer in the harness's conversation (compacted,
    /// forked away, already rewound).
    TargetNotFound,
    /// The conversation changed since the target was chosen.
    ConversationChanged,
    /// The harness cannot cut the conversation at that point.
    CannotCutHere,
    /// There was nothing to restore.
    NothingToRestore,
    /// Files changed outside the agent since; restoring would overwrite them.
    Conflicts,
    UsageLimited,
    Expired,
    /// The harness could not save the result.
    SaveFailed,
    /// This thread's session cannot be rewound.
    Unsupported,
    /// The harness process stopped before it answered.
    ProcessGone,
    #[serde(other)]
    Other,
}

/// A refusal or failure, with the harness's own words for the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindFailure {
    pub reason: RewindFailureReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RewindFailure {
    pub fn new(reason: RewindFailureReason, detail: impl Into<Option<String>>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

/// The part of a rewind that failed after another part was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindIncomplete {
    pub requested: RewindMode,
    pub failure: RewindFailure,
}

/// Files a rewind restores or would restore.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindFiles {
    pub files: Vec<RewindFile>,
    /// Totals, when the harness counts lines only across all files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    /// Paths the harness leaves alone for link safety.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_links: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
}

impl RewindFile {
    pub fn path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            insertions: None,
            deletions: None,
        }
    }
}

/// What a rewind would do, before it is made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPreview {
    pub files: RewindFiles,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<RewindConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindConflict {
    pub path: String,
    pub kind: RewindConflictKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindConflictKind {
    ModifiedExternally,
    CreatedExternally,
    DeletedExternally,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RewindPreviewResult {
    Ready(RewindPreview),
    Refused(RewindFailure),
}

impl RewindMode {
    pub fn includes_files(self) -> bool {
        matches!(self, Self::Files | Self::FilesAndConversation)
    }

    pub fn includes_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::FilesAndConversation)
    }
}

/// How strongly a provider can attribute a turn's reported file changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCompleteness {
    /// The provider supplied the exact turn-scoped net result of the file
    /// mutations it tracks, rather than inferring it from the ambient working
    /// tree. Uninstrumented command writes are outside that claim.
    Exact,
    /// The listed paths came from structured edit operations, but arbitrary
    /// commands or external writers may have changed additional files.
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    AssistantText,
    ReasoningText,
    CommandOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadItem {
    /// Provider-scoped stable id; deltas and later lifecycle events reference it.
    pub id: String,
    /// Spawn item that owns this child activity, when the item came from a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    pub content: ItemContent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemContent {
    UserMessage {
        text: String,
        /// Local paths of the image attachments sent with this message, so the
        /// timeline (live and replayed) can render them. Empty on text-only
        /// messages and on logs written before this field existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    AssistantMessage {
        text: String,
        /// The provider delivered a summary of what the model told the user,
        /// not the model's own words (Claude Code's narration blocks).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        summarized: bool,
    },
    Reasoning {
        text: String,
    },
    /// A plan the harness proposed on its way out of its own plan mode
    /// (Claude Code's `ExitPlanMode`, Grok's `exit_plan_mode`, Cursor's
    /// `create_plan`, Codex's plan item). When the harness waits on a decision
    /// it also asks an [`ApprovalKind::Plan`] naming this item.
    Plan {
        markdown: String,
    },
    CommandExecution {
        command: String,
        /// The lines written to the command's stdin while it ran, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        input: Vec<String>,
        output: String,
        exit_code: Option<i32>,
        status: ItemStatus,
    },
    FileChange {
        changes: Vec<FileChange>,
        status: ItemStatus,
        /// What the harness gave back for the change, as written: the text
        /// its tool returned, or why it failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// The command item whose run changed these files, when the harness
        /// saw them change while it ran instead of a tool asking for each
        /// change (Claude Code's `bashEditDiff`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    ToolCall {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        image_reads: Vec<Attachment>,
        name: String,
        input: serde_json::Value,
        output: Option<String>,
        status: ItemStatus,
        /// What the call is, when its adapter knows the harness's tool for
        /// one of the families every harness has: the UI draws it from these
        /// fields, never from the tool's name or input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        family: Option<Box<ToolFamily>>,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        /// Final one-line summary when finished.
        summary: Option<String>,
        /// Model the provider ran the subagent on, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Reasoning effort the subagent ran with, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },

    ImageRead {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<Attachment>,
    },
    WebSearch {
        query: String,
    },
    /// Anything canonicalization doesn't model yet; `summary` is displayable.
    Other {
        provider_kind: String,
        summary: String,
    },
}

/// A tool call as its adapter read it from the harness's own tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFamily {
    /// A read of a file. The call's `output` is what was read, without the
    /// numbers a harness put before its lines.
    Read {
        path: String,
        /// The number of the output's first line, when the harness said
        /// which line the read began at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_line: Option<u64>,
        /// The harness's own words after the lines it read: that the file
        /// goes on, and where to continue.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// A search of the workspace. The call's `output` stays the harness's
    /// whole answer, `hits` what its adapter read out of it.
    Search {
        scope: SearchScope,
        /// What it looked for: a pattern of text, or of file names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        /// Where it looked, as the harness was asked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The files it looked in, when it was narrowed to some.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glob: Option<String>,
        /// What it found, once it finished and its result could be read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hits: Option<Vec<SearchHit>>,
    },
    /// Questions the model put to the user, and the answers each got.
    Question { questions: Vec<AskedQuestion> },
    /// A page fetched from the web. The call's `output` is what came back.
    Fetch {
        url: String,
        /// What the harness was asked to make of the page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// A search of the web. The call's `output` stays the harness's whole
    /// answer, `links` the pages its adapter read out of it.
    WebSearch {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        links: Option<Vec<WebLink>>,
    },
    /// A search of the harness's own deferred tools, which loads those it
    /// finds for the model.
    LoadTools {
        query: String,
        /// The tools it loaded, once it finished.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
    },
    /// A skill the harness launched or loaded.
    Skill { name: String },
    /// A pause the model asked for, of the length it asked.
    Wait { duration_ms: u64 },
    /// A picture the harness generated. The call's images are the picture.
    ImageGeneration {
        /// The prompt the picture was generated from, as the harness
        /// rewrote it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// The harness's automatic reviewer's verdict on an action the model
    /// took. The call's output is its rationale.
    Review {
        /// The action reviewed.
        action: String,
        /// The risk it rated the action, when it rated one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        risk: Option<String>,
    },
}

impl ToolFamily {
    /// What the call is about, as its family names it.
    pub fn subject(&self) -> Option<&str> {
        match self {
            Self::Read { path, .. } => Some(path),
            Self::Search { pattern, path, .. } => pattern.as_deref().or(path.as_deref()),
            Self::Question { questions } => questions.first().map(|asked| asked.question.as_str()),
            Self::Fetch { url, .. } => Some(url),
            Self::WebSearch { query, .. } | Self::LoadTools { query, .. } => Some(query),
            Self::Skill { name } => Some(name),
            Self::Wait { .. } => None,
            Self::ImageGeneration { prompt } => prompt.as_deref(),
            Self::Review { action, .. } => Some(action),
        }
    }
}

/// A page a web search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebLink {
    pub title: String,
    pub url: String,
}

/// A question as the model asked it, with what the user answered: the
/// labels of the options chosen, or the user's own words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedQuestion {
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<UserInputOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<String>,
}

/// What a search looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    /// Lines of files matching a pattern.
    Content,
    /// Files whose names match a pattern.
    Files,
    /// What a directory holds.
    Entries,
}

/// One thing a search found: a file or directory, or a line of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// Absolute, or relative to the directory the session works in.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// The matching line's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff for this file when the provider supplies one.
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Create,
    Modify,
    Delete,
    Rename,
    /// The harness reports the file changed and does not say how.
    Changed,
}

/// A provider-supplied, multi-file Git-style unified diff, split into the
/// files it changes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UnifiedDiff {
    /// One change per `diff --git` section whose file could be read, each
    /// keeping its whole section for rendering.
    pub changes: Vec<FileChange>,
    /// What could not be read as a file's section, exactly as the provider
    /// wrote it: text before the first section, or a section naming no file.
    pub unread: Vec<String>,
}

/// Split a provider-supplied, multi-file Git-style unified diff into canonical
/// file changes. This parses only file boundaries and operation kinds; each
/// change retains its complete diff section, and what is not a readable
/// section is kept as written.
pub fn file_changes_from_unified_diff(diff: &str) -> UnifiedDiff {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        sections.push(current);
    }
    let mut read = UnifiedDiff::default();
    for section in sections {
        if section.trim().is_empty() {
            continue;
        }
        match diff_section(&section) {
            Some((path, kind)) => read.changes.push(FileChange {
                path,
                kind,
                diff: Some(section),
            }),
            None => read.unread.push(section),
        }
    }
    read
}

/// The file a `diff --git` section changes, and how.
fn diff_section(section: &str) -> Option<(String, FileChangeKind)> {
    let header = section.lines().next()?.strip_prefix("diff --git a/")?;
    let mut old_path = None;
    let mut new_path = None;
    for line in section.lines() {
        if let Some(path) = line.strip_prefix("rename from ") {
            old_path = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("rename to ") {
            new_path = Some(path.to_string());
        } else if let Some(path) = line.strip_prefix("--- a/") {
            old_path.get_or_insert_with(|| path.to_string());
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            new_path.get_or_insert_with(|| path.to_string());
        }
    }
    if (old_path.is_none() || new_path.is_none())
        && let Some((old, new)) = header.rsplit_once(" b/")
    {
        old_path.get_or_insert_with(|| old.to_string());
        new_path.get_or_insert_with(|| new.to_string());
    }
    let creates = section
        .lines()
        .any(|line| line.starts_with("new file mode ") || line == "--- /dev/null");
    let deletes = section
        .lines()
        .any(|line| line.starts_with("deleted file mode ") || line == "+++ /dev/null");
    let kind = if creates {
        FileChangeKind::Create
    } else if deletes {
        FileChangeKind::Delete
    } else if old_path != new_path {
        FileChangeKind::Rename
    } else {
        FileChangeKind::Modify
    };
    let path = match kind {
        FileChangeKind::Delete => old_path,
        _ => new_path.or(old_path),
    }
    .filter(|path| !path.is_empty())?;
    Some((path, kind))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub turn_id: Option<String>,
    /// Spawn item of the subagent that asked. A background subagent outlives
    /// the turn that spawned it, so its request stays open past that turn's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    pub kind: ApprovalKind,
    /// Why the harness asks a person rather than deciding itself, in its own
    /// words: Claude Code's consent line (a safety check it cannot bypass, a
    /// rule, a hook), Codex's justification, OpenCode's or pi's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The path a harness's path check stopped at, which may not appear in
    /// the request itself (where a symlink or redirection lands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    /// The provider's native choices for this request, in provider order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ApprovalOption>,
}

/// One choice offered by a provider's permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    /// Opaque id echoed back in [`ApprovalDecision::Option`].
    pub id: String,
    pub label: String,
    pub kind: ApprovalOptionKind,
}

/// Native scope and disposition, for presentation and orchestration routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    ExecCommand {
        command: String,
        cwd: Option<String>,
        /// What the command does, in the agent's words.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// A read-only file or search operation. `detail` is the pre-rendered
    /// summary.
    FileRead {
        detail: String,
    },
    FileChange {
        changes: Vec<FileChange>,
    },
    /// Dynamic fallback for any tool that doesn't classify as command / file
    /// read / file change (agent, mcp, image, …). `detail` is the pre-rendered
    /// summary.
    ToolUse {
        name: String,
        input: serde_json::Value,
        detail: String,
    },
    /// Leave the harness's plan mode on the plan in [`ItemContent::Plan`]
    /// item `item_id`. A reply in the user's own words is
    /// [`ApprovalDecision::Feedback`]: the harness keeps planning from it.
    Plan {
        item_id: String,
        markdown: String,
    },
}

/// What a harness asks of the host's worktrees ([`AgentEvent::WorktreeRequested`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeRequest {
    /// A git worktree called `name` of the checkout `cwd` is in, for the session
    /// or one of its native subagents; `cwd` is the directory asking.
    Create { cwd: PathBuf, name: String },
    /// Remove the worktree at `path`, which an earlier `Create` was answered with.
    Remove { path: PathBuf },
}

/// The host's answer to a [`WorktreeRequest`] ([`SessionCommand::RespondWorktree`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeAnswer {
    /// The worktree is at `path`: made for this request, or `reused` when the
    /// session asked again for a name it was given this worktree for.
    Ready { path: PathBuf, reused: bool },
    /// The worktree was removed.
    Removed,
    /// Nothing was made or removed, for `error`.
    Refused { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Codex aborts (`cancel`); Claude denies with "User cancelled tool execution";
    /// ACP reports cancelled; OpenCode replies reject.
    Cancel,
    /// Pick one of the provider's own [`ApprovalOption`]s.
    Option(String),
    /// Decline in the user's own words, which the agent continues from:
    /// Claude denies with them as the message, Grok and Cursor return them as
    /// plan feedback. A provider with no channel for words declines once.
    Feedback(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFreshness {
    /// Older logs lack provenance; their counts may be turn aggregates.
    #[default]
    Unknown,
    Current,
    LastKnown,
    Compacting,
    AwaitingObservation,
}

/// One `tcode_control` write, as the target thread records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlAction {
    pub actor_thread_id: String,
    pub actor_title: String,
    /// A semantic key from `tcode_core::control::action`, which clients put in words.
    pub action: String,
    /// Data only (a title, an id, a name, a count, someone's own words), never a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub in_progress: bool,
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
    /// Provider-reported compaction output size; not a new request observation.
    pub post_tokens: Option<u64>,
    pub dropped_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub freshness: ContextFreshness,
    /// Main-loop traffic for the completed turn, separate from occupancy.
    pub turn_processed_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total context currently in use, if the provider reports it.
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    /// Cumulative tokens processed over the session's lifetime, if known (Codex
    /// `thread/tokenUsage` running total; timeline accumulation of Claude turns).
    /// Shown as "Total processed" in the context-meter popover.
    pub total_processed_tokens: Option<u64>,
    /// Provider-reported cost for this turn/message, in US dollars.
    pub cost_usd: Option<f64>,
    /// Provider-reported turn duration, in milliseconds.
    pub duration_ms: Option<u64>,
}

impl TokenUsage {
    #[cfg(feature = "process")]
    fn merge(&mut self, usage: Self) {
        self.freshness = usage.freshness;
        self.input_tokens = add_token_counts(self.input_tokens, usage.input_tokens);
        self.cached_input_tokens =
            add_token_counts(self.cached_input_tokens, usage.cached_input_tokens);
        self.output_tokens = add_token_counts(self.output_tokens, usage.output_tokens);
        self.used_tokens = add_token_counts(self.used_tokens, usage.used_tokens);
        self.context_window = match (self.context_window, usage.context_window) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
        self.total_processed_tokens = usage.total_processed_tokens;
        self.cost_usd = match (self.cost_usd, usage.cost_usd) {
            (None, None) => None,
            (left, right) => Some(left.unwrap_or(0.0) + right.unwrap_or(0.0)),
        };
        self.duration_ms = match (self.duration_ms, usage.duration_ms) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (left, right) => left.or(right),
        };
    }
}

#[cfg(feature = "process")]
fn add_token_counts(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(feature = "process")]
fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_f64().map(|number| number.max(0.0) as u64))
    })
}

#[cfg(test)]
mod launch_env_tests {
    use super::*;

    /// A profile's home reaches each CLI as the variable that CLI reads its
    /// state directory from, over a configured pair of the same name.
    #[test]
    fn home_override_is_provider_specific_and_wins_over_configured_pairs() {
        for (provider, variables) in [
            (ProviderKind::ClaudeCode, &["CLAUDE_CONFIG_DIR"][..]),
            (ProviderKind::Codex, &["CODEX_HOME"]),
            (ProviderKind::Pi, &["PI_CODING_AGENT_DIR"]),
            (
                ProviderKind::Cursor,
                &["CURSOR_CONFIG_DIR", "CURSOR_DATA_DIR"],
            ),
            (ProviderKind::Grok, &["GROK_HOME"]),
            (ProviderKind::OpenCode, &[]),
            (ProviderKind::Acp, &[]),
        ] {
            let configured: Vec<(String, String)> = std::iter::once(("FOO", "bar"))
                .chain(
                    variables
                        .iter()
                        .map(|variable| (*variable, "/home/user/old")),
                )
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            let mut env = LaunchEnv {
                env: configured.clone(),
                home: None,
            };
            assert_eq!(env.pairs(provider), configured, "{provider:?}");
            env.home = Some(PathBuf::from("/home/user/profile"));
            let expected: Vec<_> = configured
                .iter()
                .cloned()
                .chain(
                    variables
                        .iter()
                        .map(|variable| ((*variable).to_owned(), "/home/user/profile".to_owned())),
                )
                .collect();
            assert_eq!(env.pairs(provider), expected, "{provider:?}");
            for variable in variables {
                assert_eq!(
                    env.child_path(provider, variable),
                    Some(PathBuf::from("/home/user/profile")),
                    "{provider:?}"
                );
            }
        }
    }
}

#[cfg(all(test, feature = "process"))]
mod resolve_binary_tests {
    use super::*;

    /// The documented Windows default, used when `PATHEXT` is unset.
    fn windows_pathext() -> Vec<String> {
        [".COM", ".EXE", ".BAT", ".CMD"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Windows' "is executable": a regular file (no exec bit exists there).
    #[cfg(windows)]
    fn is_file(path: &std::path::Path) -> bool {
        path.is_file()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tcode-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_paths_bypass_the_environment_path_search() {
        for path in [
            "/opt/custom/claude",
            r"C:\tools\claude.cmd",
            "C:/tools/claude.cmd",
            "relative/claude",
        ] {
            let explicit = std::path::Path::new(path);
            assert_eq!(
                resolve_binary(Some(explicit), "unrelated-name").unwrap(),
                explicit
            );
            assert_eq!(resolve_binary(None, path).unwrap(), explicit);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_search_requires_executable_files_and_absolute_directories() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("unix");
        let bin = dir.join("foo");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("bar"), "not executable").unwrap();
        std::fs::set_permissions(dir.join("bar"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let suffixed = dir.join("shim.exe");
        std::fs::write(&suffixed, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&suffixed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let extensions = path_extensions();
        assert_eq!(
            find_in_dirs([dir.clone()], "foo", &extensions, is_executable),
            Some(bin)
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "bar", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "shim", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "shim.exe", &extensions, is_executable),
            Some(suffixed)
        );
        assert_eq!(
            find_in_dirs([dir.clone()], ".", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([PathBuf::from("relative")], "foo", &extensions, |_| panic!(
                "relative PATH entries must not be probed"
            )),
            None
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The resolved file name, lowercased: the extension comes from `PATHEXT`
    /// (conventionally uppercase: `.EXE`), while the file on disk is usually
    /// lowercase. Both Windows and macOS resolve that case-insensitively, so the
    /// assertions below compare names case-insensitively too.
    #[cfg(windows)]
    fn resolved_name(path: Option<PathBuf>) -> Option<String> {
        Some(path?.file_name()?.to_string_lossy().to_lowercase())
    }

    /// PATHEXT order decides: `.EXE` beats `.CMD` when both exist.
    #[cfg(windows)]
    #[test]
    fn windows_honors_pathext_order_and_falls_back_to_the_bare_name() {
        let dir = temp_dir("win-order");
        std::fs::write(dir.join("codex.cmd"), "").unwrap();
        std::fs::write(dir.join("codex.exe"), "").unwrap();
        let found = find_in_dirs([dir], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex.exe".to_string()));

        // An extensionless file is still found — last, after every PATHEXT try.
        let bare = temp_dir("win-bare");
        std::fs::write(bare.join("codex"), "").unwrap();
        let found = find_in_dirs([bare], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex".to_string()));
    }

    /// A name that already has an extension is used verbatim (no PATHEXT loop):
    /// `claude.cmd` must not be probed as `claude.cmd.EXE`.
    #[test]
    fn windows_uses_an_explicit_extension_as_given() {
        assert_eq!(
            candidate_names("claude.cmd", &windows_pathext()),
            vec!["claude.cmd".to_string()]
        );
        assert_eq!(
            candidate_names("npm", &windows_pathext()),
            vec![
                "npm.COM".to_string(),
                "npm.EXE".to_string(),
                "npm.BAT".to_string(),
                "npm.CMD".to_string(),
                "npm".to_string(),
            ]
        );
    }
}

#[cfg(test)]
mod turn_diff_tests {
    use super::*;

    #[test]
    fn splits_multi_file_turn_diff_and_classifies_operations() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
            "diff --git a/old.txt b/old.txt\n",
            "deleted file mode 100644\n",
            "--- a/old.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-gone\n",
            "diff --git a/before.rs b/after.rs\n",
            "similarity index 100%\n",
            "rename from before.rs\n",
            "rename to after.rs\n",
        );

        let UnifiedDiff { changes, unread } = file_changes_from_unified_diff(diff);
        assert!(unread.is_empty());
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[0].kind, FileChangeKind::Modify);
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!(changes[1].kind, FileChangeKind::Create);
        assert_eq!(changes[2].path, "old.txt");
        assert_eq!(changes[2].kind, FileChangeKind::Delete);
        assert_eq!(changes[3].path, "after.rs");
        assert_eq!(changes[3].kind, FileChangeKind::Rename);
        assert!(changes[0].diff.as_deref().unwrap().contains("-old\n+new"));
        assert!(!changes[0].diff.as_deref().unwrap().contains("new.txt"));
    }

    #[test]
    fn an_empty_diff_changes_nothing_and_what_is_no_files_section_is_kept_as_written() {
        assert_eq!(file_changes_from_unified_diff("\n"), UnifiedDiff::default());
        let read = file_changes_from_unified_diff(concat!(
            "--- a/file\n+++ b/file\n",
            "diff --git a/kept.rs b/kept.rs\n--- a/kept.rs\n+++ b/kept.rs\n",
        ));
        assert_eq!(read.unread, ["--- a/file\n+++ b/file\n"]);
        assert_eq!(read.changes.len(), 1);
        assert_eq!(read.changes[0].path, "kept.rs");
    }
}

#[cfg(test)]
mod thread_item_serde_tests {
    use super::*;

    /// A call recorded before calls said what they are (v0.4.0-rc.3's
    /// `ToolCall`, which had no `family`) reads as a call of no family.
    #[test]
    fn a_call_recorded_without_a_family_reads_as_one_of_none() {
        let item: ThreadItem = serde_json::from_str(
            r#"{"id":"t1","content":{"kind":"tool_call","name":"Read","input":{"file_path":"a.rs"},"output":"1\tfn a() {}","status":"completed"}}"#,
        )
        .unwrap();
        assert!(matches!(
            item.content,
            ItemContent::ToolCall { family: None, ref name, .. } if name == "Read"
        ));
    }

    #[test]
    fn cursor_and_grok_use_their_wire_names() {
        for (kind, wire) in [
            (ProviderKind::Cursor, r#""cursor""#),
            (ProviderKind::Grok, r#""grok""#),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ProviderKind>(wire).unwrap(), kind);
        }
    }

    #[test]
    fn provider_failure_wire_tag_and_legacy_error_remain_compatible() {
        let event = AgentEvent::ProviderStartFailed {
            error: "spawn failed".into(),
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"provider_start_failed","error":"spawn failed"}"#
        );
        let legacy = r#"{"type":"error","message":"boom","fatal":true}"#;
        assert!(
            matches!(serde_json::from_str::<AgentEvent>(legacy).unwrap(),
            AgentEvent::Error { message, fatal: true } if message == "boom")
        );
    }

    #[test]
    fn user_input_delivery_is_async_only_when_recorded() {
        let legacy = r#"{"type":"user_input_requested","request_id":"q","questions":[]}"#;
        assert!(matches!(
            serde_json::from_str::<AgentEvent>(legacy).unwrap(),
            AgentEvent::UserInputRequested {
                delivery: UserInputDelivery::Blocking,
                ..
            }
        ));
        let event = AgentEvent::UserInputRequested {
            request_id: "q".into(),
            questions: Vec::new(),
            delivery: UserInputDelivery::Async,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"user_input_requested","request_id":"q","questions":[],"delivery":"async"}"#
        );
    }

    #[test]
    fn an_approval_carries_its_reason_and_older_rows_still_read() {
        let legacy = r#"{"type":"approval_requested","id":"r","turn_id":null,"kind":{"kind":"exec_command","command":"ls","cwd":null,"reason":"why"}}"#;
        let AgentEvent::ApprovalRequested(request) = serde_json::from_str(legacy).unwrap() else {
            panic!("an older approval row reads as an approval");
        };
        assert_eq!(
            request.kind,
            ApprovalKind::ExecCommand {
                command: "ls".into(),
                cwd: None,
                description: None,
            }
        );
        let legacy = r#"{"type":"approval_requested","id":"r","turn_id":null,"kind":{"kind":"file_change","changes":[],"reason":null}}"#;
        assert!(serde_json::from_str::<AgentEvent>(legacy).is_ok());

        let event = AgentEvent::ApprovalRequested(ApprovalRequest {
            id: "r".into(),
            turn_id: None,
            parent_item_id: None,
            kind: ApprovalKind::ExecCommand {
                command: "rm -rf $F/*".into(),
                cwd: None,
                description: Some("Clear the scratch directory".into()),
            },
            reason: Some("Dangerous rm operation detected".into()),
            blocked_path: Some("/tmp/scratch".into()),
            options: Vec::new(),
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"approval_requested","id":"r","turn_id":null,"kind":{"kind":"exec_command","command":"rm -rf $F/*","cwd":null,"description":"Clear the scratch directory"},"reason":"Dangerous rm operation detected","blocked_path":"/tmp/scratch"}"#
        );
    }

    #[test]
    fn assistant_messages_are_summarized_only_when_recorded() {
        let legacy = r#"{"type":"item_completed","id":"m","content":{"kind":"assistant_message","text":"hi"}}"#;
        assert!(matches!(
            serde_json::from_str::<AgentEvent>(legacy).unwrap(),
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage {
                    summarized: false,
                    ..
                },
                ..
            })
        ));
        let event = AgentEvent::ItemCompleted(ThreadItem {
            id: "m".into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage {
                text: "hi".into(),
                summarized: true,
            },
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"item_completed","id":"m","content":{"kind":"assistant_message","text":"hi","summarized":true}}"#
        );
    }

    #[test]
    fn stored_items_preserve_parent_links_and_read_legacy_records() {
        let legacy = r#"{"type":"item_completed","id":"old","content":{"kind":"user_message","text":"hello"}}"#;
        let decoded: AgentEvent = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::ItemCompleted(ThreadItem {
                parent_item_id: None,
                ..
            })
        ));
        let parented = r#"{"type":"item_completed","id":"child","parent_item_id":"spawn","content":{"kind":"assistant_message","text":"working"}}"#;
        let decoded: AgentEvent = serde_json::from_str(parented).unwrap();
        assert!(matches!(
            &decoded,
            AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), .. })
                if parent == "spawn"
        ));
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["parent_item_id"],
            "spawn"
        );
    }
}

#[cfg(test)]
mod usage_compatibility_tests {
    use super::*;

    #[test]
    fn compaction_and_usage_read_old_literal_records_without_inventing_provenance() {
        let event: AgentEvent = serde_json::from_str(r#"{"type":"context_compacted"}"#).unwrap();
        assert!(
            matches!(event, AgentEvent::ContextCompacted(c) if !c.in_progress && c.pre_tokens.is_none())
        );
        let usage: TokenUsage = serde_json::from_str(
            r#"{"used_tokens":4100000,"context_window":1000000,"total_processed_tokens":4100000}"#,
        )
        .unwrap();
        assert_eq!(usage.freshness, ContextFreshness::Unknown);
        assert_eq!(usage.total_processed_tokens, Some(4100000));
        let event = AgentEvent::ContextCompacted(Compaction {
            trigger: Some("manual".into()),
            pre_tokens: Some(500),
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"context_compacted","in_progress":false,"trigger":"manual","pre_tokens":500,"post_tokens":null,"dropped_tokens":null,"duration_ms":null}"#
        );
    }
}

/// Parse a `MAJOR.MINOR.PATCH` triple from `--version` output such as
/// `"2.1.206 (Claude Code)"` or a bare manifest version string.
pub(crate) fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().find(|token| token.contains('.'))?;
    let mut parts = token.trim_start_matches('v').split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()
            .and_then(|part| part.split(['-', '+']).next())?
            .parse()
            .ok()?,
    ))
}
