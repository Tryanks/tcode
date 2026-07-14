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

/// A model-specific identity override for an orchestrator. Models without an
/// entry here remain fully eligible for `/orchestrate`; they inherit
/// [`OrchestrateSettings::generic_identity`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorIdentity {
    pub provider: ProviderKind,
    pub model: String,
    pub identity: String,
}

/// One model the orchestrator may dispatch work to. Profiles stay in this list
/// while paused so their editable routing definition is preserved; `enabled`
/// is the actual allow-list decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrateChildModel {
    pub provider: ProviderKind,
    pub model: String,
    /// Disabled profiles retain all routing preferences but cannot receive a
    /// dispatch and are omitted from the lead model's available-fleet table.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Effort used when a dispatch omits one. Callers may still explicitly pick
    /// another effort supported by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

const DEFAULT_ORCHESTRATOR_IDENTITY: &str = "You are the primary decision model and technical lead for this session. Your leverage is judgment: understand the problem, frame it well, decompose it, define done, route work to the cheapest adequate child model, and verify the result independently. Keep architecture, ambiguous tradeoffs, and final acceptance for yourself; delegate execution when a child can complete it from a precise brief.";

const DEFAULT_FABLE_IDENTITY: &str = "You are Fable 5, the scarcest judgment resource in this fleet: a wise owl—thoughtful, discerning, and exceptionally strong at framing, architecture, taste, and clear communication. Spend that judgment on understanding, delegation, review, and final acceptance rather than routine typing. Use high effort by default; deeper tiers usually consume more of the fleet's bottleneck without improving your decisions.";

const DEFAULT_GPT_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 9, intelligence 8, taste 6. Recommended effort: medium. Default execution model for bulk implementation, closed-form debugging, reviews, migrations, computer use, and token-heavy sweeps. Escalate effort only after a concrete miss.";
const DEFAULT_SONNET_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 5, intelligence 5, taste 7. Recommended effort: high. Cheap glue work, wrappers, chores, and context gathering that does not require top-tier judgment.";
const DEFAULT_OPUS_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 4, intelligence 7, taste 8. Recommended effort: high. Best for taste-critical UI, copy, API design, and implementation review where judgment matters more than grinding depth.";
const DEFAULT_FABLE_CHILD_DEFINITION: &str = "Ratings (1–10, higher is better): cost efficiency 2, intelligence 9, taste 9. Recommended effort: high. Highest-judgment escalation for framing, architecture, ambiguous tradeoffs, and final review.";

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
            model_identities: vec![OrchestratorIdentity {
                provider: ProviderKind::ClaudeCode,
                model: "claude-fable-5".into(),
                identity: DEFAULT_FABLE_IDENTITY.into(),
            }],
            child_models: vec![
                OrchestrateChildModel {
                    provider: ProviderKind::Codex,
                    model: "gpt-5.6-sol".into(),
                    enabled: true,
                    default_effort: Some("medium".into()),
                    description: DEFAULT_GPT_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-sonnet-5".into(),
                    enabled: false,
                    default_effort: Some("high".into()),
                    description: DEFAULT_SONNET_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-opus-4-8".into(),
                    enabled: false,
                    default_effort: Some("high".into()),
                    description: DEFAULT_OPUS_CHILD_DEFINITION.into(),
                },
                OrchestrateChildModel {
                    provider: ProviderKind::ClaudeCode,
                    model: "claude-fable-5".into(),
                    enabled: false,
                    default_effort: Some("high".into()),
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
            _ => DEFAULT_ORCHESTRATOR_IDENTITY,
        }
    }

    /// Factory definition for a bundled child-model preset. Custom models have
    /// no product-authored definition and therefore reset to an empty editor.
    pub fn builtin_child_definition(provider: ProviderKind, model: &str) -> Option<&'static str> {
        match (provider, model) {
            (ProviderKind::Codex, "gpt-5.6-sol") => Some(DEFAULT_GPT_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-sonnet-5") => Some(DEFAULT_SONNET_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-opus-4-8") => Some(DEFAULT_OPUS_CHILD_DEFINITION),
            (ProviderKind::ClaudeCode, "claude-fable-5") => Some(DEFAULT_FABLE_CHILD_DEFINITION),
            _ => None,
        }
    }

    pub fn child_model(
        &self,
        provider: ProviderKind,
        model: &str,
    ) -> Option<&OrchestrateChildModel> {
        self.child_models
            .iter()
            .find(|entry| entry.provider == provider && entry.model == model)
    }

    pub fn enabled_child_model(
        &self,
        provider: ProviderKind,
        model: &str,
    ) -> Option<&OrchestrateChildModel> {
        self.child_model(provider, model)
            .filter(|entry| entry.enabled)
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderSettings>,
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
        assert!(
            legacy
                .orchestrate
                .enabled_child_model(ProviderKind::Codex, "gpt-5.6-sol")
                .is_some()
        );
        assert!(
            legacy
                .orchestrate
                .child_model(ProviderKind::ClaudeCode, "claude-opus-4-8")
                .is_some(),
            "disabled presets retain their preferences"
        );
        assert!(
            legacy
                .orchestrate
                .enabled_child_model(ProviderKind::ClaudeCode, "claude-opus-4-8")
                .is_none(),
            "the default dispatch fleet is GPT-only"
        );

        let mut settings = Settings::default();
        settings.orchestrate.generic_identity = "Custom lead identity".into();
        settings.orchestrate.model_identities.clear();
        let json = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.orchestrate, settings.orchestrate);
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
