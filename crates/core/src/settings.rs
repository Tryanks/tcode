//! Persisted application settings domain data.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use agent::ProviderKind;
use serde::{Deserialize, Serialize};

use crate::acp::InstalledAcpAgent;

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

impl ProjectSort {
    /// The next mode in the cycle (RecentActivity → NameAsc → RecentActivity).
    pub fn next(self) -> Self {
        match self {
            ProjectSort::RecentActivity => ProjectSort::NameAsc,
            ProjectSort::NameAsc => ProjectSort::RecentActivity,
        }
    }
}

/// The settings-file key for a provider ("codex" / "claude"). Stable: it keys
/// both `settings.json`'s `providers` map and `secrets.json`.
pub fn provider_key(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        // ACP agents are not one provider but many: their per-agent settings
        // live in `Settings::acp_agents`, keyed by registry id. This bucket only
        // ever holds the shared fallbacks (it is never written by the ACP card).
        ProviderKind::Acp => "acp",
    }
}

/// The provider's short, T3-style display name (the card title / picker label).
pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "Codex",
        ProviderKind::ClaudeCode => "Claude",
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
    /// Claude: `HOME` override. Codex: `CODEX_HOME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_path: Option<PathBuf>,
    /// Codex only: account-specific `CODEX_HOME` (takes precedence over `home_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_home_path: Option<PathBuf>,
    /// Claude only: extra CLI arguments appended on session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_args: Option<String>,
    /// Model slugs added by hand in the Models section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    /// Model ids hidden from the composer's model picker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_models: Vec<String>,
    /// Explicit ordering (ids listed here come first, in this order; anything
    /// else keeps its catalog order behind them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_order: Vec<String>,
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
            shadow_home_path: None,
            launch_args: None,
            custom_models: Vec::new(),
            hidden_models: Vec::new(),
            model_order: Vec::new(),
        }
    }
}

impl ProviderSettings {
    /// Claude's `Launch arguments` field, split on whitespace.
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// The home directory this provider's children should run against
    /// (`shadow_home_path` wins for Codex; `None` = inherit).
    pub fn effective_home(&self) -> Option<PathBuf> {
        self.shadow_home_path
            .clone()
            .or_else(|| self.home_path.clone())
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
/// The two *built-in* profiles (Claude Code, Codex) are not stored here — they
/// remain in [`Settings::providers`] under their [`provider_key`]. Only extra,
/// user-created profiles live in [`Settings::profiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// The protocol this profile drives. Determines which native adapter
    /// (`claude` / `codex`) spawns it and how its stdio is normalized.
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

/// A model-specific identity override for an orchestrator. Models without an
/// entry here remain fully eligible for `/orchestrate`; they inherit
/// [`OrchestrateSettings::generic_identity`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorIdentity {
    pub provider: ProviderKind,
    pub model: String,
    pub identity: String,
}

/// One (model, effort) profile the orchestrator may dispatch work to. The same
/// model may appear several times at different efforts (e.g. `gpt-5.6-sol` at
/// medium as the bulk tier and at max as the exception tier), each with its own
/// routing definition. Profiles stay in this list while paused so their
/// editable routing definition is preserved; `enabled` is the actual allow-list
/// decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrateChildModel {
    pub provider: ProviderKind,
    pub model: String,
    /// Which provider profile (endpoint config) the dispatch launches against;
    /// `None` = the kind's built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Disabled profiles retain all routing preferences but cannot receive a
    /// dispatch and are omitted from the lead model's available-fleet table.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The reasoning effort this profile dispatches at; `None` = provider
    /// default. Part of the allow-list key: a dispatch naming an effort must
    /// match an enabled profile with that effort.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "default_effort"
    )]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl OrchestrateChildModel {
    /// Whether a dispatch-supplied effort selects this profile. `None` selects
    /// any profile for the model; a named effort must match exactly
    /// (case-insensitive).
    pub fn matches_effort(&self, effort: Option<&str>) -> bool {
        match effort {
            None => true,
            Some(effort) => self
                .effort
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(effort.trim())),
        }
    }
}

const DEFAULT_ORCHESTRATOR_IDENTITY: &str = "You are the primary decision model and technical lead for this session. Your leverage is judgment: understand the problem, frame it well, decompose it, define done, route work to the cheapest adequate child model, and verify the result independently. Keep architecture, ambiguous tradeoffs, and final acceptance for yourself; delegate execution when a child can complete it from a precise brief.";

const DEFAULT_FABLE_IDENTITY: &str = "You are Fable 5, the scarcest judgment resource in this fleet: a wise owl—thoughtful, discerning, and exceptionally strong at framing, architecture, taste, and clear communication. Spend that judgment on understanding, delegation, review, and final acceptance rather than routine typing. Use high effort by default; deeper tiers usually consume more of the fleet's bottleneck without improving your decisions.";

const DEFAULT_SOL_IDENTITY: &str = "You are gpt-5.6-sol, the fleet's relentless closer: a rottweiler with an articulate report—tenacious, disciplined, and exceptional on hard, well-defined problems. As the lead, run at max effort: decision quality, not tokens, is the bottleneck in this seat. Point that tenacity at understanding, decomposition, acceptance criteria, and verification rather than typing; and since taste is not your strongest suit, route taste-critical surfaces (UI, copy, API design) to a high-taste child or flag them to the user instead of powering through.";

const DEFAULT_GPT_MEDIUM_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 9, intelligence 8, taste 6. The default profile for everything dispatched: bulk or mechanical implementation against a written brief, closed-form debugging with a repro, migrations, data analysis, reviews, sweeps, computer use and eyes-on-screen verification, and token-heavy log or codebase crawls. Extremely steerable and disciplined: respects scope fences, does not weaken tests, reports accurately. Measured equal to its higher efforts on spec-driven work — escalate only after this profile demonstrably misses on a specific piece and the gap looks like depth, not a bad brief.";
const DEFAULT_GPT_MAX_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 6, intelligence 9, taste 7. Exception tier — near the top judgment model's raw problem-solving at a fraction of the token cost, with a rottweiler temperament: grabs the problem by the throat and doesn't let go. Route it hard, well-defined problems that reward tenacity or depth: gnarly bugs with a repro, long autonomous grinds, brute-force search of a solution space, open-ended polish passes. Two measured caveats: wall-clock latency is 5–6x the medium profile, so keep it off any pipeline's critical path; and on closed-form bug fixes it produces the same fix as medium at 1.5–3x the cost. Taste 7 clears the bar for internal tools and dashboards; keep brand- or copy-critical surfaces on a taste-8+ model.";
const DEFAULT_SONNET_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 5, intelligence 5, taste 7. Cheap glue — wrappers, chores, and context gathering that does not require top-tier judgment.";
const DEFAULT_OPUS_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 4, intelligence 7, taste 8. First choice for user-facing work: UI, copy, API design, and anything where taste matters more than grinding depth. Also a strong independent reviewer of plans and implementations.";
const DEFAULT_FABLE_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 2, intelligence 9, taste 9. Highest-judgment escalation for framing, architecture, ambiguous tradeoffs, taste-critical surfaces, and final review. The scarcest resource in the fleet: dispatch to it only when nothing cheaper is adequate.";

/// Settings for tcode's built-in orchestration layer.
///
/// There is deliberately no main-model allow list. Every model may orchestrate;
/// only its identity text changes through the generic fallback and optional
/// per-model overrides. Child models, by contrast, are an explicit allow list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrateSettings {
    #[serde(default = "default_orchestrator_identity")]
    pub generic_identity: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_identities: Vec<OrchestratorIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_models: Vec<OrchestrateChildModel>,
}

fn default_orchestrator_identity() -> String {
    DEFAULT_ORCHESTRATOR_IDENTITY.to_string()
}

impl Default for OrchestrateSettings {
    fn default() -> Self {
        Self {
            generic_identity: default_orchestrator_identity(),
            model_identities: vec![
                OrchestratorIdentity {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-fable-5".into(),
                    identity: DEFAULT_FABLE_IDENTITY.into(),
                },
                OrchestratorIdentity {
                    provider: ProviderKind::Codex,
                    model: "gpt-5.6-sol".into(),
                    identity: DEFAULT_SOL_IDENTITY.into(),
                },
            ],
            child_models: vec![
                OrchestrateChildModel {
                    provider: ProviderKind::Codex,
                    model: "gpt-5.6-sol".into(),
                    profile_id: None,
                    enabled: true,
                    effort: Some("medium".into()),
                    description: DEFAULT_GPT_MEDIUM_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::Codex,
                    model: "gpt-5.6-sol".into(),
                    profile_id: None,
                    enabled: true,
                    effort: Some("max".into()),
                    description: DEFAULT_GPT_MAX_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-sonnet-5".into(),
                    profile_id: None,
                    enabled: true,
                    effort: Some("high".into()),
                    description: DEFAULT_SONNET_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-opus-4-8".into(),
                    profile_id: None,
                    enabled: true,
                    effort: Some("high".into()),
                    description: DEFAULT_OPUS_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-fable-5".into(),
                    profile_id: None,
                    enabled: true,
                    effort: Some("high".into()),
                    description: DEFAULT_FABLE_CHILD_DEFINITION.into(),
                },
            ],
        }
    }
}

impl OrchestrateSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Resolve a model's dedicated identity, falling back to the generic one.
    /// `None` (the provider's default model) also uses the generic identity.
    pub fn identity_for(&self, provider: ProviderKind, model: Option<&str>) -> &str {
        model
            .and_then(|model| {
                self.model_identities
                    .iter()
                    .find(|entry| entry.provider == provider && entry.model == model)
            })
            .map(|entry| entry.identity.as_str())
            .unwrap_or(&self.generic_identity)
    }

    pub fn builtin_generic_identity() -> &'static str {
        DEFAULT_ORCHESTRATOR_IDENTITY
    }

    /// Factory text for a dedicated identity editor. Models without a bundled
    /// specialization reset to the factory generic identity.
    pub fn builtin_identity_for(provider: ProviderKind, model: &str) -> &'static str {
        match (provider, model) {
            (ProviderKind::ClaudeCode, "claude-fable-5") => DEFAULT_FABLE_IDENTITY,
            (ProviderKind::Codex, "gpt-5.6-sol") => DEFAULT_SOL_IDENTITY,
            _ => DEFAULT_ORCHESTRATOR_IDENTITY,
        }
    }

    /// Factory definition for a bundled child-model preset. Custom models have
    /// no product-authored definition and therefore reset to an empty editor.
    pub fn builtin_child_definition(
        provider: ProviderKind,
        model: &str,
        effort: Option<&str>,
    ) -> Option<&'static str> {
        match (provider, model) {
            (ProviderKind::Codex, "gpt-5.6-sol")
                if effort.is_some_and(|effort| effort.eq_ignore_ascii_case("max")) =>
            {
                Some(DEFAULT_GPT_MAX_CHILD_DEFINITION)
            }
            (ProviderKind::Codex, "gpt-5.6-sol") => Some(DEFAULT_GPT_MEDIUM_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-sonnet-5") => Some(DEFAULT_SONNET_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-opus-4-8") => Some(DEFAULT_OPUS_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-fable-5") => Some(DEFAULT_FABLE_CHILD_DEFINITION),
            _ => None,
        }
    }

    pub fn child_profile(
        &self,
        provider: ProviderKind,
        model: &str,
        effort: Option<&str>,
    ) -> Option<&OrchestrateChildModel> {
        self.child_models.iter().find(|entry| {
            entry.provider == provider && entry.model == model && entry.matches_effort(effort)
        })
    }

    pub fn enabled_child_profile(
        &self,
        provider: ProviderKind,
        model: &str,
        effort: Option<&str>,
    ) -> Option<&OrchestrateChildModel> {
        self.child_models.iter().find(|entry| {
            entry.enabled
                && entry.provider == provider
                && entry.model == model
                && entry.matches_effort(effort)
        })
    }

    pub fn enabled_child_profiles(
        &self,
        provider: ProviderKind,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> impl Iterator<Item = &OrchestrateChildModel> {
        self.child_models.iter().filter(move |entry| {
            entry.enabled
                && entry.provider == provider
                && model.is_none_or(|model| entry.model == model)
                && !entry.model.trim().is_empty()
                && entry.matches_effort(effort)
        })
    }
}

/// Provider and model used for the isolated, background request that names a
/// newly-started thread. Reasoning effort is intentionally fixed to `low` by
/// the runtime: title generation is a small, latency-sensitive task.
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

// `Eq` is intentionally absent: `acp_agents` holds `AcpLaunch`, which the
// agent crate derives only `PartialEq` for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// None follows the operating-system language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-provider cards (Settings → Providers), keyed by [`provider_key`].
    /// These are the two *built-in* profiles (Claude Code, Codex).
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
    /// Whether the on-launch provider version check is DISABLED. Stored inverted
    /// so the feature defaults to on (s3 §6: "Provider update checks", default
    /// on) even for legacy settings files that lack the field.
    #[serde(default)]
    pub provider_update_checks_disabled: bool,
    /// Built-in orchestration identities and child-model routing table.
    #[serde(default, skip_serializing_if = "OrchestrateSettings::is_default")]
    pub orchestrate: OrchestrateSettings,
    /// Provider/model used to generate a concise title for new threads.
    #[serde(default, skip_serializing_if = "TitleGenerationSettings::is_default")]
    pub title_generation: TitleGenerationSettings,
    /// Ids of project groups the user has collapsed in the sidebar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_projects: Vec<String>,
    /// Model ids the user has starred in the model picker (favorites float to
    /// the top and are shown first under the star filter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_models: Vec<String>,
    /// Sidebar PROJECTS ordering (cycled by the sort button).
    #[serde(default)]
    pub project_sort: ProjectSort,
    /// Per-session last-visited time (unix secs), keyed by session id. A session
    /// whose `updated_at` exceeds its last-visited time (and isn't active) shows
    /// an unread dot. Opening a thread refreshes it; "Mark unread" clears it.
    /// UI state; absent in legacy files (Group A).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_visited: HashMap<String, u64>,
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

    /// The provider's card title: trimmed display-name override, else its label.
    pub fn provider_display_name(&self, provider: ProviderKind) -> String {
        let settings = self.provider(provider);
        settings
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| provider_label(provider).to_string())
    }

    /// The built-in profile id for a native protocol (its [`provider_key`]).
    /// This is the id a session carries when it uses the default, non-custom
    /// configuration for its kind.
    pub fn builtin_profile_id(kind: ProviderKind) -> &'static str {
        provider_key(kind)
    }

    /// Whether `id` names a built-in profile (`"claude"` / `"codex"` / `"acp"`).
    pub fn is_builtin_profile_id(id: &str) -> bool {
        matches!(id, "claude" | "codex" | "acp")
    }

    /// The protocol kind of a built-in profile id, if it is one. Used to route a
    /// mutation of a built-in profile back to its `providers` card.
    pub fn builtin_kind_from_id(id: &str) -> Option<ProviderKind> {
        match id {
            "claude" => Some(ProviderKind::ClaudeCode),
            "codex" => Some(ProviderKind::Codex),
            "acp" => Some(ProviderKind::Acp),
            _ => None,
        }
    }

    /// Resolve a profile id to its protocol kind and effective card settings.
    /// Built-in ids resolve to the matching `providers` card; anything else to
    /// a user-created `profiles` entry. `None` for an unknown id.
    pub fn resolved_profile(&self, id: &str) -> Option<ResolvedProfile> {
        for kind in [
            ProviderKind::Codex,
            ProviderKind::ClaudeCode,
            ProviderKind::Acp,
        ] {
            if provider_key(kind) == id {
                return Some(ResolvedProfile {
                    id: id.to_string(),
                    kind,
                    settings: self.provider(kind),
                });
            }
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

    /// One installed ACP agent, by registry id.
    pub fn acp_agent(&self, id: &str) -> Option<&InstalledAcpAgent> {
        self.acp_agents.get(id)
    }

    /// Every installed ACP agent, in registry-id order (the marketplace and the
    /// provider rail both render them in this order).
    pub fn installed_acp_agents(&self) -> Vec<&InstalledAcpAgent> {
        self.acp_agents.values().collect()
    }

    /// The ACP agents offered when starting a thread: installed *and* enabled.
    pub fn enabled_acp_agents(&self) -> Vec<&InstalledAcpAgent> {
        self.acp_agents
            .values()
            .filter(|agent| agent.enabled)
            .collect()
    }

    /// Fold the pre-`providers` binary overrides into the map (once, on load).
    pub fn migrate_legacy(&mut self) {
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
    fn project_sort_defaults_and_cycles() {
        // Legacy files (field absent) default to recent-activity ordering.
        assert_eq!(ProjectSort::default(), ProjectSort::RecentActivity);
        // The button cycles RecentActivity → NameAsc → RecentActivity.
        assert_eq!(ProjectSort::RecentActivity.next(), ProjectSort::NameAsc);
        assert_eq!(ProjectSort::NameAsc.next(), ProjectSort::RecentActivity);
    }
    #[test]
    fn display_name_falls_back_to_driver_label() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.provider_display_name(ProviderKind::ClaudeCode),
            "Claude"
        );
        assert_eq!(settings.provider_display_name(ProviderKind::Codex), "Codex");
        // A blank override is treated as "no override".
        settings.provider_mut(ProviderKind::Codex).display_name = Some("   ".into());
        assert_eq!(settings.provider_display_name(ProviderKind::Codex), "Codex");
        settings.provider_mut(ProviderKind::Codex).display_name = Some("Work".into());
        assert_eq!(settings.provider_display_name(ProviderKind::Codex), "Work");
    }

    #[test]
    fn launch_arguments_split_on_whitespace() {
        let settings = ProviderSettings {
            launch_args: Some("  --chrome  --verbose ".into()),
            ..ProviderSettings::default()
        };
        assert_eq!(settings.extra_args(), vec!["--chrome", "--verbose"]);
        assert!(ProviderSettings::default().extra_args().is_empty());
    }

    #[test]
    fn shadow_home_wins_over_home() {
        let settings = ProviderSettings {
            home_path: Some(PathBuf::from("/a")),
            shadow_home_path: Some(PathBuf::from("/b")),
            ..ProviderSettings::default()
        };
        assert_eq!(settings.effective_home(), Some(PathBuf::from("/b")));
    }

    #[test]
    fn orchestrator_identity_uses_model_override_then_generic_fallback() {
        let settings = OrchestrateSettings::default();
        assert!(
            settings
                .identity_for(ProviderKind::ClaudeCode, Some("claude-fable-5"))
                .contains("wise owl")
        );
        assert!(
            settings
                .identity_for(ProviderKind::Codex, Some("gpt-5.6-sol"))
                .contains("rottweiler"),
            "gpt-5.6-sol is the other model bundled with a dedicated lead identity"
        );
        assert_eq!(
            settings.identity_for(ProviderKind::ClaudeCode, Some("claude-opus-4-8")),
            settings.generic_identity
        );
        assert_eq!(
            settings.identity_for(ProviderKind::Codex, Some("claude-fable-5")),
            settings.generic_identity,
            "the provider is part of a model's identity key"
        );
        assert_eq!(
            settings.identity_for(ProviderKind::Acp, None),
            settings.generic_identity,
            "provider-default and ACP models remain eligible through the fallback"
        );
    }

    #[test]
    fn orchestrate_defaults_round_trip_and_legacy_files_get_defaults() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(legacy.orchestrate, OrchestrateSettings::default());
        assert_eq!(legacy.orchestrate.child_models.len(), 5);
        assert!(
            legacy
                .orchestrate
                .child_models
                .iter()
                .all(|entry| entry.enabled)
        );
        let medium = legacy
            .orchestrate
            .enabled_child_profile(ProviderKind::Codex, "gpt-5.6-sol", Some("medium"))
            .unwrap();
        let max = legacy
            .orchestrate
            .enabled_child_profile(ProviderKind::Codex, "gpt-5.6-sol", Some("max"))
            .unwrap();
        assert_ne!(medium.description, max.description);

        let mut settings = Settings::default();
        settings.orchestrate.generic_identity = "Custom lead identity".into();
        settings.orchestrate.model_identities.clear();
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.orchestrate, settings.orchestrate);
    }

    #[test]
    fn orchestrate_child_legacy_default_effort_alias_parses() {
        let settings: OrchestrateSettings = serde_json::from_str(
            r#"{"child_models":[{"provider":"codex","model":"m","default_effort":"high"}]}"#,
        )
        .unwrap();
        assert_eq!(settings.child_models[0].effort.as_deref(), Some("high"));
    }

    #[test]
    fn orchestrate_child_effort_matching_is_exact_case_insensitive() {
        let profile = OrchestrateChildModel {
            provider: ProviderKind::Codex,
            model: "m".into(),
            profile_id: None,
            enabled: true,
            effort: Some("High".into()),
            description: String::new(),
        };
        assert!(profile.matches_effort(Some("high")));
        assert!(!profile.matches_effort(Some("medium")));
        assert!(profile.matches_effort(None));

        let provider_default = OrchestrateChildModel {
            effort: None,
            ..profile
        };
        assert!(!provider_default.matches_effort(Some("high")));
    }

    #[test]
    fn title_generation_defaults_and_round_trips() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(
            legacy.title_generation,
            TitleGenerationSettings {
                provider: ProviderKind::Codex,
                model: "gpt-5.6-luna".into(),
                profile_id: None,
            }
        );
        let partial: TitleGenerationSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(partial, TitleGenerationSettings::default());

        let settings = Settings {
            title_generation: TitleGenerationSettings {
                provider: ProviderKind::ClaudeCode,
                model: "claude-haiku-4-5".into(),
                profile_id: Some("work-claude".into()),
            },
            ..Default::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title_generation, settings.title_generation);
    }

    #[test]
    fn provider_profile_fields_are_backward_compatible_and_round_trip() {
        let child: OrchestrateChildModel =
            serde_json::from_str(r#"{"provider":"codex","model":"m","enabled":true}"#).unwrap();
        assert_eq!(child.profile_id, None);

        let title: TitleGenerationSettings =
            serde_json::from_str(r#"{"provider":"codex","model":"m"}"#).unwrap();
        assert_eq!(title.profile_id, None);

        let child = OrchestrateChildModel {
            profile_id: Some("kimi".into()),
            ..child
        };
        let child_back: OrchestrateChildModel =
            serde_json::from_str(&serde_json::to_string(&child).unwrap()).unwrap();
        assert_eq!(child_back, child);

        let title = TitleGenerationSettings {
            profile_id: Some("kimi".into()),
            ..title
        };
        let title_back: TitleGenerationSettings =
            serde_json::from_str(&serde_json::to_string(&title).unwrap()).unwrap();
        assert_eq!(title_back, title);
    }

    #[test]
    fn sidebar_collapsed_round_trips_and_defaults_to_expanded() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert!(!legacy.sidebar_collapsed, "legacy files must open expanded");

        let settings = Settings {
            sidebar_collapsed: true,
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back.sidebar_collapsed);
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
        // Codex still resolves to exactly its built-in.
        assert_eq!(settings.profiles_for_kind(ProviderKind::Codex).len(), 1);

        // Display names: built-in falls back to label; user shows its name.
        assert_eq!(settings.profile_display_name("claude"), "Claude");
        assert_eq!(settings.profile_display_name(&id), "Klaude Kode");
        assert_eq!(settings.resolved_profile("nope"), None);

        // A second profile of the same name gets a distinct id.
        assert_eq!(settings.allocate_profile_id("Klaude Kode"), "klaude-kode-2");
    }

    #[test]
    fn profiles_round_trip_through_json() {
        let mut settings = Settings::default();
        let id = settings.allocate_profile_id("Kimi");
        settings.profiles.insert(
            id.clone(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    display_name: Some("Kimi".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.profiles, settings.profiles);
        // Legacy files with no `profiles` key still parse (defaults to empty).
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert!(legacy.profiles.is_empty());
    }

    /// A build that predates a field must not destroy it: unknown keys survive a
    /// load → save round trip. (We hit this for real: an older binary dropped
    /// `acp_agents` and the next save wiped the installed agents.)
    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let json = r#"{
            "theme_mode": "dark",
            "a_future_field": {"nested": [1, 2, 3]},
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
    }
}
