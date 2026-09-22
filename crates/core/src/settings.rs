//! Persisted application settings domain data.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use agent::{ModelSpec, OptionDescriptor, ProviderKind};
use serde::{Deserialize, Serialize};

use crate::acp::InstalledAcpAgent;
pub use crate::provider_colors::{PROVIDER_COLOR_PALETTE, builtin_provider_color};

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

/// The stable settings-file key for a provider. It keys
/// both `settings.json`'s `providers` map and `secrets.json`.
pub fn provider_key(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
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

/// The provider's short display name used for card titles and picker labels.
pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "Codex",
        ProviderKind::ClaudeCode => "Claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "OpenCode",
        ProviderKind::Acp => "ACP",
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

/// Per-provider configuration (Settings → Providers card).
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
    /// Override for the CLI binary (`None` = resolve from PATH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<PathBuf>,
    /// Claude: `HOME`; Codex: `CODEX_HOME`; pi: `PI_CODING_AGENT_DIR`.
    /// OpenCode ignores this field because it has no single-home override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_path: Option<PathBuf>,
    /// Native-provider CLI arguments appended on session start (ignored for Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub pi: PiProviderSettings,
    /// Model slugs added by hand in the Models section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    /// Model ids hidden from the composer's model picker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_models: Vec<String>,
}

/// Serializable edits the UI can make to one provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSettingsPatch {
    SetEnabled { enabled: bool },
    ReplaceConfiguration(Box<ProfileConfigurationPatch>),
}

/// The provider-dialog configuration payload of
/// [`ProfileSettingsPatch::ReplaceConfiguration`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfigurationPatch {
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub env: Vec<EnvVar>,
    pub binary_path: Option<PathBuf>,
    pub home_path: Option<PathBuf>,
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub pi: PiProviderSettings,
    pub custom_models: Vec<String>,
    pub hidden_models: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiProviderSettings {
    /// Whether pi should trust and load the project's local `.pi` configuration.
    #[serde(default, rename = "pi_trust_project_extensions")]
    pub trust_project_extensions: bool,
    /// Whether tcode should inject its native approval extension into pi.
    #[serde(default, rename = "pi_native_approvals")]
    pub native_approvals: bool,
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
            binary_path: None,
            home_path: None,
            launch_args: None,
            pi: PiProviderSettings::default(),
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

    /// A native provider's `Launch arguments` field, split on whitespace.
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// A user-created provider profile (Settings → Providers "+ New profile").
///
/// A profile pairs a *protocol* ([`ProviderKind`] — which native CLI/adapter
/// spawns it) with a full [`ProviderSettings`] card. Several profiles may share
/// one protocol, which is how a session can talk to the official Anthropic API
/// *and* a third-party Anthropic-compatible endpoint at the same time: both are
/// `ProviderKind::ClaudeCode`, each with its own `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL` env and its own isolated home.
///
/// The built-in profiles are not stored here — they
/// remain in [`Settings::providers`] under their [`provider_key`]. Only extra,
/// user-created profiles live in [`Settings::profiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// The protocol this profile drives. Determines which native adapter
    /// (`claude` / `codex` / `pi` / `opencode`) spawns it and how its native
    /// protocol is normalized.
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
    /// The effective card settings.
    pub settings: ProviderSettings,
}

impl ResolvedProfile {
    /// Whether this endpoint/auth configuration can query native account limits.
    pub fn supports_account_usage(&self) -> bool {
        let (endpoint_key, native_endpoint, credentials, backends): (&str, &str, &[&str], &[&str]) =
            match self.kind {
                ProviderKind::ClaudeCode => (
                    "ANTHROPIC_BASE_URL",
                    "https://api.anthropic.com",
                    &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
                    &[
                        "CLAUDE_CODE_USE_BEDROCK",
                        "CLAUDE_CODE_USE_VERTEX",
                        "CLAUDE_CODE_USE_FOUNDRY",
                    ],
                ),
                ProviderKind::Codex => (
                    "OPENAI_BASE_URL",
                    "https://api.openai.com/v1",
                    &["OPENAI_API_KEY", "CODEX_API_KEY"],
                    &[],
                ),
                _ => return false,
            };
        // Secret presence is enough to classify API-key auth; never inspect or
        // replicate secret values. Missing native sign-in remains a probe error.
        !self.settings.env.iter().enumerate().any(|(index, env)| {
            // LaunchEnv uses the last occurrence of a repeated environment key.
            if self.settings.env[index + 1..]
                .iter()
                .any(|later| later.name == env.name)
            {
                return false;
            }
            let configured = env.sensitive || !env.value.trim().is_empty();
            configured
                && (credentials.contains(&env.name.as_str())
                    || (backends.contains(&env.name.as_str())
                        && env.value != "0"
                        && env.value != "false")
                    || (env.name == endpoint_key
                        && (env.sensitive
                            || env.value.trim().trim_end_matches('/').to_ascii_lowercase()
                                != native_endpoint)))
        })
    }
}

/// One configured model, unique by provider and model ID within its role.
/// Reasoning effort is selected per tool call from the provider's capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrateChildModel {
    pub provider: ProviderKind,
    pub model: String,
    /// Which provider profile (endpoint config) the dispatch launches against;
    /// `None` = the kind's built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Controls availability as a collaboration peer or executor in this list.
    /// Disabled entries retain their configuration and are omitted from the fleet.
    /// This never controls whether the model can run as the main decision model.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Dispatch with the provider's fast mode (Claude `fastMode`, Codex `fast`
    /// service tier). Ignored by providers without one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

const LEGACY_GPT_MEDIUM_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 9, intelligence 8, taste 6. An economical execution profile for bulk or mechanical implementation against a written brief, closed-form debugging with a repro, migrations, data analysis, reviews, sweeps, computer use and eyes-on-screen verification, and token-heavy log or codebase crawls. Extremely steerable and disciplined: respects scope fences, does not weaken tests, reports accurately. Measured equal to its higher efforts on spec-driven work — escalate only after this profile demonstrably misses on a specific piece and the gap looks like depth, not a bad brief.";
const LEGACY_GPT_MAX_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 6, intelligence 9, taste 7. Exception tier — near the top judgment model's raw problem-solving at a fraction of the token cost, with a rottweiler temperament: grabs the problem by the throat and doesn't let go. Route it hard, well-defined problems that reward tenacity or depth: gnarly bugs with a repro, long autonomous grinds, brute-force search of a solution space, open-ended polish passes. Two measured caveats: wall-clock latency is 5–6x the medium profile, so keep it off any pipeline's critical path; and on closed-form bug fixes it produces the same fix as medium at 1.5–3x the cost. Taste 7 clears the bar for internal tools and dashboards; keep brand- or copy-critical surfaces on a taste-8+ model.";
const LEGACY_SONNET_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 5, intelligence 5, taste 7. Cheap glue — wrappers, chores, and context gathering that does not require top-tier judgment.";
const LEGACY_OPUS_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 4, intelligence 7, taste 8. First choice for user-facing work: UI, copy, API design, and anything where taste matters more than grinding depth. Also a strong independent reviewer of plans and implementations.";
const LEGACY_ASTRA_DECISION_DEFINITION: &str = "Decision collaboration: develop independent approaches, challenge assumptions, and review architecture and acceptance evidence. Consult alongside Fable for another provider's perspective; route implementation and evidence gathering to execution models.";
const LEGACY_FABLE_DECISION_DEFINITION: &str = "Decision collaboration: examine framing, architecture, user-facing design, and ambiguous tradeoffs. Consult alongside Astra for another provider's perspective; route implementation and evidence gathering to execution models.";

const OLD_DEFAULT_SOL_DEFINITION: &str = "Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely.";
const OLD_DEFAULT_OPUS_DEFINITION: &str = "Execution model for agentic coding, cross-file implementation, refactoring, debugging, and review. Consider it alongside Sol across providers, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely.";
const OLD_DEFAULT_GPT_6_EXECUTION_DEFINITION: &str = "Baseline execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Default to low effort for a clear brief; raise effort only when a specific piece demonstrably needs more depth. Keep unrelated improvements out of scope, match verification to the changed behavior, and report the concrete result and relevant checks concisely.";
const DEFAULT_GPT_6_EXECUTION_DEFINITION: &str = "Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, evidence gathering, and computer use. It is exceptionally strong at driving and reading real UIs (find_roots → observe_ui → search_ui / inspect_ui / read_text, and act_ui / wait_for when the brief allows), so route eyes-on-screen verification and UI-driving work here first. Always dispatch it at low effort: low outperforms the former Sol executor at xhigh on quality and at a fraction of the token cost, so medium or higher is never justified for this profile and only wastes money; a task that seems to need more depth needs a better brief, not more effort. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely.";
const DEFAULT_SOL_6_DEFINITION: &str = "Primary execution model: dispatch ordinary implementation, debugging with a reproduction, migrations, code review, data analysis, evidence gathering, and computer use here first. GPT-6 Sol is built for complex coding and agentic workflows, with about half the factual errors of GPT-5.6 Sol, fewer misleading claims about its own work, and Astra-level reliability at a fraction of the cost; on computer-use tasks it matches Opus-class models at far lower cost per task. Start at medium, the recommended default for everyday and complex work; use low for narrow mechanical edits, and raise to high or xhigh only for work that needs more planning, analysis, or checking across many steps, sources, or tradeoffs; reserve max for the hardest well-defined problems. Keep unrelated improvements out of scope, match verification to the changed behavior, and report the concrete result and relevant checks concisely.";
const PREVIOUS_DEFAULT_OPUS_DEFINITION: &str = "Execution model for agentic coding, cross-file implementation, refactoring, debugging, and review across providers, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely.";
const DEFAULT_OPUS_DEFINITION: &str = "Secondary execution model, not the default: Sol handles ordinary work. Choose Opus 5.5 when a task specifically benefits from a Claude Code perspective, when a second independent implementation or review across providers is wanted, or when Sol is unavailable or has demonstrably stalled on it. Strong at agentic coding, cross-file implementation, refactoring, debugging, and review, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely.";
const DEFAULT_ASTRA_DEFINITION: &str = include_str!("../../../assets/orchestrate/astra.md");
const DEFAULT_FABLE_DEFINITION: &str = include_str!("../../../assets/orchestrate/fable-5-1.md");

/// Live catalogs are authoritative. Bundled fallbacks cover startup before discovery.
/// Collaboration is deliberately capped at medium/high, independent of execution.
pub fn orchestrate_efforts(
    provider: ProviderKind,
    model: &str,
    catalog: &[ModelSpec],
    collaboration: bool,
) -> Vec<String> {
    let mut efforts = if let Some(spec) = catalog.iter().find(|spec| spec.id == model) {
        spec.options
            .iter()
            .find_map(|option| match option {
                OptionDescriptor::Select { id, options, .. } if id == "reasoningEffort" => Some(
                    options
                        .iter()
                        .map(|choice| choice.value.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    } else {
        let fallback: &[&str] = match (provider, model) {
            (ProviderKind::Codex, "gpt-5.6-sol" | "gpt-6-sol" | "gpt-6-astra") => {
                &["low", "medium", "high", "xhigh", "max", "ultra"]
            }
            (
                ProviderKind::ClaudeCode,
                "claude-opus-5-5" | "claude-opus-5" | "claude-fable-5-1",
            ) => &[
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode",
                "ultrathink",
            ],
            _ => &[],
        };
        fallback
            .iter()
            .map(|effort| (*effort).to_string())
            .collect()
    };
    if collaboration {
        efforts.retain(|effort| matches!(effort.as_str(), "medium" | "high"));
    }
    efforts
}

/// Who answers permission requests raised by dispatched child threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChildApprovalMode {
    /// The parent (decision-layer) model answers via the orchestrate `approve` tool.
    #[default]
    Orchestrator,
    /// Auto-approve every child request for the session.
    AlwaysAllow,
    /// The user answers in the child's composer (legacy behavior).
    Manual,
}

/// Settings for tcode's built-in orchestration layer.
///
/// Decision profiles support peer consultation; execution profiles receive tasks.
/// Any session may invoke the workflow. Bundled decision profiles are Astra and Fable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OrchestrateSettingsData")]
pub struct OrchestrateSettings {
    /// Collaboration invite list, not an allow list for the main decision model.
    pub decision_models: Vec<OrchestrateChildModel>,
    pub child_models: Vec<OrchestrateChildModel>,
    #[serde(default)]
    pub child_approval: ChildApprovalMode,
    /// Give dispatched children dedicated Git worktrees when their resolved cwd
    /// is itself a repository root. Dispatch-level `worktree` overrides this.
    #[serde(default)]
    pub child_worktrees: bool,
    /// Archive completed children automatically once their terminal result has
    /// been delivered to the parent. Per-dispatch `archive_on_complete`
    /// overrides this; failed children always stay visible for retries.
    #[serde(default = "default_true")]
    pub archive_on_complete: bool,
}

impl Default for OrchestrateSettings {
    fn default() -> Self {
        Self {
            decision_models: vec![
                builtin_model(ProviderKind::Codex, "gpt-6-astra", DEFAULT_ASTRA_DEFINITION),
                builtin_model(
                    ProviderKind::ClaudeCode,
                    "claude-fable-5-1",
                    DEFAULT_FABLE_DEFINITION,
                ),
            ],
            child_models: vec![
                builtin_model(ProviderKind::Codex, "gpt-6-sol", DEFAULT_SOL_6_DEFINITION),
                builtin_model(
                    ProviderKind::ClaudeCode,
                    "claude-opus-5-5",
                    DEFAULT_OPUS_DEFINITION,
                ),
            ],
            child_approval: ChildApprovalMode::default(),
            child_worktrees: false,
            archive_on_complete: true,
        }
    }
}

fn builtin_model(provider: ProviderKind, model: &str, description: &str) -> OrchestrateChildModel {
    OrchestrateChildModel {
        provider,
        model: model.into(),
        profile_id: None,
        enabled: true,
        fast: false,
        description: description.into(),
    }
}

#[derive(Deserialize)]
struct LegacyOrchestrateModel {
    #[serde(flatten)]
    entry: OrchestrateChildModel,
    #[serde(default, alias = "default_effort")]
    effort: Option<String>,
}

impl LegacyOrchestrateModel {
    fn migrate(
        mut self,
        collaboration: bool,
        replace_untouched_sol: bool,
        replace_untouched_opus: bool,
    ) -> Option<OrchestrateChildModel> {
        // An untouched bundled Codex executor (Sol 5.6, then Astra) follows
        // the bundle to GPT-6 Sol unless the user already added it; rows with
        // custom guidance, an endpoint profile, or changed switches stay.
        let untouched_codex_executor = !collaboration
            && self.effort.is_none()
            && self.entry.provider == ProviderKind::Codex
            && self.entry.profile_id.is_none()
            && self.entry.enabled
            && !self.entry.fast
            && match self.entry.model.as_str() {
                "gpt-5.6-sol" => self.entry.description == OLD_DEFAULT_SOL_DEFINITION,
                "gpt-6-astra" => {
                    self.entry.description == OLD_DEFAULT_GPT_6_EXECUTION_DEFINITION
                        || self.entry.description == DEFAULT_GPT_6_EXECUTION_DEFINITION
                }
                _ => false,
            };
        if untouched_codex_executor {
            return replace_untouched_sol.then(|| {
                builtin_model(ProviderKind::Codex, "gpt-6-sol", DEFAULT_SOL_6_DEFINITION)
            });
        }
        if !collaboration
            && self.entry.provider == ProviderKind::ClaudeCode
            && self.entry.model == "claude-opus-5"
            && self.entry.profile_id.is_none()
            && (self.entry.description == OLD_DEFAULT_OPUS_DEFINITION
                || self.entry.description == PREVIOUS_DEFAULT_OPUS_DEFINITION
                || self.entry.description == DEFAULT_OPUS_DEFINITION)
        {
            // An untouched bundled Opus 5 row follows the release to Opus 5.5
            // unless the user already added Opus 5.5 themselves; rows with
            // custom guidance or an endpoint profile stay where they are.
            if replace_untouched_opus {
                self.entry.model = "claude-opus-5-5".into();
            }
            self.entry.description = DEFAULT_OPUS_DEFINITION.into();
        }
        if !collaboration
            && self.entry.provider == ProviderKind::Codex
            && self.entry.model == "gpt-6-astra"
            && self.entry.description == OLD_DEFAULT_GPT_6_EXECUTION_DEFINITION
        {
            self.entry.description = DEFAULT_GPT_6_EXECUTION_DEFINITION.into();
        }
        if !collaboration
            && self.entry.provider == ProviderKind::ClaudeCode
            && self.entry.model == "claude-opus-5-5"
            && self.entry.description == PREVIOUS_DEFAULT_OPUS_DEFINITION
        {
            self.entry.description = DEFAULT_OPUS_DEFINITION.into();
        }
        let entry = &mut self.entry;
        let legacy = self.effort.is_some();
        if legacy {
            if entry.provider == ProviderKind::ClaudeCode {
                match entry.model.as_str() {
                    "claude-sonnet-5" if entry.description == LEGACY_SONNET_CHILD_DEFINITION => {
                        return None;
                    }
                    "claude-opus-4-8" | "claude-opus-5" => entry.model = "claude-opus-5-5".into(),
                    "claude-fable-5" => entry.model = "claude-fable-5-1".into(),
                    _ => {}
                }
            }
            let bundled = [
                LEGACY_GPT_MEDIUM_CHILD_DEFINITION,
                LEGACY_GPT_MAX_CHILD_DEFINITION,
                LEGACY_OPUS_CHILD_DEFINITION,
                LEGACY_ASTRA_DECISION_DEFINITION,
                LEGACY_FABLE_DECISION_DEFINITION,
                "Ratings (1–10, higher is better): cost efficiency 2, intelligence 9, taste 9. Highest-judgment escalation for framing, architecture, ambiguous tradeoffs, taste-critical surfaces, and final review. The scarcest resource in the fleet: dispatch to it only when nothing cheaper is adequate.",
            ];
            if bundled.contains(&entry.description.as_str())
                || entry.description
                    == LEGACY_GPT_MEDIUM_CHILD_DEFINITION.replace(
                        "An economical execution profile for",
                        "The default profile for everything dispatched:",
                    )
            {
                if let Some(description) = match (entry.provider, entry.model.as_str()) {
                    (ProviderKind::Codex, "gpt-5.6-sol") => Some(OLD_DEFAULT_SOL_DEFINITION),
                    (ProviderKind::Codex, "gpt-6-astra") => Some(DEFAULT_ASTRA_DEFINITION),
                    (ProviderKind::ClaudeCode, "claude-opus-5-5") => Some(DEFAULT_OPUS_DEFINITION),
                    (ProviderKind::ClaudeCode, "claude-fable-5-1") => {
                        Some(DEFAULT_FABLE_DEFINITION)
                    }
                    _ => None,
                } {
                    entry.description = description.into();
                }
            } else if !entry.description.trim().is_empty() {
                // Old custom tier guidance remains meaningful after merging rows.
                entry.description = format!(
                    "Guidance previously used at {} effort: {}",
                    self.effort.as_deref().unwrap(),
                    entry.description
                );
            }
        }
        Some(self.entry)
    }
}

/// Ignore retired lead identities and consume old fixed efforts only for migration.
#[derive(Deserialize)]
#[serde(default)]
struct OrchestrateSettingsData {
    decision_models: Option<Vec<LegacyOrchestrateModel>>,
    child_models: Vec<LegacyOrchestrateModel>,
    child_approval: ChildApprovalMode,
    child_worktrees: bool,
    archive_on_complete: bool,
}

impl Default for OrchestrateSettingsData {
    fn default() -> Self {
        Self {
            decision_models: None,
            child_models: Vec::new(),
            child_approval: ChildApprovalMode::default(),
            child_worktrees: false,
            archive_on_complete: true,
        }
    }
}

impl From<OrchestrateSettingsData> for OrchestrateSettings {
    fn from(data: OrchestrateSettingsData) -> Self {
        let (decisions, children) = if let Some(decisions) = data.decision_models {
            let has_execution = |provider: ProviderKind, model: &str| {
                data.child_models
                    .iter()
                    .any(|entry| entry.entry.provider == provider && entry.entry.model == model)
            };
            let has_execution_sol_6 = has_execution(ProviderKind::Codex, "gpt-6-sol");
            let has_execution_opus_5_5 = has_execution(ProviderKind::ClaudeCode, "claude-opus-5-5");
            (
                decisions
                    .into_iter()
                    .filter_map(|entry| entry.migrate(true, false, false))
                    .collect(),
                data.child_models
                    .into_iter()
                    .filter_map(|entry| {
                        entry.migrate(false, !has_execution_sol_6, !has_execution_opus_5_5)
                    })
                    .collect(),
            )
        } else {
            let mut decisions = Self::default().decision_models;
            let mut legacy_decisions = Vec::new();
            let mut legacy_children = Vec::new();
            for entry in data.child_models {
                let legacy_decision = matches!(
                    (entry.entry.provider, entry.entry.model.as_str()),
                    (ProviderKind::Codex, "gpt-6-astra")
                        | (
                            ProviderKind::ClaudeCode,
                            "claude-fable-5" | "claude-fable-5-1"
                        )
                );
                if legacy_decision {
                    legacy_decisions.push(entry);
                } else {
                    legacy_children.push(entry);
                }
            }
            let mut migrated: Vec<_> = legacy_decisions
                .into_iter()
                .filter_map(|entry| entry.migrate(true, false, false))
                .collect();
            for builtin in &mut decisions {
                if let Some(index) = migrated.iter().position(|entry| {
                    builtin.provider == entry.provider && builtin.model == entry.model
                }) {
                    *builtin = migrated.remove(index);
                }
            }
            decisions.extend(migrated);
            let children = legacy_children
                .into_iter()
                .filter_map(|entry| entry.migrate(false, true, true))
                .collect();
            (decisions, children)
        };
        let mut settings = Self {
            decision_models: decisions,
            child_models: children,
            child_approval: data.child_approval,
            child_worktrees: data.child_worktrees,
            archive_on_complete: data.archive_on_complete,
        };
        settings.deduplicate_models(true);
        settings
    }
}

impl OrchestrateSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn builtin_decision_definition(
        provider: ProviderKind,
        model: &str,
    ) -> Option<&'static str> {
        match (provider, model) {
            (ProviderKind::ClaudeCode, "claude-fable-5-1") => Some(DEFAULT_FABLE_DEFINITION),
            (ProviderKind::Codex, "gpt-6-astra") => Some(DEFAULT_ASTRA_DEFINITION),
            _ => None,
        }
    }

    pub fn builtin_child_definition(provider: ProviderKind, model: &str) -> Option<&'static str> {
        match (provider, model) {
            (ProviderKind::Codex, "gpt-6-sol") => Some(DEFAULT_SOL_6_DEFINITION),
            (ProviderKind::Codex, "gpt-6-astra") => Some(DEFAULT_GPT_6_EXECUTION_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-opus-5-5" | "claude-opus-5") => {
                Some(DEFAULT_OPUS_DEFINITION)
            }
            _ => None,
        }
    }

    /// A model occurs once per role. Migration combines distinct notes within
    /// each list; new patches keep the first row without mutating it.
    pub fn deduplicate_models(&mut self, merge_notes: bool) {
        for models in [&mut self.decision_models, &mut self.child_models] {
            let mut unique: Vec<OrchestrateChildModel> = Vec::new();
            for mut entry in std::mem::take(models) {
                entry.model = entry.model.trim().to_string();
                if let Some(existing) = unique.iter_mut().find(|existing| {
                    existing.provider == entry.provider && existing.model == entry.model
                }) {
                    if merge_notes
                        && !entry.description.is_empty()
                        && !existing.description.contains(&entry.description)
                    {
                        if !existing.description.is_empty() {
                            existing.description.push_str("\n\n");
                        }
                        existing.description.push_str(&entry.description);
                    }
                } else {
                    unique.push(entry);
                }
            }
            *models = unique;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackReviewSettings {
    #[serde(default = "default_title_provider")]
    pub provider: ProviderKind,
    #[serde(default = "default_title_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl Default for FallbackReviewSettings {
    fn default() -> Self {
        Self {
            provider: default_title_provider(),
            model: default_title_model(),
            profile_id: None,
        }
    }
}

impl FallbackReviewSettings {
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
    /// When observations include a screenshot. Absent in legacy files → `auto`.
    #[serde(default)]
    pub image_mode: ImageMode,
    /// When false the tools are observe-only (`act_ui` rejects every action).
    /// Defaults to TRUE and tolerates an absent field in legacy files.
    #[serde(default = "default_true")]
    pub allow_input: bool,
    /// Permit an opt-in foreground HID retry for keyboard actions when
    /// background PID delivery cannot be initialized.
    #[serde(default)]
    pub allow_foreground_fallback: bool,
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
            allow_foreground_fallback: false,
            show_agent_cursor: true,
        }
    }
}

/// Settings for the embedded preview browser (Settings → Browser) and the
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
    Language(Option<String>),
    ThemeMode(ThemeMode),
    WordWrapDiffs(bool),
    SkipDeleteConfirmation(bool),
    AutoOpenTaskPanel(bool),
    LiveCommandPanelDisabled(bool),
    SidebarProviderMarks(bool),
    ProviderUpdateChecksDisabled(bool),
    InactiveFrameThrottleDisabled(bool),
    AbortOnModelFallback(bool),
    ResumeOnLimitReset(bool),
    FallbackReviewAdvisor(bool),
    AutoArchiveDisabled(bool),
    AutoArchiveMaxIdleDays(u32),
    AutoArchiveKeepCount(usize),
    AutoArchiveNoticeShown(bool),
    OrchestrateDecisionModels(Vec<OrchestrateChildModel>),
    OrchestrateChildModels(Vec<OrchestrateChildModel>),
    OrchestrateChildApproval(ChildApprovalMode),
    OrchestrateChildWorktrees(bool),
    OrchestrateArchiveOnComplete(bool),
    ComputerUseEnabled(bool),
    ComputerUseImageMode(ImageMode),
    ComputerUseAllowInput(bool),
    ComputerUseAllowForegroundFallback(bool),
    ComputerUseShowAgentCursor(bool),
    BrowserEnabled(bool),
    BrowserHomeUrl(Option<String>),
    BrowserAllowEvaluate(bool),
    TitleGenerationProvider(ProviderKind),
    TitleGenerationModel(String),
    TitleGenerationProfileId(Option<String>),
    FallbackReviewProvider(ProviderKind),
    FallbackReviewModel(String),
    FallbackReviewProfileId(Option<String>),
    SidebarLayout(SidebarLayout),
    RemoteHostingEnabled(bool),
    Traverse(TraverseSetting),
    RemoteHostName(Option<String>),
    LastProject(Option<String>),
}

/// Which Traverse instance this machine publishes to while hosting: relay
/// fallback and wide-area discovery for devices off the LAN.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TraverseSetting {
    /// The official service.
    #[default]
    Official,
    /// A self-hosted instance, by the base URL its manifest is served from.
    Custom { url: String },
    /// No relay and no wide-area discovery: devices reach this machine on
    /// the LAN or at the addresses an invite carries.
    Off,
}

impl TraverseSetting {
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
    /// None follows the operating-system language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-provider cards (Settings → Providers), keyed by [`provider_key`].
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
    /// When true, the right-side plan/task panel opens automatically the first
    /// time steps appear in a turn (unless the user closed it during that turn).
    /// Absent in legacy files (defaults to off).
    #[serde(default)]
    pub auto_open_task_panel: bool,
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
    #[serde(default = "default_true")]
    pub abort_on_model_fallback: bool,
    #[serde(default = "default_true")]
    pub resume_on_limit_reset: bool,
    #[serde(default)]
    pub fallback_review_advisor: bool,
    /// Whether automatic archiving is DISABLED. Stored inverted so the feature
    /// defaults to on even for legacy settings files that lack the field.
    #[serde(default)]
    pub auto_archive_disabled: bool,
    /// Threads must be idle longer than this many days before auto-archive.
    #[serde(default = "default_auto_archive_max_idle_days")]
    pub auto_archive_max_idle_days: u32,
    /// Newest siblings preserved regardless of age by auto-archive.
    #[serde(default = "default_auto_archive_keep_count")]
    pub auto_archive_keep_count: usize,
    /// Whether the one-time first-auto-archive explanation has been shown.
    #[serde(default)]
    pub auto_archive_notice_shown: bool,
    /// Built-in orchestration identities and child-model routing table.
    #[serde(default, skip_serializing_if = "OrchestrateSettings::is_default")]
    pub orchestrate: OrchestrateSettings,
    /// Global desktop computer-use feature settings. Absent in legacy files,
    /// where the feature remains disabled by default.
    #[serde(default)]
    pub computer_use: ComputerUseSettings,
    /// Embedded preview browser settings. Skipped when default (like
    /// `orchestrate`), so legacy files stay clean and load with the defaults.
    #[serde(default, skip_serializing_if = "BrowserSettings::is_default")]
    pub browser: BrowserSettings,
    /// Provider/model used to generate a concise title for new threads.
    #[serde(default, skip_serializing_if = "TitleGenerationSettings::is_default")]
    pub title_generation: TitleGenerationSettings,
    #[serde(default, skip_serializing_if = "FallbackReviewSettings::is_default")]
    pub fallback_review: FallbackReviewSettings,
    /// Ids of project groups the user has collapsed in the sidebar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_projects: Vec<String>,
    /// Ids of parent threads whose child rows are folded in the thread list.
    /// Host-owned so every client shows the same list; reseeded at host
    /// start so threads that already had children begin folded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_threads: Vec<String>,
    /// Model ids the user has starred in the model picker (favorites float to
    /// the top and are shown first under the star filter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_models: Vec<String>,
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
    /// Traverse instance used while hosting. Absent in legacy files → official.
    #[serde(default, skip_serializing_if = "TraverseSetting::is_default")]
    pub traverse: TraverseSetting,
    /// Name this host advertises while pairing and on the discovery beacon.
    /// None uses the machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host_name: Option<String>,
    /// Per-session last-visited time (unix secs), keyed by session id. A session
    /// whose `updated_at` exceeds its last-visited time (and isn't active) shows
    /// an unread dot. Opening a thread refreshes it; "Mark unread" clears it.
    /// UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_visited: HashMap<String, u64>,
    /// Project the user last navigated to or started a thread in. A workspace
    /// with no conversation open (launch, or the thread on screen going away)
    /// opens this project's new-thread draft. Set from user navigation only, so
    /// background activity cannot move it. UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_project_id: Option<String>,
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

/// Factory value for [`Settings::auto_archive_max_idle_days`]. Public so the
/// settings page can tell an overridden field from an untouched one.
pub const DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS: u32 = 7;
/// Factory value for [`Settings::auto_archive_keep_count`].
pub const DEFAULT_AUTO_ARCHIVE_KEEP_COUNT: usize = 30;

const fn default_auto_archive_max_idle_days() -> u32 {
    DEFAULT_AUTO_ARCHIVE_MAX_IDLE_DAYS
}

const fn default_auto_archive_keep_count() -> usize {
    DEFAULT_AUTO_ARCHIVE_KEEP_COUNT
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: None,
            providers: BTreeMap::new(),
            profiles: BTreeMap::new(),
            codex_binary: None,
            claude_binary: None,
            theme_mode: ThemeMode::default(),
            sidebar_collapsed: false,
            word_wrap_diffs: false,
            skip_delete_confirmation: false,
            auto_open_task_panel: false,
            live_command_panel_disabled: false,
            sidebar_provider_marks: false,
            provider_update_checks_disabled: false,
            inactive_frame_throttle_disabled: false,
            abort_on_model_fallback: true,
            resume_on_limit_reset: true,
            fallback_review_advisor: false,
            auto_archive_disabled: false,
            auto_archive_max_idle_days: default_auto_archive_max_idle_days(),
            auto_archive_keep_count: default_auto_archive_keep_count(),
            auto_archive_notice_shown: false,
            orchestrate: OrchestrateSettings::default(),
            computer_use: ComputerUseSettings::default(),
            browser: BrowserSettings::default(),
            title_generation: TitleGenerationSettings::default(),
            fallback_review: FallbackReviewSettings::default(),
            collapsed_projects: Vec::new(),
            collapsed_threads: Vec::new(),
            favorite_models: Vec::new(),
            project_sort: ProjectSort::default(),
            sidebar_layout: SidebarLayout::default(),
            remote_hosting_enabled: false,
            traverse: TraverseSetting::default(),
            remote_host_name: None,
            last_visited: HashMap::new(),
            last_project_id: None,
            acp_agents: BTreeMap::new(),
            unknown: serde_json::Map::new(),
        }
    }
}

impl Settings {
    /// Apply one field-scoped mutation without replacing sibling fields.
    pub fn apply(&mut self, patch: SettingsPatch) {
        match patch {
            SettingsPatch::Language(value) => self.language = value,
            SettingsPatch::ThemeMode(value) => self.theme_mode = value,
            SettingsPatch::WordWrapDiffs(value) => self.word_wrap_diffs = value,
            SettingsPatch::SkipDeleteConfirmation(value) => {
                self.skip_delete_confirmation = value;
            }
            SettingsPatch::AutoOpenTaskPanel(value) => self.auto_open_task_panel = value,
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
            SettingsPatch::AbortOnModelFallback(value) => {
                self.abort_on_model_fallback = value;
            }
            SettingsPatch::ResumeOnLimitReset(value) => {
                self.resume_on_limit_reset = value;
            }
            SettingsPatch::FallbackReviewAdvisor(value) => {
                self.fallback_review_advisor = value;
            }
            SettingsPatch::AutoArchiveDisabled(value) => self.auto_archive_disabled = value,
            SettingsPatch::AutoArchiveMaxIdleDays(value) => {
                self.auto_archive_max_idle_days = value;
            }
            SettingsPatch::AutoArchiveKeepCount(value) => {
                self.auto_archive_keep_count = value;
            }
            SettingsPatch::AutoArchiveNoticeShown(value) => {
                self.auto_archive_notice_shown = value;
            }
            SettingsPatch::OrchestrateDecisionModels(value) => {
                self.orchestrate.decision_models = value;
                self.orchestrate.deduplicate_models(false);
            }
            SettingsPatch::OrchestrateChildModels(value) => {
                self.orchestrate.child_models = value;
                self.orchestrate.deduplicate_models(false);
            }
            SettingsPatch::OrchestrateChildApproval(value) => {
                self.orchestrate.child_approval = value;
            }
            SettingsPatch::OrchestrateChildWorktrees(value) => {
                self.orchestrate.child_worktrees = value;
            }
            SettingsPatch::OrchestrateArchiveOnComplete(value) => {
                self.orchestrate.archive_on_complete = value;
            }
            SettingsPatch::ComputerUseEnabled(value) => self.computer_use.enabled = value,
            SettingsPatch::ComputerUseImageMode(value) => self.computer_use.image_mode = value,
            SettingsPatch::ComputerUseAllowInput(value) => self.computer_use.allow_input = value,
            SettingsPatch::ComputerUseAllowForegroundFallback(value) => {
                self.computer_use.allow_foreground_fallback = value
            }
            SettingsPatch::ComputerUseShowAgentCursor(value) => {
                self.computer_use.show_agent_cursor = value
            }
            SettingsPatch::BrowserEnabled(value) => self.browser.enabled = value,
            SettingsPatch::BrowserHomeUrl(value) => self.browser.home_url = value,
            SettingsPatch::BrowserAllowEvaluate(value) => self.browser.allow_evaluate = value,
            SettingsPatch::TitleGenerationProvider(value) => {
                self.title_generation.provider = value;
            }
            SettingsPatch::TitleGenerationModel(value) => self.title_generation.model = value,
            SettingsPatch::TitleGenerationProfileId(value) => {
                self.title_generation.profile_id = value;
            }
            SettingsPatch::FallbackReviewProvider(value) => {
                self.fallback_review.provider = value;
            }
            SettingsPatch::FallbackReviewModel(value) => self.fallback_review.model = value,
            SettingsPatch::FallbackReviewProfileId(value) => {
                self.fallback_review.profile_id = value;
            }
            SettingsPatch::SidebarLayout(value) => self.sidebar_layout = value,
            SettingsPatch::RemoteHostingEnabled(value) => self.remote_hosting_enabled = value,
            SettingsPatch::Traverse(value) => self.traverse = value,
            SettingsPatch::RemoteHostName(value) => self.remote_host_name = value,
            SettingsPatch::LastProject(value) => self.last_project_id = value,
        }
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

    /// The built-in profile id for a native protocol (its [`provider_key`]).
    /// This is the id a session carries when it uses the default, non-custom
    /// configuration for its kind.
    pub fn builtin_profile_id(kind: ProviderKind) -> &'static str {
        provider_key(kind)
    }

    /// Whether `id` names a built-in provider profile.
    pub fn is_builtin_profile_id(id: &str) -> bool {
        Self::builtin_kind_from_id(id).is_some()
    }

    /// The protocol kind of a built-in profile id, if it is one. Used to route a
    /// mutation of a built-in profile back to its `providers` card.
    pub fn builtin_kind_from_id(id: &str) -> Option<ProviderKind> {
        match id {
            "claude" => Some(ProviderKind::ClaudeCode),
            "codex" => Some(ProviderKind::Codex),
            "pi" => Some(ProviderKind::Pi),
            "opencode" => Some(ProviderKind::OpenCode),
            "acp" => Some(ProviderKind::Acp),
            _ => None,
        }
    }

    /// Resolve a profile id to its protocol kind and effective card settings.
    /// Built-in ids resolve to the matching `providers` card; anything else to
    /// a user-created `profiles` entry. `None` for an unknown id.
    pub fn resolved_profile(&self, id: &str) -> Option<ResolvedProfile> {
        if let Some(kind) = Self::builtin_kind_from_id(id) {
            return Some(ResolvedProfile {
                id: id.to_string(),
                kind,
                settings: self.provider(kind),
            });
        }
        self.profiles.get(id).map(|profile| ResolvedProfile {
            id: id.to_string(),
            kind: profile.kind,
            settings: profile.settings.clone(),
        })
    }

    /// Every selectable profile that drives `kind`: the built-in first, then any
    /// user profiles of that kind in id order. This is what the provider/model
    /// picker iterates.
    pub fn profiles_for_kind(&self, kind: ProviderKind) -> Vec<ResolvedProfile> {
        let mut out = vec![ResolvedProfile {
            id: provider_key(kind).to_string(),
            kind,
            settings: self.provider(kind),
        }];
        for (id, profile) in &self.profiles {
            if profile.kind == kind {
                out.push(ResolvedProfile {
                    id: id.clone(),
                    kind,
                    settings: profile.settings.clone(),
                });
            }
        }
        out
    }

    /// A profile's card title: its display-name override, else — for built-ins —
    /// the driver label, else the id. Used by the sidebar / picker / status row.
    pub fn profile_display_name(&self, id: &str) -> String {
        let Some(profile) = self.resolved_profile(id) else {
            return id.to_string();
        };
        if let Some(name) = profile
            .settings
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
        if Self::is_builtin_profile_id(id) {
            provider_label(profile.kind).to_string()
        } else {
            id.to_string()
        }
    }

    /// Turn a human name into a stable, unique profile id (slug). Never collides
    /// with a built-in id or an existing profile id.
    pub fn allocate_profile_id(&self, name: &str) -> String {
        let base: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "profile" } else { base };
        let taken =
            |id: &str, s: &Settings| Self::is_builtin_profile_id(id) || s.profiles.contains_key(id);
        if !taken(base, self) {
            return base.to_string();
        }
        let mut n = 2;
        loop {
            let candidate = format!("{base}-{n}");
            if !taken(&candidate, self) {
                return candidate;
            }
            n += 1;
        }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_settings_preserve_access_policy_and_accept_partial_feature_blocks() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert!(!legacy.auto_archive_disabled);
        assert_eq!(legacy.auto_archive_max_idle_days, 7);
        assert_eq!(legacy.auto_archive_keep_count, 30);
        assert!(!legacy.auto_archive_notice_shown);
        assert!(!legacy.sidebar_provider_marks);
        assert!(!legacy.sidebar_collapsed);
        assert!(!legacy.remote_hosting_enabled);
        assert_eq!(legacy.traverse, TraverseSetting::Official);
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
            "title_generation":{"provider":"codex","model":"m"}
        }"#,
        )
        .unwrap();
        assert!(partial.computer_use.enabled);
        assert_eq!(partial.computer_use.image_mode, ImageMode::Auto);
        assert!(partial.computer_use.allow_input);
        assert!(!partial.computer_use.allow_foreground_fallback);
        assert!(partial.computer_use.show_agent_cursor);
        assert!(!partial.browser.enabled);
        assert!(partial.browser.allow_evaluate);
        assert_eq!(
            partial.orchestrate.child_approval,
            ChildApprovalMode::Orchestrator
        );
        assert!(!partial.orchestrate.child_worktrees);
        assert_eq!(partial.title_generation.profile_id, None);

        let child: OrchestrateChildModel =
            serde_json::from_str(r#"{"provider":"codex","model":"m","enabled":true}"#).unwrap();
        assert_eq!(child.profile_id, None);
        assert!(!child.fast);
    }

    #[test]
    fn provider_color_maps_builtins_to_brand_and_custom_providers_to_distinct_palette_slots() {
        let mut settings = Settings::default();
        assert_eq!(settings.provider_color("claude"), 0xD97757);
        assert_eq!(settings.provider_color("codex"), 0x8B5CF6);
        assert_eq!(settings.provider_color("pi"), 0x4D9ABF);
        assert_eq!(settings.provider_color("opencode"), 0x22A06B);

        for id in ["work-claude", "proxy-codex", "lab"] {
            settings.profiles.insert(
                id.into(),
                ProviderProfile {
                    kind: ProviderKind::ClaudeCode,
                    settings: ProviderSettings::default(),
                },
            );
        }
        for id in ["gemini", "goose"] {
            settings.acp_agents.insert(
                id.into(),
                InstalledAcpAgent {
                    id: id.into(),
                    name: id.into(),
                    version: String::new(),
                    icon: None,
                    launch: agent::AcpLaunch::Npx {
                        package: id.into(),
                        args: Vec::new(),
                        env: Vec::new(),
                    },
                    enabled: true,
                    env: Vec::new(),
                    launch_args: None,
                },
            );
        }
        let keys = [
            "work-claude",
            "proxy-codex",
            "lab",
            "acp:gemini",
            "acp:goose",
        ];
        let colors: Vec<u32> = keys.iter().map(|k| settings.provider_color(k)).collect();
        let mut distinct = colors.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), keys.len(), "{colors:06X?}");
        for color in &colors {
            assert!(PROVIDER_COLOR_PALETTE.contains(color));
            assert!(
                ![0xD97757, 0x8B5CF6, 0x4D9ABF, 0x22A06B].contains(color),
                "custom providers never take a brand color"
            );
        }
        // Deterministic across calls, and a deleted profile still has a color.
        assert_eq!(
            colors,
            keys.iter()
                .map(|k| settings.provider_color(k))
                .collect::<Vec<_>>()
        );
        assert!(PROVIDER_COLOR_PALETTE.contains(&settings.provider_color("gone")));
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
        assert!(!legacy.pi.native_approvals);

        let old_json = r#"{
            "pi_trust_project_extensions": true,
            "pi_native_approvals": true
        }"#;
        let settings: ProviderSettings = serde_json::from_str(old_json).unwrap();
        assert!(settings.pi.trust_project_extensions);
        assert!(settings.pi.native_approvals);
        let serialized = serde_json::to_value(&settings).unwrap();
        assert_eq!(serialized["pi_trust_project_extensions"], true);
        assert_eq!(serialized["pi_native_approvals"], true);
        assert!(serialized.get("pi").is_none());

        let legacy_patch: ProfileConfigurationPatch = serde_json::from_str(
            r#"{
                "display_name": null,
                "accent_color": null,
                "env": [],
                "binary_path": null,
                "home_path": null,
                "launch_args": null,
                "custom_models": [],
                "hidden_models": []
            }"#,
        )
        .unwrap();
        assert!(!legacy_patch.pi.trust_project_extensions);
        assert!(!legacy_patch.pi.native_approvals);
    }

    #[test]
    fn orchestrate_defaults_and_legacy_migration() {
        let defaults = OrchestrateSettings::default();
        assert_eq!(
            defaults
                .decision_models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-6-astra", "claude-fable-5-1"]
        );
        assert_eq!(defaults.child_models.len(), 2);
        assert_eq!(defaults.child_models[0].model, "gpt-6-sol");
        assert_eq!(defaults.child_models[0].provider, ProviderKind::Codex);
        assert!(!defaults.child_models[0].fast);
        assert!(
            defaults.child_models[0]
                .description
                .contains("Start at medium")
        );
        assert_ne!(
            defaults.child_models[0].description,
            defaults.decision_models[0].description
        );
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&serde_json::to_string(&defaults).unwrap())
                .unwrap(),
            defaults,
            "role-specific Astra definitions survive legacy deduplication"
        );
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(legacy.orchestrate, defaults);
        let mut old = serde_json::to_value(&defaults).unwrap();
        old.as_object_mut().unwrap().remove("decision_models");
        old["child_models"][0] = serde_json::json!({
            "provider": "codex",
            "model": "gpt-5.6-sol",
            "enabled": true,
            "fast": false,
            "description": OLD_DEFAULT_SOL_DEFINITION,
        });
        old["generic_identity"] = "old self-concept".into();
        old["model_identities"] = serde_json::json!([{"provider":"codex","model":"gpt-5.6-sol","identity":"old identity"}]);
        old["child_models"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(&defaults.decision_models[0]).unwrap());
        let mut fable = defaults.decision_models[1].clone();
        fable.enabled = false;

        fable.description = "Custom consultation guidance".into();
        old["child_models"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(&fable).unwrap());
        let migrated: OrchestrateSettings = serde_json::from_value(old).unwrap();
        assert_eq!(migrated.child_models, defaults.child_models);
        assert_eq!(migrated.decision_models[1], fable);
        let json = serde_json::to_string(&migrated).unwrap();
        assert!(!json.contains("identity"));
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&json).unwrap(),
            migrated
        );
        let empty: OrchestrateSettings =
            serde_json::from_str(r#"{"decision_models":[],"child_models":[]}"#).unwrap();
        let legacy_empty: OrchestrateSettings =
            serde_json::from_str(r#"{"generic_identity":"old instructions"}"#).unwrap();
        assert!(
            legacy_empty.child_models.is_empty(),
            "omitted legacy execution list stays empty"
        );
        assert!(empty.decision_models.is_empty());
        assert!(empty.child_models.is_empty());
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&serde_json::to_string(&empty).unwrap())
                .unwrap(),
            empty
        );
    }

    #[test]
    fn orchestrate_migrates_untouched_sol_and_opus_defaults() {
        let old_json = r#"{
            "decision_models": [],
            "child_models": [
                {
                    "provider": "codex",
                    "model": "gpt-5.6-sol",
                    "enabled": true,
                    "fast": false,
                    "description": "Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely."
                },
                {
                    "provider": "claude_code",
                    "model": "claude-opus-5",
                    "enabled": true,
                    "fast": false,
                    "description": "Execution model for agentic coding, cross-file implementation, refactoring, debugging, and review. Consider it alongside Sol across providers, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely."
                }
            ]
        }"#;
        let migrated: OrchestrateSettings = serde_json::from_str(old_json).unwrap();

        assert_eq!(
            migrated.child_models,
            OrchestrateSettings::default().child_models
        );
    }

    #[test]
    fn orchestrate_keeps_customised_opus_5_rows() {
        let old_json = r#"{
            "decision_models": [],
            "child_models": [
                {"provider": "claude_code", "model": "claude-opus-5", "profile_id": "corp", "description": "Execution model for agentic coding, cross-file implementation, refactoring, debugging, and review across providers, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely."},
                {"provider": "claude_code", "model": "claude-opus-5", "description": "Reviewer only."}
            ]
        }"#;
        let migrated: OrchestrateSettings = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            migrated.child_models.len(),
            1,
            "same model merges into one row"
        );
        let opus = &migrated.child_models[0];
        assert_eq!(opus.model, "claude-opus-5");
        assert_eq!(opus.profile_id.as_deref(), Some("corp"));
        assert!(opus.description.contains("Reviewer only."));

        // A user who already added Opus 5.5 keeps the untouched Opus 5 row as is.
        let old_json = format!(
            r#"{{"decision_models":[],"child_models":[
                {{"provider":"claude_code","model":"claude-opus-5-5","description":"Mine."}},
                {{"provider":"claude_code","model":"claude-opus-5","description":{}}}
            ]}}"#,
            serde_json::to_string(OLD_DEFAULT_OPUS_DEFINITION).unwrap()
        );
        let migrated: OrchestrateSettings = serde_json::from_str(&old_json).unwrap();
        let models: Vec<_> = migrated
            .child_models
            .iter()
            .map(|entry| (entry.model.as_str(), entry.description.as_str()))
            .collect();
        assert_eq!(
            models,
            [
                ("claude-opus-5-5", "Mine."),
                ("claude-opus-5", DEFAULT_OPUS_DEFINITION)
            ]
        );
    }

    #[test]
    fn orchestrate_refreshes_previous_opus_5_5_default_text() {
        let old_json = format!(
            r#"{{"decision_models":[],"child_models":[{{"provider":"claude_code","model":"claude-opus-5-5","description":{}}}]}}"#,
            serde_json::to_string(PREVIOUS_DEFAULT_OPUS_DEFINITION).unwrap()
        );
        let migrated: OrchestrateSettings = serde_json::from_str(&old_json).unwrap();
        assert_eq!(
            migrated.child_models[0].description,
            DEFAULT_OPUS_DEFINITION
        );
        let customized = old_json.replace(PREVIOUS_DEFAULT_OPUS_DEFINITION, "mine");
        let kept: OrchestrateSettings = serde_json::from_str(&customized).unwrap();
        assert_eq!(kept.child_models[0].description, "mine");
    }

    #[test]
    fn orchestrate_moves_untouched_astra_executors_to_sol_6() {
        for text in [
            OLD_DEFAULT_GPT_6_EXECUTION_DEFINITION,
            DEFAULT_GPT_6_EXECUTION_DEFINITION,
        ] {
            let old_json = format!(
                r#"{{"decision_models":[],"child_models":[{{"provider":"codex","model":"gpt-6-astra","description":{},"enabled":true,"fast":false}}]}}"#,
                serde_json::to_string(text).unwrap()
            );
            let migrated: OrchestrateSettings = serde_json::from_str(&old_json).unwrap();
            assert_eq!(
                migrated.child_models,
                [builtin_model(
                    ProviderKind::Codex,
                    "gpt-6-sol",
                    DEFAULT_SOL_6_DEFINITION
                )]
            );
            // Customised text, a profile, or a flipped switch keeps Astra.
            for variant in [
                old_json.replace(text, "mine"),
                old_json.replace(r#""enabled":true"#, r#""enabled":false"#),
                old_json.replace(r#""fast":false"#, r#""fast":true"#),
                old_json.replace(
                    r#""provider":"codex","#,
                    r#""provider":"codex","profile_id":"corp","#,
                ),
            ] {
                let kept: OrchestrateSettings = serde_json::from_str(&variant).unwrap();
                assert_eq!(kept.child_models.len(), 1, "input: {variant}");
                assert_eq!(
                    kept.child_models[0].model, "gpt-6-astra",
                    "input: {variant}"
                );
            }
        }
        // An untouched Astra row next to a user-added Sol 6 row is dropped
        // rather than duplicating Sol 6.
        let both = format!(
            r#"{{"decision_models":[],"child_models":[{{"provider":"codex","model":"gpt-6-sol","description":"Mine."}},{{"provider":"codex","model":"gpt-6-astra","description":{},"enabled":true,"fast":false}}]}}"#,
            serde_json::to_string(DEFAULT_GPT_6_EXECUTION_DEFINITION).unwrap()
        );
        let migrated: OrchestrateSettings = serde_json::from_str(&both).unwrap();
        assert_eq!(migrated.child_models.len(), 1);
        assert_eq!(migrated.child_models[0].description, "Mine.");
    }

    #[test]
    fn orchestrate_preserves_every_customized_sol_shape() {
        let customized = [
            r#"{"description":"custom","enabled":true,"fast":false}"#,
            r#"{"description":"Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely.","profile_id":"custom","enabled":true,"fast":false}"#,
            r#"{"description":"Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely.","enabled":false,"fast":false}"#,
            r#"{"description":"Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely.","enabled":true,"fast":true}"#,
        ];
        for row in customized {
            let expected: serde_json::Value = serde_json::from_str(row).unwrap();
            let old_json = format!(
                r#"{{"decision_models":[],"child_models":[{{"provider":"codex","model":"gpt-5.6-sol",{}}}]}}"#,
                &row[1..row.len() - 1]
            );
            let migrated: OrchestrateSettings = serde_json::from_str(&old_json).unwrap();
            assert_eq!(migrated.child_models.len(), 1, "input: {row}");
            let actual = &migrated.child_models[0];
            assert_eq!(actual.model, "gpt-5.6-sol", "input: {row}");
            assert_eq!(
                actual.description,
                expected["description"].as_str().unwrap(),
                "input: {row}"
            );
            assert_eq!(
                actual.enabled,
                expected["enabled"].as_bool().unwrap(),
                "input: {row}"
            );
            assert_eq!(
                actual.fast,
                expected["fast"].as_bool().unwrap(),
                "input: {row}"
            );
            assert_eq!(
                actual.profile_id.as_deref(),
                expected["profile_id"].as_str(),
                "input: {row}"
            );
        }
    }

    #[test]
    fn orchestrate_migration_does_not_duplicate_existing_execution_sol_6() {
        let old_json = r#"{
            "decision_models": [],
            "child_models": [
                {
                    "provider": "codex",
                    "model": "gpt-5.6-sol",
                    "enabled": true,
                    "fast": false,
                    "description": "Execution model for scoped implementation, debugging with a reproduction, migrations, code review, data analysis, and evidence gathering. Use medium for routine work with a clear brief; increase through high and xhigh as interacting constraints or reasoning difficulty grow; use max for the hardest well-defined problems or when a lower effort has demonstrably stalled. Choose any supported effort that fits the task, not just the endpoints. Keep unrelated improvements out of scope. Report the concrete result and relevant checks concisely."
                },
                {
                    "provider": "codex",
                    "model": "gpt-6-sol",
                    "profile_id": "custom-codex",
                    "enabled": false,
                    "fast": true,
                    "description": "User execution guidance"
                }
            ]
        }"#;
        let migrated: OrchestrateSettings = serde_json::from_str(old_json).unwrap();

        assert_eq!(migrated.child_models.len(), 1);
        assert_eq!(migrated.child_models[0].model, "gpt-6-sol");
        assert_eq!(
            migrated.child_models[0].profile_id.as_deref(),
            Some("custom-codex")
        );
        assert_eq!(
            migrated.child_models[0].description,
            "User execution guidance"
        );
        assert!(!migrated.child_models[0].enabled);
        assert!(migrated.child_models[0].fast);
    }

    #[test]
    fn orchestrate_merges_legacy_tiers_and_upgrades_bundled_models() {
        let settings: OrchestrateSettings = serde_json::from_value(serde_json::json!({
            "child_models": [
                {"provider":"codex", "model":"gpt-5.6-sol", "default_effort":"medium", "description":"Routine work", "enabled":false, "profile_id":"custom", "fast":true},
                {"provider":"codex", "model":"gpt-5.6-sol", "effort":"max", "description":"Difficult bugs"},
                {"provider":"claude_code", "model":"claude-sonnet-5", "effort":"high", "description":LEGACY_SONNET_CHILD_DEFINITION},
                {"provider":"claude_code", "model":"claude-opus-4-8", "effort":"high", "description":LEGACY_OPUS_CHILD_DEFINITION},
                {"provider":"claude_code", "model":"claude-fable-5", "effort":"high", "description":LEGACY_FABLE_DECISION_DEFINITION}
            ]
        })).unwrap();
        assert_eq!(settings.child_models.len(), 2);
        let sol = &settings.child_models[0];
        assert!(sol.description.contains("medium effort: Routine work"));
        assert!(sol.description.contains("max effort: Difficult bugs"));
        assert!(!sol.enabled);
        assert!(sol.fast);
        assert_eq!(sol.profile_id.as_deref(), Some("custom"));
        assert_eq!(settings.child_models[1].model, "claude-opus-5-5");
        assert_eq!(
            settings.child_models[1].description,
            DEFAULT_OPUS_DEFINITION
        );
        assert_eq!(settings.decision_models[1].model, "claude-fable-5-1");
        assert_eq!(
            settings.decision_models[1].description,
            DEFAULT_FABLE_DEFINITION
        );
        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("\"effort\""));
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&serialized).unwrap(),
            settings
        );
    }

    #[test]
    fn orchestrate_capabilities_use_catalog_and_cap_collaboration() {
        let catalog = vec![ModelSpec {
            id: "custom".into(),
            display_name: "Custom".into(),
            is_default: false,
            options: vec![OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Effort".into(),
                default_value: None,
                options: ["medium", "high", "deep"]
                    .into_iter()
                    .map(|value| agent::SelectOption {
                        value: value.into(),
                        label: value.into(),
                        description: None,
                    })
                    .collect(),
            }],
        }];
        assert_eq!(
            orchestrate_efforts(ProviderKind::Codex, "custom", &catalog, false),
            ["medium", "high", "deep"]
        );
        assert_eq!(
            orchestrate_efforts(ProviderKind::Codex, "custom", &catalog, true),
            ["medium", "high"]
        );
        assert!(orchestrate_efforts(ProviderKind::Codex, "unknown", &catalog, false).is_empty());
        for entry in OrchestrateSettings::default().decision_models {
            assert_eq!(
                orchestrate_efforts(entry.provider, &entry.model, &[], true),
                ["medium", "high"]
            );
        }
    }

    #[test]
    fn orchestrate_settings_patches_deduplicate_within_each_role() {
        let mut settings = Settings::default();
        let peer = settings.orchestrate.decision_models[0].clone();
        // The same model serves both roles with role-specific guidance.
        let executor = builtin_model(
            peer.provider,
            &peer.model,
            DEFAULT_GPT_6_EXECUTION_DEFINITION,
        );
        assert_ne!(executor.description, peer.description);
        settings.orchestrate.child_models[0] = executor.clone();

        let mut duplicate = executor.clone();
        duplicate.profile_id = Some("another-endpoint".into());
        duplicate.description = "must not overwrite".into();
        let mut children = settings.orchestrate.child_models.clone();
        children.push(duplicate);
        settings.apply(SettingsPatch::OrchestrateChildModels(children));
        assert_eq!(settings.orchestrate.child_models.len(), 2);
        assert_eq!(settings.orchestrate.child_models[0], executor);

        let mut duplicate = peer.clone();
        duplicate.profile_id = Some("another-endpoint".into());
        duplicate.description = "must not overwrite".into();
        let mut decisions = settings.orchestrate.decision_models.clone();
        decisions.push(duplicate);
        settings.apply(SettingsPatch::OrchestrateDecisionModels(decisions));
        assert_eq!(settings.orchestrate.decision_models.len(), 2);
        assert_eq!(settings.orchestrate.decision_models[0], peer);
        assert_eq!(settings.orchestrate.child_models[0], executor);
    }

    #[test]
    fn resolves_builtin_and_user_profiles() {
        let mut settings = Settings::default();
        // Give the built-in Claude card a base URL and a display name.
        settings.provider_mut(ProviderKind::ClaudeCode).display_name = Some("Claude".into());

        // A user profile driving the same protocol (third-party Claude).
        let id = settings.allocate_profile_id("Klaude Kode");
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

        // Display names: built-in falls back to label; user shows its name.
        assert_eq!(settings.profile_display_name("claude"), "Claude");
        assert_eq!(settings.profile_display_name(&id), "Klaude Kode");
        assert_eq!(settings.resolved_profile("nope"), None);

        // A second profile of the same name gets a distinct id.
        assert_eq!(settings.allocate_profile_id("Klaude Kode"), "klaude-kode-2");
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
        assert_eq!(legacy.traverse, TraverseSetting::Official);
        assert_eq!(legacy.remote_host_name.as_deref(), Some("Studio"));
        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(written.get("remote_port"), None);
        assert_eq!(written.get("traverse"), None, "the default is not written");

        let custom: Settings = serde_json::from_str(
            r#"{"traverse":{"mode":"custom","url":"https://traverse.example/"}}"#,
        )
        .unwrap();
        assert_eq!(
            custom.traverse,
            TraverseSetting::Custom {
                url: "https://traverse.example/".into()
            }
        );
        let off: Settings = serde_json::from_str(r#"{"traverse":{"mode":"off"}}"#).unwrap();
        assert_eq!(off.traverse, TraverseSetting::Off);

        let mut patched = Settings::default();
        patched.apply(SettingsPatch::Traverse(TraverseSetting::Off));
        assert_eq!(
            serde_json::to_value(&patched).unwrap().get("traverse"),
            Some(&serde_json::json!({"mode": "off"}))
        );
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
