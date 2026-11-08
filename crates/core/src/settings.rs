//! Persisted application settings domain data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use agent::ProviderKind;
use serde::{Deserialize, Serialize};

use crate::acp::InstalledAcpAgent;
pub use crate::provider_colors::{PROVIDER_COLOR_PALETTE, builtin_provider_color};
use crate::pull_request::HostKind;
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlSettings {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSettings>,
    /// Host-authored discovery; never persisted in settings.json.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status: BTreeMap<String, HostStatus>,
}

impl SourceControlSettings {
    /// The kind a host is read as: the one settings give it, configured or found.
    pub fn kind(&self, host: &str) -> HostKind {
        HostKind::of(
            self.hosts
                .get(host)
                .map(|choice| choice.kind)
                .or_else(|| self.status.get(host).map(|status| status.kind)),
            host,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    pub kind: HostKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The chosen account when the host's CLI has several.
    #[serde(default)]
    pub account: Option<String>,
}

impl HostSettings {
    pub fn new(kind: HostKind) -> Self {
        Self {
            kind,
            enabled: true,
            account: None,
        }
    }
}

/// `github.hosts` as settings.json held it before hosts had kinds; read once and moved into
/// `source_control.hosts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGitHubSettings {
    #[serde(default)]
    pub hosts: BTreeMap<String, LegacyGitHubHost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGitHubHost {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub account: Option<String>,
}

/// Where the token Tcode uses for a host comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialSource {
    Saved,
    /// A saved token sent with the account's email, as the host's API tokens are.
    SavedBasic {
        email: String,
    },
    /// A saved token sent alone, as the host's access tokens are.
    SavedBearer,
    /// The environment variable's name.
    Env {
        name: String,
    },
    /// The CLI whose stored login supplies it.
    Cli {
        tool: String,
    },
}

/// What keeps a host from being read, when something does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostProblem {
    /// Nothing resolved: no token saved or in the environment, and these CLIs were not found.
    NoCredential { tools_missing: Vec<String> },
    /// The CLI exists and has no login for the host; `command` signs it in.
    NotSignedIn {
        tool: String,
        command: Option<String>,
    },
    /// The token in use is a saved one the host has not yet answered a request with, so
    /// nothing has shown that it works.
    Unverified,
    /// The host refused the token in use, saved or from the environment, with `status` at
    /// `at` (seconds since the Unix epoch). It stays refused until the token changes.
    Rejected { status: u16, at: u64 },
}

/// The account a token signs in as. Only names cross to clients, never the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAccount {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What a host answered when asked whose a token is, before the token is saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenCheck {
    /// The host took the token; `account` is whose it is, when the host says.
    Accepted { account: Option<HostAccount> },
    /// The host refused the token (401 or 403).
    Rejected { status: u16 },
    /// The token works but lacks these scopes, as the host names them.
    MissingScopes { scopes: Vec<String> },
    /// The host could not be asked: no answer (`status` none), or an answer about something
    /// other than the token.
    Unreachable { status: Option<u16> },
    /// The host never says whose a token of this kind is, so nothing can confirm it before use.
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub kind: HostKind,
    /// Only the user's settings name it: no CLI login, environment variable or default does.
    pub added: bool,
    pub token_set: bool,
    /// The source in use now; `None` when nothing resolved.
    pub source: Option<CredentialSource>,
    /// The CLI's accounts for the host, for choosing one.
    pub accounts: Vec<String>,
    pub env_overrides_account: bool,
    /// The host's resolution order: the first that works is used.
    pub order: Vec<CredentialSource>,
    pub problem: Option<HostProblem>,
    /// Where the user creates the token the host takes. A host whose token may instead be an
    /// access token sent alone explains those at `access_token_url`: they are created on the
    /// item they are scoped to, not on one page.
    pub token_url: Option<String>,
    pub access_token_url: Option<String>,
    /// The scopes a token needs, as the host's token page names them.
    pub token_scopes: Vec<String>,
    /// The account the token in use signs in as, once the host has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

/// How the sidebar's PROJECTS groups are ordered. Cycled by the sort button
/// next to the "PROJECTS" header and persisted in settings.json.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSort {
    /// Newest session activity first (default; the original behavior).
    #[default]
    RecentActivity,
    /// Project name, case-insensitive A-Z.
    NameAsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarLayout {
    /// One flat list of threads sorted by attention then recency (default).
    #[default]
    Flat,
    /// Threads grouped under project headers (the legacy layout).
    Grouped,
}

impl ProjectSort {
    /// The next mode in the cycle (RecentActivity → NameAsc → RecentActivity).
    pub fn next(self) -> Self {
        match self {
            ProjectSort::RecentActivity => ProjectSort::NameAsc,
            ProjectSort::NameAsc => ProjectSort::RecentActivity,
        }
    }
}

/// The stable settings-file key for a provider: its entry in `settings.json`'s
/// `providers` map. Profile-keyed data such as `secrets.json` uses
/// [`Settings::builtin_profile_id`].
pub fn provider_key(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Cursor => "cursor",
        ProviderKind::Grok => "grok",
        // ACP agents are not one provider but many: their per-agent settings
        // live in `Settings::acp_agents`, keyed by registry id. This bucket only
        // ever holds the shared fallbacks (it is never written by the ACP card).
        ProviderKind::Acp => "acp",
    }
}

/// The provider color key of an ACP agent: agents share `ProviderKind::Acp`,
/// so the registry id is what tells them apart.
pub fn acp_color_key(agent_id: &str) -> String {
    format!("acp:{agent_id}")
}

/// The provider's product name used for card titles and picker labels. ACP
/// agents are named by their own registry entry, so their kind reads as the
/// protocol alone.
pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Acp => "ACP",
        provider => provider.display_name(),
    }
}

/// One `KEY=VALUE` pair passed into a provider's child processes.
///
/// Sensitive rows never store their value here: it lives in `secrets.json`
/// (0600) and is never handed back to the UI, which renders the "Stored secret"
/// placeholder instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    /// Plaintext value for non-sensitive rows; always empty when `sensitive`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// Per-provider configuration (Settings → Harnesses card).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// Whether the provider may be used for new sessions.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional label shown in the provider list (falls back to the driver name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `#rrggbb` accent tinting the provider glyph in picker rails / model lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    /// Environment variables merged into every child process for this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    /// On a built-in card, the variables every account and endpoint profile
    /// of its harness gets before its own `env`, whose values win. Sensitive
    /// values are stored under [`harness_secret_id`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_env: Vec<EnvVar>,
    /// Override for the CLI binary (`None` = resolve from PATH). On a user
    /// profile, a binary of its own instead of its harness's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<PathBuf>,
    /// The provider's config-directory variable ([`agent::LaunchEnv::home`]),
    /// `CLAUDE_CONFIG_DIR` for Claude. OpenCode ignores this field because it
    /// has no single-directory override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_path: Option<PathBuf>,
    /// Native-provider CLI arguments appended on session start (ignored for Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub pi: PiProviderSettings,
    /// On a built-in card, the permission value a new thread of the harness
    /// starts with where its project names none (`None` = the recommended one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission: Option<String>,
    /// Model slugs added by hand in the Models section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    /// Model ids hidden from the composer's model picker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_models: Vec<String>,
}

/// Serializable edits the UI can make to one provider profile, one field at
/// a time. The harness-wide fields (binary, launch arguments, shared
/// environment, switches) are edits of its built-in profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProfileSettingsPatch {
    SetEnabled {
        enabled: bool,
    },
    /// Every profile of the harness at once.
    SetHarnessEnabled {
        enabled: bool,
    },
    SetDisplayName(Option<String>),
    SetAccentColor(Option<String>),
    SetEnv(Vec<EnvVar>),
    SetSharedEnv(Vec<EnvVar>),
    SetBinaryPath(Option<PathBuf>),
    /// Only a built-in profile's config directory is the user's to move; a
    /// profile's own directory holds its sign-in.
    SetHomePath(Option<PathBuf>),
    SetLaunchArgs(Option<String>),
    SetToggle {
        id: String,
        enabled: bool,
    },
    SetDefaultPermission(Option<String>),
    SetCustomModels(Vec<String>),
    SetHiddenModels(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiProviderSettings {
    /// Whether pi should trust and load the project's local `.pi` configuration.
    #[serde(default, rename = "pi_trust_project_extensions")]
    pub trust_project_extensions: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            display_name: None,
            accent_color: None,
            env: Vec::new(),
            shared_env: Vec::new(),
            binary_path: None,
            home_path: None,
            launch_args: None,
            pi: PiProviderSettings::default(),
            default_permission: None,
            custom_models: Vec::new(),
            hidden_models: Vec::new(),
        }
    }
}

impl ProviderSettings {
    /// The card's `accent_color` as `0xRRGGBB`, `None` when unset or not a
    /// six-digit hex string.
    pub fn accent_rgb(&self) -> Option<u32> {
        let hex = self.accent_color.as_deref()?.trim().trim_start_matches('#');
        (hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten()
    }

    /// The state of a harness switch by its [`agent::HarnessFacts::toggles`] id.
    pub fn toggle(&self, id: &str) -> bool {
        match id {
            agent::harness::TRUST_PROJECT_EXTENSIONS => self.pi.trust_project_extensions,
            _ => false,
        }
    }

    pub fn set_toggle(&mut self, id: &str, enabled: bool) {
        if id == agent::harness::TRUST_PROJECT_EXTENSIONS {
            self.pi.trust_project_extensions = enabled;
        }
    }

    /// A native provider's `Launch arguments` field, split on whitespace.
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// A user-created provider profile (Settings → Harnesses "+ New profile").
///
/// A profile pairs a *protocol* ([`ProviderKind`] — which native CLI/adapter
/// spawns it) with a full [`ProviderSettings`] card. Several profiles may share
/// one protocol, which is how a session can talk to the official Anthropic API
/// *and* a third-party Anthropic-compatible endpoint at the same time: both are
/// `ProviderKind::ClaudeCode`, each with its own `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL` env and its own config directory.
///
/// The built-in profiles are not stored here — they
/// remain in [`Settings::providers`] under their [`provider_key`]. Only extra,
/// user-created profiles live in [`Settings::profiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// The protocol this profile drives. Determines which native adapter
    /// spawns it and how its native protocol is normalized.
    pub kind: ProviderKind,
    /// The card configuration (env, binary, home, models, display name, …).
    /// Flattened so a profile's JSON is a superset of a provider card's.
    #[serde(flatten)]
    pub settings: ProviderSettings,
}

/// A profile resolved to the two things the launch path needs: which protocol
/// to speak, and the effective card settings to spawn with. Produced by
/// [`Settings::resolved_profile`] for both built-in and user profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Stable profile id: a built-in [`provider_key`] or a user-chosen slug.
    pub id: String,
    /// The protocol this profile drives.
    pub kind: ProviderKind,
    /// The effective card settings: a user profile's binary and launch
    /// arguments fall back to its harness's, and its switches are the
    /// harness's.
    pub settings: ProviderSettings,
    /// The harness's shared variables, which `settings.env` follows.
    pub shared_env: Vec<EnvVar>,
}

impl ResolvedProfile {
    /// Whether Tcode signs this profile in and out of its native account
    /// through the CLI.
    pub fn supports_account_login(&self) -> bool {
        self.kind.caps().account_login && self.supports_account_usage()
    }

    /// Whether this endpoint/auth configuration can query native account limits.
    pub fn supports_account_usage(&self) -> bool {
        let Some(endpoint) = self.kind.harness().endpoint else {
            return false;
        };
        let env: Vec<&EnvVar> = self.shared_env.iter().chain(&self.settings.env).collect();
        // Secret presence is enough to classify API-key auth; never inspect or
        // replicate secret values. Missing native sign-in remains a probe error.
        !env.iter().enumerate().any(|(index, var)| {
            // LaunchEnv uses the last occurrence of a repeated environment key.
            if env[index + 1..].iter().any(|later| later.name == var.name) {
                return false;
            }
            let configured = var.sensitive || !var.value.trim().is_empty();
            configured
                && (endpoint.key_envs.contains(&var.name.as_str())
                    || (endpoint.backend_switches.contains(&var.name.as_str())
                        && var.value != "0"
                        && var.value != "false")
                    || (var.name == endpoint.base_url_env
                        && (var.sensitive
                            || var.value.trim().trim_end_matches('/').to_ascii_lowercase()
                                != endpoint.native_base_url)))
        })
    }

    /// Whether this profile is an endpoint profile: one that reaches a model
    /// API through its own variables rather than an account.
    pub fn is_endpoint_profile(&self) -> bool {
        let Some(endpoint) = self.kind.harness().endpoint else {
            return false;
        };
        !Settings::is_builtin_profile_id(&self.id)
            && self.settings.env.iter().any(|var| {
                var.name == endpoint.base_url_env || endpoint.key_envs.contains(&var.name.as_str())
            })
    }
}

/// Who answers the permission requests and questions raised by dispatched
/// child threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChildApprovalMode {
    Orchestrator,
    AlwaysAllow,
    Manual,
    #[default]
    #[serde(other)]
    Auto,
}

/// How `tcode_control` starts a subagent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentSettings {
    pub child_approval: ChildApprovalMode,
    /// Give subagents dedicated Git worktrees when their resolved cwd is itself a
    /// repository root. `start_thread`'s `workspace` overrides this.
    pub child_worktrees: bool,
}

impl SubagentSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// A star in the model picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FavoriteModel {
    /// One model under one account: `profile_id` is `None` on the provider's
    /// built-in profile, as on a thread.
    Row {
        provider: ProviderKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        model: String,
    },
    /// An id saved before stars named an account. Nothing records which
    /// provider or account it was starred under, so it stars every row with
    /// that id.
    Id(String),
}

impl FavoriteModel {
    /// The model id this star lights on an account, if any.
    fn id_on(&self, provider: ProviderKind, profile_id: Option<&str>) -> Option<&str> {
        match self {
            Self::Row {
                provider: starred_provider,
                profile_id: starred_profile,
                model,
            } => (*starred_provider == provider && starred_profile.as_deref() == profile_id)
                .then_some(model),
            Self::Id(id) => Some(id),
        }
    }
}

/// Provider and model used for the isolated, background request that names a
/// thread, initially or on request. Reasoning effort is intentionally fixed to
/// `low` by the runtime: title generation is a small, latency-sensitive task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleGenerationSettings {
    #[serde(default = "default_title_provider")]
    pub provider: ProviderKind,
    #[serde(default = "default_title_model")]
    pub model: String,
    /// Which provider profile (endpoint config) the dispatch launches against;
    /// `None` = the kind's built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

pub const DEFAULT_TITLE_MODEL: &str = "gpt-5.6-luna";

fn default_title_provider() -> ProviderKind {
    ProviderKind::Codex
}

fn default_title_model() -> String {
    DEFAULT_TITLE_MODEL.to_string()
}

impl Default for TitleGenerationSettings {
    fn default() -> Self {
        Self {
            provider: default_title_provider(),
            model: default_title_model(),
            profile_id: None,
        }
    }
}

impl TitleGenerationSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// When a computer-use observation carries a screenshot alongside the folded
/// accessibility outline. Mirrors `computer_use_mcp::config::ImageMode`; kept in
/// core so settings stay GPUI/backend-free and the app maps one to the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMode {
    /// Screenshot only when the outline looks too sparse to act on (default).
    #[default]
    Auto,
    /// Always attach a screenshot.
    Always,
    /// Never attach a screenshot (outline only).
    Never,
}

/// Global configuration for desktop computer-use tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerUseSettings {
    /// Whether newly spawned provider sessions receive the computer-use MCP server.
    #[serde(default)]
    pub enabled: bool,
    /// When observations include a screenshot.
    #[serde(default)]
    pub image_mode: ImageMode,
    /// When false the tools are observe-only (`act_ui` rejects every action).
    #[serde(default = "default_true")]
    pub allow_input: bool,
    /// Show the agent cursor overlay.
    #[serde(default = "default_true")]
    pub show_agent_cursor: bool,
}

impl Default for ComputerUseSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            image_mode: ImageMode::default(),
            allow_input: true,
            show_agent_cursor: true,
        }
    }
}

/// Settings for the embedded preview browser (Settings → Preview) and the
/// `tcode_preview` MCP server it backs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSettings {
    /// Whether the embedded browser and its preview MCP tools are available.
    /// Defaults to TRUE; absent in legacy files → enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Initial page opened when the preview panel is shown without a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_url: Option<String>,
    /// Whether the `preview_evaluate` MCP tool may run JavaScript. Defaults to
    /// TRUE; absent in legacy files → allowed.
    #[serde(default = "default_true")]
    pub allow_evaluate: bool,
}

/// A field-scoped mutation of persisted application settings.
///
/// Keeping nested settings mutations field-scoped prevents a writer holding a
/// stale snapshot from replacing unrelated fields changed by another writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum SettingsPatch {
    /// Adds the host with `kind` when the settings do not list it yet; the kind of a listed host
    /// never changes.
    SourceControlHost {
        host: String,
        kind: HostKind,
        enabled: Option<bool>,
        account: Option<Option<String>>,
    },
    RemoveSourceControlHost {
        host: String,
    },
    Language(Option<String>),
    ThemeMode(ThemeMode),
    WordWrapDiffs(bool),
    SkipDeleteConfirmation(bool),
    LiveCommandPanelDisabled(bool),
    SidebarProviderMarks(bool),
    ProviderUpdateChecksDisabled(bool),
    InactiveFrameThrottleDisabled(bool),
    ScrollToLatestOnSend(bool),
    ResumeOnLimitReset(bool),
    AutoSettleAfterDays(Option<f64>),
    AutoSettleOnMerge(bool),
    ProjectSettlement {
        project_id: String,
        value: Option<ProjectSettlementSettings>,
    },
    ProjectMergeMethod {
        project_id: String,
        method: crate::pull_request::PullRequestMergeMethod,
    },
    RemoveAgentCreditsOnMerge(bool),
    OrchestrateChildApproval(ChildApprovalMode),
    OrchestrateChildWorktrees(bool),
    ComputerUseEnabled(bool),
    ComputerUseImageMode(ImageMode),
    ComputerUseAllowInput(bool),
    ComputerUseShowAgentCursor(bool),
    BrowserEnabled(bool),
    BrowserHomeUrl(Option<String>),
    BrowserAllowEvaluate(bool),
    ControlEnabled(bool),
    ControlDefaultGrant(crate::control::ControlGrant),
    ControlProjectGrant {
        project_id: String,
        value: Option<crate::control::ControlGrant>,
    },
    SettlementAutomation(crate::control::SettlementAutomation),
    TitleGenerationProvider(ProviderKind),
    TitleGenerationModel(String),
    TitleGenerationProfileId(Option<String>),
    SidebarLayout(SidebarLayout),
    RemoteHostingEnabled(bool),
    Traverse(TraverseSetting),
    RemoteHostName(Option<String>),
    LastProject(Option<String>),
    NewProjectDir(Option<String>),
    ProjectStart(ProjectStart),
    ProjectStartCustom(Option<String>),
}

/// Where Create puts a new project when Settings names no folder.
pub const DEFAULT_NEW_PROJECT_DIR: &str = "~/TcodeProjects";

/// A folder Settings name on the host: `~` (its home) or a path under it, or
/// an absolute path by this host's rules. A blank one is no folder.
fn host_folder(value: Option<String>) -> Result<Option<String>, &'static str> {
    let Some(value) = value.filter(|dir| !dir.trim().is_empty()) else {
        return Ok(None);
    };
    let path = std::path::Path::new(value.trim());
    if path.strip_prefix("~").is_ok() || path.is_absolute() {
        Ok(Some(value))
    } else {
        Err("A folder must be an absolute path, ~ or a path under ~.")
    }
}

/// Where Add Project's folder list opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStart {
    /// The host's home.
    #[default]
    Home,
    /// Where Create puts a new project.
    NewProjectDir,
    /// The folder `Settings.project_start_custom` names.
    Custom,
}

impl ProjectStart {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The Traverse instances this machine publishes to while hosting: relay
/// fallback and wide-area discovery for devices off the LAN. The official
/// service is always the first source and is only ever disabled, never
/// removed; with every source disabled the machine uses no relay and no
/// wide-area discovery, and devices reach it on the LAN (DNS-SD) or at an
/// address the user types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TraverseSettingFile")]
pub struct TraverseSetting {
    pub sources: Vec<TraverseSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseSource {
    #[serde(flatten)]
    pub instance: TraverseInstance,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraverseInstance {
    Official,
    /// A self-hosted instance, by the base URL its manifest is served from.
    Custom {
        url: String,
    },
}

/// Every shape a settings file has held: the source list, or the single
/// choice written before a machine could publish to several instances.
#[derive(Deserialize)]
#[serde(untagged)]
enum TraverseSettingFile {
    Sources { sources: Vec<TraverseSource> },
    Mode(TraverseMode),
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum TraverseMode {
    Official,
    Custom { url: String },
    Off,
}

impl From<TraverseSettingFile> for TraverseSetting {
    fn from(file: TraverseSettingFile) -> Self {
        let official = |enabled| TraverseSource {
            instance: TraverseInstance::Official,
            enabled,
        };
        let sources = match file {
            TraverseSettingFile::Sources { sources } => sources,
            TraverseSettingFile::Mode(TraverseMode::Official) => vec![official(true)],
            TraverseSettingFile::Mode(TraverseMode::Off) => vec![official(false)],
            // The old choice was either-or: a self-hosted instance replaced
            // the official one.
            TraverseSettingFile::Mode(TraverseMode::Custom { url }) => vec![
                official(false),
                TraverseSource {
                    instance: TraverseInstance::Custom { url },
                    enabled: true,
                },
            ],
        };
        Self::new(sources)
    }
}

impl Default for TraverseSetting {
    fn default() -> Self {
        Self {
            sources: vec![TraverseSource {
                instance: TraverseInstance::Official,
                enabled: true,
            }],
        }
    }
}

impl TraverseSetting {
    /// `sources` with the official service first exactly once, keeping the
    /// first official entry's switch, and each self-hosted URL once. A list
    /// that names no official entry leaves it off, as a self-hosted choice
    /// always has.
    pub fn new(sources: Vec<TraverseSource>) -> Self {
        let official = sources
            .iter()
            .find(|source| source.instance == TraverseInstance::Official)
            .is_some_and(|source| source.enabled);
        let mut normalized = vec![TraverseSource {
            instance: TraverseInstance::Official,
            enabled: official,
        }];
        for source in sources {
            if !normalized
                .iter()
                .any(|kept| kept.instance == source.instance)
            {
                normalized.push(source);
            }
        }
        Self {
            sources: normalized,
        }
    }

    pub fn enabled(&self) -> impl Iterator<Item = &TraverseInstance> {
        self.sources
            .iter()
            .filter(|source| source.enabled)
            .map(|source| &source.instance)
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            home_url: None,
            allow_evaluate: true,
        }
    }
}

impl BrowserSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

// `Eq` is intentionally absent: `acp_agents` holds `AcpLaunch`, which the
// agent crate derives only `PartialEq` for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub source_control: SourceControlSettings,
    /// Legacy input: moved into `source_control` on load; never written back.
    #[serde(default, skip_serializing)]
    pub github: Option<LegacyGitHubSettings>,
    /// None follows the operating-system language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-provider cards (Settings → Harnesses), keyed by [`provider_key`].
    /// These are the built-in native-provider profiles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderSettings>,
    /// User-created provider profiles, keyed by a stable slug id. Each carries
    /// its own [`ProviderKind`], so multiple profiles can drive the same
    /// protocol (e.g. official Claude + a third-party endpoint). Built-in
    /// profiles are *not* here — they live in `providers`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, ProviderProfile>,
    /// Legacy (pre-`providers`) binary overrides. Read once and migrated into
    /// `providers` on load; never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_binary: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_binary: Option<PathBuf>,
    #[serde(default)]
    pub theme_mode: ThemeMode,
    /// Whether the sidebar is collapsed to its icon strip. Persisted so the
    /// choice survives a restart (absent in legacy files → expanded).
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Default soft-wrap for long lines in the diff panel. Tolerantly added:
    /// absent in legacy settings.json files (defaults to off).
    #[serde(default)]
    pub word_wrap_diffs: bool,
    /// When true, the inline archive/delete action skips its confirm dialog.
    /// Stored inverted so legacy files (field absent → false) keep the confirm
    /// dialog on by default. Surfaced as the "Delete confirmation" toggle.
    #[serde(default)]
    pub skip_delete_confirmation: bool,
    /// Whether the live command panel is DISABLED. Stored inverted so absent
    /// legacy settings keep the feature enabled.
    #[serde(default)]
    pub live_command_panel_disabled: bool,
    /// Whether sidebar thread rows show their provider's mark. Off by
    /// default and absent in legacy files.
    #[serde(default)]
    pub sidebar_provider_marks: bool,
    /// Whether the on-launch provider version check is DISABLED. Stored inverted
    /// so it remains enabled for legacy settings files that lack the field.
    #[serde(default)]
    pub provider_update_checks_disabled: bool,
    /// Whether inactive-window frame throttling is DISABLED. Stored inverted so
    /// the throttle defaults to on even for legacy settings files that lack the field.
    #[serde(default)]
    pub inactive_frame_throttle_disabled: bool,
    /// Whether a conversation scrolled back from its end goes to it once a
    /// message sent from it reaches the host.
    #[serde(default = "default_true")]
    pub scroll_to_latest_on_send: bool,
    #[serde(default = "default_true")]
    pub resume_on_limit_reset: bool,
    #[serde(default = "default_auto_settle_after_days")]
    pub auto_settle_after_days: Option<f64>,
    #[serde(default)]
    pub auto_settle_on_merge: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_settlement_overrides: BTreeMap<String, ProjectSettlementSettings>,
    /// The merge method a project's pull requests are merged with unless another is chosen,
    /// set from the merge confirmation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_merge_methods: BTreeMap<String, crate::pull_request::PullRequestMergeMethod>,
    /// Whether a merge leaves agents' credit lines out of GitHub's merge message by default.
    #[serde(default)]
    pub remove_agent_credits_on_merge: bool,
    /// Stored under the name of the removed Orchestrate page, so files that
    /// page wrote keep these values; its model lists there are ignored.
    #[serde(
        default,
        rename = "orchestrate",
        skip_serializing_if = "SubagentSettings::is_default"
    )]
    pub subagents: SubagentSettings,
    /// Global desktop computer-use feature settings. Absent in legacy files,
    /// where the feature remains disabled by default.
    #[serde(default)]
    pub computer_use: ComputerUseSettings,
    /// Embedded preview browser settings. Skipped when default (like
    /// `subagents`), so legacy files stay clean and load with the defaults.
    #[serde(default, skip_serializing_if = "BrowserSettings::is_default")]
    pub browser: BrowserSettings,
    /// What a thread's model may do in Tcode through `tcode_control`.
    #[serde(
        default,
        skip_serializing_if = "crate::control::ControlSettings::is_default"
    )]
    pub control: crate::control::ControlSettings,
    #[serde(
        default,
        skip_serializing_if = "crate::control::SettlementAutomation::is_default"
    )]
    pub settlement_automation: crate::control::SettlementAutomation,
    /// Provider/model used to generate a concise title for new threads.
    #[serde(default, skip_serializing_if = "TitleGenerationSettings::is_default")]
    pub title_generation: TitleGenerationSettings,
    /// Ids of project groups the user has collapsed in the sidebar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_projects: Vec<String>,
    /// Models the user has starred in the model picker (favorites float to
    /// the top and are shown first under the star filter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_models: Vec<FavoriteModel>,
    /// Sidebar PROJECTS ordering (cycled by the sort button).
    #[serde(default)]
    pub project_sort: ProjectSort,
    /// Sidebar thread layout (flat by default; grouped keeps the legacy view).
    #[serde(default)]
    pub sidebar_layout: SidebarLayout,
    /// Whether this desktop app also serves the remote protocol to other tcode
    /// clients. Absent in legacy files → hosting off.
    #[serde(default)]
    pub remote_hosting_enabled: bool,
    /// Traverse instances used while hosting. Absent in legacy files →
    /// official.
    #[serde(default, skip_serializing_if = "TraverseSetting::is_default")]
    pub traverse: TraverseSetting,
    /// Name this host advertises while pairing and on the discovery beacon.
    /// None uses the machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host_name: Option<String>,
    /// Per-session last-visited time (unix secs), keyed by session id. A session
    /// whose `updated_at` exceeds its last-visited time (and isn't active) shows
    /// an unread dot. A client that has loaded a thread advances it to the
    /// `updated_at` it showed; "Mark unread" sets it just below.
    /// UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_visited: HashMap<String, u64>,
    /// Project the user last navigated to or started a thread in. A workspace
    /// with no conversation open (launch, or the thread on screen going away)
    /// opens this project's new-thread draft. Set from user navigation only, so
    /// background activity cannot move it. UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_project_id: Option<String>,
    /// The host directory Create puts a new project in, one folder each, `~`
    /// standing for the host's home. `None` is [`DEFAULT_NEW_PROJECT_DIR`];
    /// absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_project_dir: Option<String>,
    /// Where Add Project's folder list opens; absent in legacy files.
    #[serde(default, skip_serializing_if = "ProjectStart::is_default")]
    pub project_start: ProjectStart,
    /// The folder [`ProjectStart::Custom`] opens on, kept while another choice
    /// is made so choosing Custom again brings it back. `~` stands for the
    /// host's home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_start_custom: Option<String>,
    /// ACP agents the user installed from the marketplace (or defined by hand),
    /// keyed by registry id. Each carries its resolved launch recipe, so a
    /// session can start without consulting the registry again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub acp_agents: BTreeMap<String, InstalledAcpAgent>,
    /// Keys this build does not know about, preserved verbatim on save.
    ///
    /// Without this, an older build (or any build predating a field) would drop
    /// the unknown key on load and silently destroy it on the next save — one
    /// downgrade, and your installed ACP agents or provider config are gone.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

const fn default_auto_settle_after_days() -> Option<f64> {
    Some(3.0)
}

/// Auto-settle after 1 to 90 days, or never.
fn auto_settle_days(days: Option<f64>) -> Result<Option<f64>, &'static str> {
    match days {
        Some(days) if !(1.0..=90.0).contains(&days) => {
            Err("Auto-settle days must be between 1 and 90.")
        }
        days => Ok(days),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettlementSettings {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_days_override"
    )]
    pub auto_settle_after_days: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_settle_on_merge: Option<bool>,
}

fn deserialize_days_override<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(deserializer).map(Some)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_control: SourceControlSettings::default(),
            github: None,
            language: None,
            providers: BTreeMap::new(),
            profiles: BTreeMap::new(),
            codex_binary: None,
            claude_binary: None,
            theme_mode: ThemeMode::default(),
            sidebar_collapsed: false,
            word_wrap_diffs: false,
            skip_delete_confirmation: false,
            live_command_panel_disabled: false,
            sidebar_provider_marks: false,
            provider_update_checks_disabled: false,
            inactive_frame_throttle_disabled: false,
            scroll_to_latest_on_send: true,
            resume_on_limit_reset: true,
            auto_settle_after_days: default_auto_settle_after_days(),
            auto_settle_on_merge: false,
            project_settlement_overrides: BTreeMap::new(),
            project_merge_methods: BTreeMap::new(),
            remove_agent_credits_on_merge: false,
            subagents: SubagentSettings::default(),
            computer_use: ComputerUseSettings::default(),
            browser: BrowserSettings::default(),
            control: crate::control::ControlSettings::default(),
            settlement_automation: crate::control::SettlementAutomation::default(),
            title_generation: TitleGenerationSettings::default(),
            collapsed_projects: Vec::new(),
            favorite_models: Vec::new(),
            project_sort: ProjectSort::default(),
            sidebar_layout: SidebarLayout::default(),
            remote_hosting_enabled: false,
            traverse: TraverseSetting::default(),
            remote_host_name: None,
            last_visited: HashMap::new(),
            last_project_id: None,
            new_project_dir: None,
            project_start: ProjectStart::default(),
            project_start_custom: None,
            acp_agents: BTreeMap::new(),
            unknown: serde_json::Map::new(),
        }
    }
}

impl Settings {
    /// Where Create puts a new project, `~` standing for the host's home.
    pub fn new_project_dir(&self) -> &str {
        self.new_project_dir
            .as_deref()
            .unwrap_or(DEFAULT_NEW_PROJECT_DIR)
    }

    /// The folder Add Project's list opens on, `~` standing for the host's
    /// home: a Custom choice with no folder named yet opens on the home.
    pub fn project_start_dir(&self) -> &str {
        match self.project_start {
            ProjectStart::Home => "~",
            ProjectStart::NewProjectDir => self.new_project_dir(),
            ProjectStart::Custom => self.project_start_custom.as_deref().unwrap_or("~"),
        }
    }

    /// Apply one field-scoped mutation without replacing sibling fields. A
    /// value outside its field's range is refused and changes nothing.
    pub fn apply(&mut self, patch: SettingsPatch) -> Result<(), &'static str> {
        match patch {
            SettingsPatch::SourceControlHost {
                host,
                kind,
                enabled,
                account,
            } => {
                let entry = self
                    .source_control
                    .hosts
                    .entry(host.trim().to_ascii_lowercase())
                    .or_insert_with(|| HostSettings::new(kind));
                if entry.kind != kind {
                    return Err("A host's kind does not change.");
                }
                if let Some(enabled) = enabled {
                    entry.enabled = enabled;
                }
                if let Some(account) = account {
                    entry.account = account.filter(|value| !value.trim().is_empty());
                }
            }
            SettingsPatch::RemoveSourceControlHost { host } => {
                self.source_control
                    .hosts
                    .remove(&host.trim().to_ascii_lowercase());
            }
            SettingsPatch::Language(value) => self.language = value,
            SettingsPatch::ThemeMode(value) => self.theme_mode = value,
            SettingsPatch::WordWrapDiffs(value) => self.word_wrap_diffs = value,
            SettingsPatch::SkipDeleteConfirmation(value) => {
                self.skip_delete_confirmation = value;
            }
            SettingsPatch::LiveCommandPanelDisabled(value) => {
                self.live_command_panel_disabled = value;
            }
            SettingsPatch::SidebarProviderMarks(value) => {
                self.sidebar_provider_marks = value;
            }
            SettingsPatch::ProviderUpdateChecksDisabled(value) => {
                self.provider_update_checks_disabled = value;
            }
            SettingsPatch::InactiveFrameThrottleDisabled(value) => {
                self.inactive_frame_throttle_disabled = value;
            }
            SettingsPatch::ScrollToLatestOnSend(value) => {
                self.scroll_to_latest_on_send = value;
            }
            SettingsPatch::ResumeOnLimitReset(value) => {
                self.resume_on_limit_reset = value;
            }
            SettingsPatch::AutoSettleAfterDays(value) => {
                self.auto_settle_after_days = auto_settle_days(value)?;
            }
            SettingsPatch::AutoSettleOnMerge(value) => self.auto_settle_on_merge = value,
            SettingsPatch::ProjectSettlement { project_id, value } => {
                if let Some(mut value) = value {
                    if let Some(days) = value.auto_settle_after_days {
                        value.auto_settle_after_days = Some(auto_settle_days(days)?);
                    }
                    self.project_settlement_overrides.insert(project_id, value);
                } else {
                    self.project_settlement_overrides.remove(&project_id);
                }
            }
            SettingsPatch::ProjectMergeMethod { project_id, method } => {
                self.project_merge_methods.insert(project_id, method);
            }
            SettingsPatch::RemoveAgentCreditsOnMerge(value) => {
                self.remove_agent_credits_on_merge = value;
            }
            SettingsPatch::OrchestrateChildApproval(value) => {
                self.subagents.child_approval = value;
            }
            SettingsPatch::OrchestrateChildWorktrees(value) => {
                self.subagents.child_worktrees = value;
            }
            SettingsPatch::ComputerUseEnabled(value) => self.computer_use.enabled = value,
            SettingsPatch::ComputerUseImageMode(value) => self.computer_use.image_mode = value,
            SettingsPatch::ComputerUseAllowInput(value) => self.computer_use.allow_input = value,
            SettingsPatch::ComputerUseShowAgentCursor(value) => {
                self.computer_use.show_agent_cursor = value
            }
            SettingsPatch::BrowserEnabled(value) => self.browser.enabled = value,
            SettingsPatch::BrowserHomeUrl(value) => self.browser.home_url = value,
            SettingsPatch::BrowserAllowEvaluate(value) => self.browser.allow_evaluate = value,
            SettingsPatch::ControlEnabled(value) => self.control.enabled = value,
            SettingsPatch::ControlDefaultGrant(value) => self.control.default_grant = value,
            SettingsPatch::ControlProjectGrant { project_id, value } => match value {
                Some(value) => {
                    self.control.project_overrides.insert(project_id, value);
                }
                None => {
                    self.control.project_overrides.remove(&project_id);
                }
            },
            SettingsPatch::SettlementAutomation(value) => self.settlement_automation = value,
            SettingsPatch::TitleGenerationProvider(value) => {
                self.title_generation.provider = value;
            }
            SettingsPatch::TitleGenerationModel(value) => self.title_generation.model = value,
            SettingsPatch::TitleGenerationProfileId(value) => {
                self.title_generation.profile_id = value;
            }
            SettingsPatch::SidebarLayout(value) => self.sidebar_layout = value,
            SettingsPatch::RemoteHostingEnabled(value) => self.remote_hosting_enabled = value,
            SettingsPatch::Traverse(value) => self.traverse = TraverseSetting::new(value.sources),
            SettingsPatch::RemoteHostName(value) => self.remote_host_name = value,
            SettingsPatch::LastProject(value) => self.last_project_id = value,
            SettingsPatch::NewProjectDir(value) => {
                self.new_project_dir = host_folder(value)?;
            }
            SettingsPatch::ProjectStart(value) => self.project_start = value,
            SettingsPatch::ProjectStartCustom(value) => {
                self.project_start_custom = host_folder(value)?;
            }
        }
        Ok(())
    }
}

impl Settings {
    /// This provider's card settings (defaults when never configured).
    pub fn provider(&self, provider: ProviderKind) -> ProviderSettings {
        self.providers
            .get(provider_key(provider))
            .cloned()
            .unwrap_or_default()
    }

    /// Mutable access, inserting defaults on first write.
    pub fn provider_mut(&mut self, provider: ProviderKind) -> &mut ProviderSettings {
        self.providers
            .entry(provider_key(provider).to_string())
            .or_default()
    }

    /// The built-in profile id for a native protocol. This is the id a session
    /// carries when it uses the default, non-custom configuration for its kind.
    ///
    /// Built-in ids resolve before user profiles, whose ids are slugs of ASCII
    /// alphanumerics and hyphens. Providers added after user profiles existed
    /// therefore take a `native:` id no slug can produce, so a user profile
    /// that already has the provider's name keeps resolving to itself.
    pub fn builtin_profile_id(kind: ProviderKind) -> &'static str {
        match kind {
            ProviderKind::Cursor => "native:cursor",
            ProviderKind::Grok => "native:grok",
            ProviderKind::ClaudeCode
            | ProviderKind::Codex
            | ProviderKind::Pi
            | ProviderKind::OpenCode
            | ProviderKind::Acp => provider_key(kind),
        }
    }

    /// The model ids starred on one account.
    pub fn favorite_ids(&self, provider: ProviderKind, profile_id: &str) -> Vec<String> {
        let profile_id = Some(profile_id).filter(|id| !Self::is_builtin_profile_id(id));
        self.favorite_models
            .iter()
            .filter_map(|favorite| favorite.id_on(provider, profile_id))
            .map(str::to_owned)
            .collect()
    }

    /// Star a model on one account, or take the star off. Taking off a star
    /// saved as a bare id takes it off every account it lit.
    pub fn toggle_favorite_model(
        &mut self,
        provider: ProviderKind,
        profile_id: Option<String>,
        model: String,
    ) {
        let profile_id = profile_id.filter(|id| !Self::is_builtin_profile_id(id));
        let starred = self.favorite_models.len();
        self.favorite_models.retain(|favorite| {
            favorite.id_on(provider, profile_id.as_deref()) != Some(model.as_str())
        });
        if self.favorite_models.len() == starred {
            self.favorite_models.push(FavoriteModel::Row {
                provider,
                profile_id,
                model,
            });
        }
    }

    /// Whether `id` names a built-in provider profile.
    pub fn is_builtin_profile_id(id: &str) -> bool {
        Self::builtin_kind_from_id(id).is_some()
    }

    /// The protocol kind of a built-in profile id, if it is one. Used to route a
    /// mutation of a built-in profile back to its `providers` card.
    pub fn builtin_kind_from_id(id: &str) -> Option<ProviderKind> {
        ProviderKind::NATIVE
            .into_iter()
            .chain([ProviderKind::Acp])
            .find(|kind| Self::builtin_profile_id(*kind) == id)
    }

    /// Resolve a profile id to its protocol kind and effective card settings.
    /// Built-in ids resolve to the matching `providers` card; anything else to
    /// a user-created `profiles` entry. `None` for an unknown id.
    pub fn resolved_profile(&self, id: &str) -> Option<ResolvedProfile> {
        if let Some(kind) = Self::builtin_kind_from_id(id) {
            let settings = self.provider(kind);
            return Some(ResolvedProfile {
                id: id.to_string(),
                kind,
                shared_env: settings.shared_env.clone(),
                settings,
            });
        }
        let profile = self.profiles.get(id)?;
        Some(self.resolve_user_profile(id, profile))
    }

    /// A user profile on top of its harness: the harness's binary, launch
    /// arguments and switches unless the profile has a binary or arguments
    /// of its own.
    fn resolve_user_profile(&self, id: &str, profile: &ProviderProfile) -> ResolvedProfile {
        let harness = self.provider(profile.kind);
        let mut settings = profile.settings.clone();
        settings.binary_path = settings.binary_path.or(harness.binary_path);
        settings.launch_args = settings.launch_args.or(harness.launch_args);
        settings.pi = harness.pi;
        settings.shared_env = Vec::new();
        ResolvedProfile {
            id: id.to_string(),
            kind: profile.kind,
            settings,
            shared_env: harness.shared_env,
        }
    }

    /// The secrets key of a harness's shared variables.
    pub fn harness_secret_id(kind: ProviderKind) -> String {
        format!("harness:{}", provider_key(kind))
    }

    /// The secrets key of an installed ACP agent's variables.
    pub fn acp_secret_id(agent_id: &str) -> String {
        acp_color_key(agent_id)
    }

    /// Whether `id` keys secrets: a profile, a harness's shared variables or
    /// an installed ACP agent's.
    pub fn owns_secrets(&self, id: &str) -> bool {
        self.resolved_profile(id).is_some()
            || ProviderKind::NATIVE
                .into_iter()
                .any(|kind| Self::harness_secret_id(kind) == id)
            || id
                .strip_prefix("acp:")
                .is_some_and(|agent| self.acp_agents.contains_key(agent))
    }

    /// Every selectable profile that drives `kind`: the built-in first, then any
    /// user profiles of that kind in id order. This is what the provider/model
    /// picker iterates.
    pub fn profiles_for_kind(&self, kind: ProviderKind) -> Vec<ResolvedProfile> {
        let builtin = self.provider(kind);
        let mut out = vec![ResolvedProfile {
            id: Self::builtin_profile_id(kind).to_string(),
            kind,
            shared_env: builtin.shared_env.clone(),
            settings: builtin,
        }];
        for (id, profile) in &self.profiles {
            if profile.kind == kind {
                out.push(self.resolve_user_profile(id, profile));
            }
        }
        out
    }

    /// A profile as the UI names it: the provider's product name, followed for
    /// a named or user-created profile by its display name (else its id) in
    /// parentheses, so an account never reads as a provider of its own.
    pub fn profile_display_name(&self, id: &str) -> String {
        self.profile_display_name_as(id, None)
    }

    /// [`Self::profile_display_name`], given the email the profile is signed
    /// in as. Once its harness has a second account or endpoint profile, an
    /// unnamed built-in account is named for that email's local part, so
    /// two accounts never both read as the bare product name.
    pub fn profile_display_name_as(&self, id: &str, signed_in_as: Option<&str>) -> String {
        let Some(profile) = self.resolved_profile(id) else {
            return id.to_string();
        };
        let product = provider_label(profile.kind);
        let builtin = Self::is_builtin_profile_id(id);
        let account = signed_in_as
            .filter(|_| {
                builtin
                    && self
                        .profiles
                        .values()
                        .any(|other| other.kind == profile.kind)
            })
            .and_then(|email| email.split('@').next())
            .map(str::trim)
            .filter(|local| !local.is_empty());
        let qualifier = profile
            .settings
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or(account)
            .or_else(|| (!builtin).then_some(id));
        match qualifier {
            Some(name) => format!("{product} ({name})"),
            None => product.to_string(),
        }
    }

    /// The ids a new profile named `name` may take, best first: the name as a
    /// slug, then `<slug>-2`, `<slug>-3`, …, skipping built-in ids and the ids
    /// of these settings' profiles.
    pub fn profile_id_candidates(&self, name: &str) -> impl Iterator<Item = String> + use<> {
        let base: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "profile" } else { base }.to_string();
        let taken: BTreeSet<String> = self.profiles.keys().cloned().collect();
        std::iter::once(base.clone())
            .chain((2..).map(move |n| format!("{base}-{n}")))
            .filter(move |id| !Self::is_builtin_profile_id(id) && !taken.contains(id))
    }

    /// The `0xRRGGBB` color for a provider color key (see
    /// [`SessionMeta::provider_color_key`](crate::project::SessionMeta::provider_color_key)).
    /// A card's own accent wins when set; otherwise built-in profiles use their
    /// brand color and user profiles and ACP agents take a palette slot that is
    /// distinct from every other configured custom provider while the palette
    /// has room. ACP keys are not profile ids, so they never carry an accent.
    pub fn provider_color(&self, key: &str) -> u32 {
        if let Some(accent) = self
            .resolved_profile(key)
            .and_then(|profile| profile.settings.accent_rgb())
        {
            return accent;
        }
        if let Some(color) = Self::builtin_kind_from_id(key).and_then(builtin_provider_color) {
            return color;
        }
        let acp_keys: Vec<String> = self.acp_agents.keys().map(|id| acp_color_key(id)).collect();
        let mut known: Vec<&str> = self
            .profiles
            .keys()
            .map(String::as_str)
            .chain(acp_keys.iter().map(String::as_str))
            .collect();
        known.sort_unstable();
        crate::provider_colors::palette_color(key, &known)
    }

    /// One installed ACP agent, by registry id.
    pub fn acp_agent(&self, id: &str) -> Option<&InstalledAcpAgent> {
        self.acp_agents.get(id)
    }

    /// Every installed ACP agent, in registry-id order (the marketplace and the
    /// provider rail both render them in this order).
    pub fn installed_acp_agents(&self) -> Vec<&InstalledAcpAgent> {
        self.acp_agents.values().collect()
    }

    /// Fold the pre-`providers` binary overrides into the map (once, on load)
    /// and drop the port of the retired HTTP listener, which no build reads.
    pub fn migrate_legacy(&mut self) {
        self.unknown.remove("remote_port");
        for (host, legacy) in self.github.take().unwrap_or_default().hosts {
            self.source_control
                .hosts
                .entry(host.trim().to_ascii_lowercase())
                .or_insert(HostSettings {
                    kind: HostKind::Github,
                    enabled: legacy.enabled,
                    account: legacy.account,
                });
        }
        for (provider, legacy) in [
            (ProviderKind::Codex, self.codex_binary.take()),
            (ProviderKind::ClaudeCode, self.claude_binary.take()),
        ] {
            if let Some(path) = legacy {
                let entry = self.provider_mut(provider);
                if entry.binary_path.is_none() {
                    entry.binary_path = Some(path);
                }
            }
        }
        // A binary and launch arguments belong to the harness. Earlier builds
        // copied the built-in's into each new account; a copy is dropped, and
        // only a value of the profile's own stays, as its override.
        let harnesses = self.providers.clone();
        for profile in self.profiles.values_mut() {
            let harness = harnesses
                .get(provider_key(profile.kind))
                .cloned()
                .unwrap_or_default();
            let own = &mut profile.settings;
            if own.binary_path == harness.binary_path {
                own.binary_path = None;
            }
            if own.launch_args == harness.launch_args {
                own.launch_args = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Files written while an app could be brought to the front for typing
    /// still load, keeping the rest of what they say.
    #[test]
    fn computer_use_settings_from_the_foreground_typing_era_still_load() {
        for file in [
            r#"{"enabled":true,"allow_input":false,"allow_foreground_fallback":true}"#,
            r#"{"enabled":true,"allow_input":false,"foreground_default":"deny","foreground_apps":{"com.apple.Notes":{"name":"Notes","allow":true}}}"#,
        ] {
            let settings: ComputerUseSettings = serde_json::from_str(file).unwrap();
            assert_eq!(
                settings,
                ComputerUseSettings {
                    enabled: true,
                    allow_input: false,
                    ..ComputerUseSettings::default()
                }
            );
        }
    }

    #[test]
    fn child_approval_settings_preserve_old_orchestrator_and_default_to_auto() {
        for (json, expected) in [
            (r#"{}"#, ChildApprovalMode::Auto),
            (
                r#"{"child_approval":"orchestrator"}"#,
                ChildApprovalMode::Orchestrator,
            ),
            (r#"{"child_approval":"auto"}"#, ChildApprovalMode::Auto),
            (
                r#"{"child_approval":"always_allow"}"#,
                ChildApprovalMode::AlwaysAllow,
            ),
            (r#"{"child_approval":"manual"}"#, ChildApprovalMode::Manual),
            (
                r#"{"child_approval":"future_value"}"#,
                ChildApprovalMode::Auto,
            ),
        ] {
            let settings: SubagentSettings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.child_approval, expected, "{json}");
        }
        assert_eq!(
            SubagentSettings::default().child_approval,
            ChildApprovalMode::Auto
        );
        assert_eq!(
            serde_json::to_value(ChildApprovalMode::Auto).unwrap(),
            "auto"
        );
    }

    #[test]
    fn older_settings_preserve_access_policy_and_accept_partial_feature_blocks() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(legacy.auto_settle_after_days, Some(3.0));
        assert!(!legacy.auto_settle_on_merge);
        assert!(!legacy.sidebar_provider_marks);
        assert!(legacy.scroll_to_latest_on_send);
        assert!(!legacy.sidebar_collapsed);
        assert!(!legacy.remote_hosting_enabled);
        assert_eq!(legacy.remote_host_name, None);
        assert!(legacy.profiles.is_empty());
        assert_eq!(legacy.title_generation.profile_id, None);
        assert!(!legacy.computer_use.enabled);
        assert!(legacy.browser.enabled);
        assert!(legacy.browser.allow_evaluate);
        assert_eq!(legacy.browser.home_url, None);

        let partial: Settings = serde_json::from_str(
            r#"{
            "computer_use":{"enabled":true},
            "browser":{"enabled":false},
            "orchestrate":{},
            "plugins":{"providers":{"claude":true,"kiro":true}},
            "title_generation":{"provider":"codex","model":"m"}
        }"#,
        )
        .unwrap();
        assert!(partial.computer_use.enabled);
        assert_eq!(partial.computer_use.image_mode, ImageMode::Auto);
        assert!(partial.computer_use.allow_input);
        assert!(partial.computer_use.show_agent_cursor);
        assert!(!partial.browser.enabled);
        assert!(partial.browser.allow_evaluate);
        assert!(!partial.subagents.child_worktrees);
        assert_eq!(partial.title_generation.profile_id, None);
    }

    #[test]
    fn an_orchestrate_block_with_its_model_lists_keeps_only_the_subagent_settings() {
        let older: Settings = serde_json::from_str(
            r#"{"orchestrate":{
                "decision_models":[{"provider":"claude","model":"claude-fable-5-1","bundled":"fable"}],
                "child_models":[{"provider":"codex","model":"gpt-6.1-sol","fast":true,"description":"notes"}],
                "child_approval":"manual",
                "child_worktrees":true
            }}"#,
        )
        .unwrap();
        assert_eq!(
            older.subagents,
            SubagentSettings {
                child_approval: ChildApprovalMode::Manual,
                child_worktrees: true,
            }
        );
        assert!(older.unknown.is_empty());
        assert_eq!(
            serde_json::to_value(&older).unwrap()["orchestrate"],
            serde_json::json!({"child_approval": "manual", "child_worktrees": true})
        );
    }

    #[test]
    fn provider_color_maps_builtins_to_brand_and_custom_providers_to_distinct_palette_slots() {
        let settings = Settings::default();
        assert_eq!(settings.provider_color("claude"), 0xD97757);
        assert_eq!(settings.provider_color("codex"), 0x8B5CF6);
        assert_eq!(settings.provider_color("pi"), 0x4D9ABF);
        assert_eq!(settings.provider_color("opencode"), 0x22A06B);

        for (profiles, agents, expected) in [
            (
                vec!["work-claude".to_string()],
                vec!["gemini"],
                Some(vec![0x6B7FD7, 0x14B8A6]),
            ),
            (
                ["work-claude", "proxy-codex", "lab"]
                    .map(String::from)
                    .to_vec(),
                vec!["gemini", "goose"],
                None,
            ),
            (
                (0..13).map(|i| format!("profile-{i:02}")).collect(),
                vec![],
                None,
            ),
        ] {
            let mut settings = Settings::default();
            for id in &profiles {
                settings.profiles.insert(
                    id.clone(),
                    ProviderProfile {
                        kind: ProviderKind::ClaudeCode,
                        settings: ProviderSettings::default(),
                    },
                );
            }
            for id in &agents {
                settings.acp_agents.insert(
                    (*id).into(),
                    InstalledAcpAgent {
                        id: (*id).into(),
                        name: (*id).into(),
                        version: String::new(),
                        icon: None,
                        launch: agent::AcpLaunch::Npx {
                            package: (*id).into(),
                            args: Vec::new(),
                            env: Vec::new(),
                        },
                        enabled: true,
                        env: Vec::new(),
                        launch_args: None,
                    },
                );
            }
            let keys: Vec<_> = profiles
                .into_iter()
                .chain(agents.iter().map(|id| format!("acp:{id}")))
                .collect();
            let colors: Vec<u32> = keys
                .iter()
                .map(|key| settings.provider_color(key))
                .collect();
            if let Some(expected) = expected {
                assert_eq!(colors, expected);
            }
            let mut first_palette: Vec<_> = colors
                .iter()
                .take(PROVIDER_COLOR_PALETTE.len())
                .copied()
                .collect();
            first_palette.sort_unstable();
            first_palette.dedup();
            assert_eq!(
                first_palette.len(),
                keys.len().min(PROVIDER_COLOR_PALETTE.len()),
                "{colors:06X?}"
            );
            for color in &colors {
                assert!(PROVIDER_COLOR_PALETTE.contains(color));
                assert!(![0xD97757, 0x8B5CF6, 0x4D9ABF, 0x22A06B].contains(color));
            }
            assert_eq!(
                colors,
                keys.iter()
                    .map(|key| settings.provider_color(key))
                    .collect::<Vec<_>>()
            );
            assert_eq!(settings.provider_color("deleted-profile"), 0xF59E0B);
        }
    }

    #[test]
    fn card_accent_wins_over_brand_and_palette_when_valid() {
        let mut settings = Settings::default();
        settings.profiles.insert(
            "work-claude".into(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    accent_color: Some("#2563eb".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        settings.profiles.insert(
            "broken".into(),
            ProviderProfile {
                kind: ProviderKind::Codex,
                settings: ProviderSettings {
                    accent_color: Some("blue".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        settings.provider_mut(ProviderKind::Codex).accent_color = Some("00FF00".into());
        assert_eq!(settings.provider_color("work-claude"), 0x2563EB);
        assert_eq!(settings.provider_color("codex"), 0x00FF00);
        assert!(PROVIDER_COLOR_PALETTE.contains(&settings.provider_color("broken")));
        assert_eq!(settings.provider_color("claude"), 0xD97757);
    }

    #[test]
    fn old_pi_provider_settings_field_names_remain_serde_compatible() {
        let legacy: ProviderSettings = serde_json::from_str("{}").unwrap();
        assert!(!legacy.pi.trust_project_extensions);

        let old_json = r#"{
            "pi_trust_project_extensions": true
        }"#;
        let settings: ProviderSettings = serde_json::from_str(old_json).unwrap();
        assert!(settings.pi.trust_project_extensions);
        let serialized = serde_json::to_value(&settings).unwrap();
        assert_eq!(serialized["pi_trust_project_extensions"], true);
        assert!(serialized.get("pi").is_none());
    }

    #[test]
    fn favorites_read_older_bare_ids_beside_rows() {
        let mut settings: Settings = serde_json::from_str(
            r#"{"favorite_models":["opus",
                {"provider":"codex","model":"gpt-6.1-sol","profile_id":"second"},
                {"provider":"codex","model":"gpt-6-astra"}]}"#,
        )
        .unwrap();
        assert_eq!(
            settings.favorite_ids(ProviderKind::ClaudeCode, "claude"),
            ["opus"]
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::ClaudeCode, "second-claude"),
            ["opus"]
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::Codex, "codex"),
            ["opus", "gpt-6-astra"]
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::Codex, "second"),
            ["opus", "gpt-6.1-sol"]
        );
        assert_eq!(
            serde_json::to_value(&settings).unwrap()["favorite_models"],
            serde_json::json!([
                "opus",
                {"provider":"codex","model":"gpt-6.1-sol","profile_id":"second"},
                {"provider":"codex","model":"gpt-6-astra"}
            ])
        );

        settings.toggle_favorite_model(ProviderKind::ClaudeCode, None, "opus".into());
        assert_eq!(
            settings.favorite_ids(ProviderKind::ClaudeCode, "second-claude"),
            Vec::<String>::new(),
            "a bare id has one star to take off"
        );
    }

    #[test]
    fn a_favorite_stars_one_account() {
        let mut settings = Settings::default();
        settings.toggle_favorite_model(ProviderKind::Codex, Some("second".into()), "model".into());
        settings.toggle_favorite_model(
            ProviderKind::Codex,
            Some("codex".into()),
            "built-in".into(),
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::Codex, "second"),
            ["model"]
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::Codex, "codex"),
            ["built-in"]
        );
        assert_eq!(
            settings.favorite_ids(ProviderKind::ClaudeCode, "claude"),
            Vec::<String>::new()
        );

        settings.toggle_favorite_model(ProviderKind::Codex, Some("second".into()), "model".into());
        settings.toggle_favorite_model(ProviderKind::Codex, None, "built-in".into());
        assert!(settings.favorite_models.is_empty());
    }

    #[test]
    fn resolves_builtin_and_user_profiles() {
        let mut settings = Settings::default();
        // Give the built-in Claude card a base URL and a display name.
        settings.provider_mut(ProviderKind::ClaudeCode).display_name = Some("Claude".into());

        // A user profile driving the same protocol (third-party Claude).
        let id = settings
            .profile_id_candidates("Klaude Kode")
            .next()
            .unwrap();
        assert_eq!(id, "klaude-kode");
        settings.profiles.insert(
            id.clone(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    display_name: Some("Klaude Kode".into()),
                    env: vec![EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.com/coding/".into(),
                        sensitive: false,
                    }],
                    ..ProviderSettings::default()
                },
            },
        );

        // Built-in id resolves to the provider card.
        let builtin = settings.resolved_profile("claude").unwrap();
        assert_eq!(builtin.kind, ProviderKind::ClaudeCode);
        assert!(Settings::is_builtin_profile_id("claude"));

        // User id resolves to its profile, tagged with the shared protocol.
        let custom = settings.resolved_profile(&id).unwrap();
        assert_eq!(custom.kind, ProviderKind::ClaudeCode);
        assert_eq!(custom.settings.env[0].value, "https://api.kimi.com/coding/");
        assert!(!Settings::is_builtin_profile_id(&id));

        // Both Claude profiles are offered for the kind, built-in first.
        let claude_profiles = settings.profiles_for_kind(ProviderKind::ClaudeCode);
        assert_eq!(claude_profiles.len(), 2);
        assert_eq!(claude_profiles[0].id, "claude");
        assert_eq!(claude_profiles[1].id, id);
        // Every other native provider still resolves to exactly its built-in.
        assert_eq!(settings.profiles_for_kind(ProviderKind::Codex).len(), 1);
        assert_eq!(settings.profiles_for_kind(ProviderKind::Pi).len(), 1);
        assert_eq!(settings.profiles_for_kind(ProviderKind::OpenCode).len(), 1);
        assert_eq!(
            settings.resolved_profile("pi").unwrap().kind,
            ProviderKind::Pi
        );
        assert_eq!(
            settings.resolved_profile("opencode").unwrap().kind,
            ProviderKind::OpenCode
        );

        // Display names: the product name, then the display name in parentheses.
        assert_eq!(
            settings.profile_display_name("claude"),
            "Claude Code (Claude)"
        );
        assert_eq!(
            settings.profile_display_name(&id),
            "Claude Code (Klaude Kode)"
        );
        assert_eq!(settings.resolved_profile("nope"), None);

        // A second profile of the same name gets a distinct id.
        assert_eq!(
            settings
                .profile_id_candidates("Klaude Kode")
                .next()
                .unwrap(),
            "klaude-kode-2"
        );
    }

    #[test]
    fn a_user_profile_named_after_a_new_native_provider_keeps_its_identity() {
        // Saved before Cursor was native: a Claude profile the user named "Cursor".
        let settings: Settings = serde_json::from_str(
            r##"{"profiles":{"cursor":{"kind":"claude_code","display_name":"Cursor","accent_color":"#123456"}}}"##,
        )
        .unwrap();
        assert_eq!(
            settings.resolved_profile("cursor").unwrap().kind,
            ProviderKind::ClaudeCode
        );
        let [builtin] = settings
            .profiles_for_kind(ProviderKind::Cursor)
            .try_into()
            .unwrap();
        assert_eq!(builtin.kind, ProviderKind::Cursor);
        assert_eq!(
            settings.resolved_profile(&builtin.id).unwrap().kind,
            ProviderKind::Cursor
        );
        assert!(!Settings::is_builtin_profile_id(
            &settings.profile_id_candidates(&builtin.id).next().unwrap()
        ));
        let thread = crate::project::SessionMeta::new(ProviderKind::Cursor, "/x".into(), None);
        assert_ne!(
            settings.provider_color(&thread.provider_color_key()),
            0x123456
        );
    }

    /// A build that predates a field must not destroy it: unknown keys survive a
    /// load → save round trip. (We hit this for real: an older binary dropped
    /// `acp_agents` and the next save wiped the installed agents.)
    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let json = r#"{
            "theme_mode": "dark",
            "a_future_field": {"nested": [1, 2, 3]},
            "diff_view_mode": "line",
            "another": "value"
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.theme_mode, ThemeMode::Dark);

        let written = serde_json::to_string(&settings).unwrap();
        let back: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            back.get("a_future_field"),
            Some(&serde_json::json!({"nested": [1, 2, 3]})),
            "an unknown field was dropped on save"
        );
        assert_eq!(back.get("another"), Some(&serde_json::json!("value")));
        assert_eq!(back.get("diff_view_mode"), Some(&serde_json::json!("line")));
    }

    /// A settings file written by the HTTP-listener builds names a port and no
    /// Traverse mode. It must load as hosting on the official service with the
    /// name kept, and the retired port must not be written back.
    #[test]
    fn hosting_settings_from_the_http_listener_builds_migrate_to_official_traverse() {
        let mut legacy: Settings = serde_json::from_str(
            r#"{"remote_hosting_enabled":true,"remote_port":47421,"remote_host_name":"Studio"}"#,
        )
        .unwrap();
        legacy.migrate_legacy();
        assert!(legacy.remote_hosting_enabled);
        assert_eq!(
            serde_json::to_value(&legacy.traverse).unwrap(),
            serde_json::json!({"sources": [{"kind": "official", "enabled": true}]})
        );
        assert_eq!(legacy.remote_host_name.as_deref(), Some("Studio"));
        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(written.get("remote_port"), None);
        assert_eq!(written.get("traverse"), None, "the default is not written");
    }

    /// Every Traverse setting a settings file has held loads as the source
    /// list it meant: the single choice of earlier builds keeps its
    /// either-or meaning, and a hand-edited list gets the official source
    /// first exactly once. What is written back is the list.
    #[test]
    fn traverse_settings_load_as_a_source_list_with_the_official_source_first() {
        let official = |enabled| serde_json::json!({"kind": "official", "enabled": enabled});
        let custom = |url: &str, enabled| serde_json::json!({"kind": "custom", "url": url, "enabled": enabled});
        for (file, expected) in [
            (r#"{}"#, vec![official(true)]),
            (r#"{"traverse":{"mode":"official"}}"#, vec![official(true)]),
            (r#"{"traverse":{"mode":"off"}}"#, vec![official(false)]),
            (
                r#"{"traverse":{"mode":"custom","url":"https://traverse.example/"}}"#,
                vec![official(false), custom("https://traverse.example/", true)],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"official","enabled":true},{"kind":"custom","url":"https://a.example/","enabled":true},{"kind":"custom","url":"https://b.example/","enabled":false}]}}"#,
                vec![
                    official(true),
                    custom("https://a.example/", true),
                    custom("https://b.example/", false),
                ],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"custom","url":"https://a.example/","enabled":true},{"kind":"official","enabled":true},{"kind":"official","enabled":false},{"kind":"custom","url":"https://a.example/","enabled":false}]}}"#,
                vec![official(true), custom("https://a.example/", true)],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"custom","url":"https://a.example/","enabled":true}]}}"#,
                vec![official(false), custom("https://a.example/", true)],
            ),
        ] {
            let settings: Settings = serde_json::from_str(file).unwrap();
            assert_eq!(
                serde_json::to_value(&settings.traverse).unwrap(),
                serde_json::json!({ "sources": expected }),
                "{file}"
            );
        }

        let mut patched = Settings::default();
        let off: TraverseSetting = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        patched.apply(SettingsPatch::Traverse(off)).unwrap();
        assert_eq!(
            serde_json::to_value(&patched).unwrap().get("traverse"),
            Some(&serde_json::json!({"sources": [official(false)]}))
        );
        assert_eq!(patched.traverse.enabled().count(), 0);
    }
}

#[cfg(test)]
mod account_usage_tests {
    use super::*;

    #[test]
    fn resolved_custom_endpoint_is_not_a_native_account() {
        let settings: Settings = serde_json::from_str(r#"{"profiles":{"custom":{"kind":"claude_code","display_name":"Kimi","env":[{"name":"ANTHROPIC_BASE_URL","value":"https://api.example.com/anthropic"},{"name":"ANTHROPIC_API_KEY","sensitive":true}]}}}"#).unwrap();
        let mut custom = settings.resolved_profile("custom").unwrap();
        assert!(
            !custom.supports_account_usage(),
            "custom protocol compatibility does not imply native subscription support"
        );
        custom.settings.display_name = Some("Claude".into());
        assert!(!custom.supports_account_usage());
        custom.settings.env.clear();
        assert!(
            custom.supports_account_usage(),
            "a custom profile using native account auth remains eligible, even signed out"
        );
    }
}

#[cfg(test)]
mod harness_settings_tests {
    use super::*;
    use std::path::Path;

    /// Earlier builds copied the built-in binary and launch arguments into
    /// each new account. A settings file from then loads with the copies
    /// dropped, so the account follows its harness, while a binary or
    /// arguments of the account's own stay its override.
    #[test]
    fn account_copies_of_the_harness_binary_are_dropped_and_their_own_kept() {
        let mut settings: Settings = serde_json::from_str(
            r#"{
                "providers": {"claude": {"binary_path": "/a/claude", "launch_args": "--chrome"}},
                "profiles": {
                    "work": {"kind": "claude_code", "display_name": "Work",
                             "binary_path": "/a/claude", "launch_args": "--debug"},
                    "kimi": {"kind": "claude_code", "binary_path": "/b/claude",
                             "launch_args": "--chrome"}
                }
            }"#,
        )
        .unwrap();
        settings.migrate_legacy();
        let own = |id: &str| settings.profiles[id].settings.clone();
        assert_eq!(own("work").binary_path, None);
        assert_eq!(own("work").launch_args.as_deref(), Some("--debug"));
        assert_eq!(
            own("kimi").binary_path.as_deref(),
            Some(Path::new("/b/claude"))
        );
        assert_eq!(own("kimi").launch_args, None);

        // The harness's binary moves, and the account without its own moves with it.
        settings.provider_mut(ProviderKind::ClaudeCode).binary_path = Some("/c/claude".into());
        let work = settings.resolved_profile("work").unwrap().settings;
        assert_eq!(work.binary_path.as_deref(), Some(Path::new("/c/claude")));
        assert_eq!(work.launch_args.as_deref(), Some("--debug"));
        let kimi = settings.resolved_profile("kimi").unwrap().settings;
        assert_eq!(kimi.binary_path.as_deref(), Some(Path::new("/b/claude")));
        assert_eq!(kimi.launch_args.as_deref(), Some("--chrome"));
    }

    /// An unnamed default account reads as its harness alone until the
    /// harness has another account or endpoint profile; then it is named for
    /// the local part of the email it is signed in as, unless the user named
    /// it.
    #[test]
    fn the_default_account_is_named_for_its_email_beside_another() {
        let mut settings = Settings::default();
        let email = Some("tryanks@example.com");
        assert_eq!(
            settings.profile_display_name_as("claude", email),
            "Claude Code"
        );
        settings.profiles.insert(
            "kimi".into(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings::default(),
            },
        );
        assert_eq!(
            settings.profile_display_name_as("claude", email),
            "Claude Code (tryanks)"
        );
        assert_eq!(
            settings.profile_display_name_as("claude", None),
            "Claude Code"
        );
        assert_eq!(settings.profile_display_name_as("codex", email), "Codex");
        settings.provider_mut(ProviderKind::ClaudeCode).display_name = Some("Mine".into());
        assert_eq!(
            settings.profile_display_name_as("claude", email),
            "Claude Code (Mine)"
        );
    }
}
