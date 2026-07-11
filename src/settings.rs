//! Persisted application settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use agent::ProviderKind;

pub const LANGUAGE_ENGLISH: &str = "en";
pub const LANGUAGE_SIMPLIFIED_CHINESE: &str = "zh-CN";

/// Resolve the persisted override against the current system preference and
/// update rust-i18n's process-global locale (shared by gpui-component).
pub fn apply_locale(override_locale: Option<&str>) {
    let locale = match override_locale {
        Some(LANGUAGE_ENGLISH) => LANGUAGE_ENGLISH,
        Some(LANGUAGE_SIMPLIFIED_CHINESE) => LANGUAGE_SIMPLIFIED_CHINESE,
        _ => {
            if sys_locale::get_locale()
                .as_deref()
                .is_some_and(|locale| locale.to_ascii_lowercase().starts_with("zh"))
            {
                LANGUAGE_SIMPLIFIED_CHINESE
            } else {
                LANGUAGE_ENGLISH
            }
        }
    };
    rust_i18n::set_locale(locale);
    gpui_component::set_locale(locale);
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

impl ProjectSort {
    /// The next mode in the cycle (RecentActivity → NameAsc → RecentActivity).
    pub fn next(self) -> Self {
        match self {
            ProjectSort::RecentActivity => ProjectSort::NameAsc,
            ProjectSort::NameAsc => ProjectSort::RecentActivity,
        }
    }

    /// A human label for the sort button's tooltip.
    pub fn label(self) -> String {
        match self {
            ProjectSort::RecentActivity => rust_i18n::t!("sidebar.sort_recent").into_owned(),
            ProjectSort::NameAsc => rust_i18n::t!("sidebar.sort_name").into_owned(),
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

/// The six accent presets offered by the provider card (T3 §2).
pub const ACCENT_PRESETS: [&str; 6] = [
    "#2563eb", "#16a34a", "#ea580c", "#dc2626", "#7c3aed", "#0891b2",
];

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
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub last_visited: std::collections::HashMap<String, u64>,
    /// ACP agents the user installed from the marketplace (or defined by hand),
    /// keyed by registry id. Each carries its resolved launch recipe, so a
    /// session can start without consulting the registry again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub acp_agents: BTreeMap<String, crate::acp_registry::InstalledAgent>,
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
    pub fn acp_agent(&self, id: &str) -> Option<&crate::acp_registry::InstalledAgent> {
        self.acp_agents.get(id)
    }

    /// Every installed ACP agent, in registry-id order (the marketplace and the
    /// provider rail both render them in this order).
    pub fn installed_acp_agents(&self) -> Vec<&crate::acp_registry::InstalledAgent> {
        self.acp_agents.values().collect()
    }

    /// The ACP agents offered when starting a thread: installed *and* enabled.
    pub fn enabled_acp_agents(&self) -> Vec<&crate::acp_registry::InstalledAgent> {
        self.acp_agents
            .values()
            .filter(|agent| agent.enabled)
            .collect()
    }

    /// Fold the pre-`providers` binary overrides into the map (once, on load).
    fn migrate_legacy(&mut self) {
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

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    secrets_path: PathBuf,
}

impl SettingsStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            path: data_dir.join("settings.json"),
            secrets_path: data_dir.join("secrets.json"),
        }
    }

    pub fn load(&self) -> Settings {
        let Ok(bytes) = fs::read(&self.path) else {
            return Settings::default();
        };
        match serde_json::from_slice::<Settings>(&bytes) {
            Ok(mut settings) => {
                settings.migrate_legacy();
                settings
            }
            Err(err) => {
                log::warn!("failed to parse settings.json: {err}");
                Settings::default()
            }
        }
    }

    // -- sensitive env values (secrets.json, 0600) --------------------------

    /// Every stored secret, keyed by provider key then variable name.
    pub fn load_secrets(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        fs::read(&self.secrets_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The sensitive env values for one provider (used only when spawning).
    pub fn provider_secrets(&self, provider: ProviderKind) -> BTreeMap<String, String> {
        self.load_secrets()
            .remove(provider_key(provider))
            .unwrap_or_default()
    }

    /// Store (`Some`) or clear (`None`) one provider secret. Written 0600 and
    /// never returned to the settings UI.
    pub fn set_secret(
        &self,
        provider: ProviderKind,
        name: &str,
        value: Option<&str>,
    ) -> std::io::Result<()> {
        let mut all = self.load_secrets();
        let entry = all.entry(provider_key(provider).to_string()).or_default();
        match value {
            Some(value) => {
                entry.insert(name.to_string(), value.to_string());
            }
            None => {
                entry.remove(name);
            }
        }
        if entry.is_empty() {
            all.remove(provider_key(provider));
        }
        self.write_secrets(&all)
    }

    fn write_secrets(
        &self,
        all: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(all)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.secrets_path.with_extension("json.tmp");
        fs::write(&tmp, data)?;
        restrict_permissions(&tmp)?;
        fs::rename(&tmp, &self.secrets_path)?;
        restrict_permissions(&self.secrets_path)
    }

    pub fn save(&self, settings: &Settings) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(tmp, &self.path)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        let mut providers = BTreeMap::new();
        providers.insert(
            "codex".to_string(),
            ProviderSettings {
                enabled: false,
                display_name: Some("Work Codex".into()),
                accent_color: Some("#2563eb".into()),
                env: vec![
                    EnvVar {
                        name: "BASE_URL".into(),
                        value: "https://example.test".into(),
                        sensitive: false,
                    },
                    EnvVar {
                        name: "OPENAI_API_KEY".into(),
                        value: String::new(),
                        sensitive: true,
                    },
                ],
                binary_path: Some(PathBuf::from("/opt/tools/codex")),
                home_path: Some(PathBuf::from("/tmp/codex-home")),
                shadow_home_path: Some(PathBuf::from("/tmp/codex-shadow")),
                launch_args: None,
                custom_models: vec!["gpt-6.7-codex".into()],
                hidden_models: vec!["gpt-5".into()],
                model_order: vec!["gpt-6".into(), "gpt-5".into()],
            },
        );
        providers.insert(
            "claude".to_string(),
            ProviderSettings {
                binary_path: Some(PathBuf::from("/opt/tools/claude")),
                launch_args: Some("--chrome".into()),
                ..ProviderSettings::default()
            },
        );
        let expected = Settings {
            unknown: serde_json::Map::new(),
            language: Some(LANGUAGE_SIMPLIFIED_CHINESE.into()),
            providers,
            codex_binary: None,
            claude_binary: None,
            theme_mode: ThemeMode::Dark,
            sidebar_collapsed: true,
            word_wrap_diffs: true,
            skip_delete_confirmation: true,
            auto_open_task_panel: true,
            provider_update_checks_disabled: true,
            collapsed_projects: vec!["proj-a".into(), "proj-b".into()],
            favorite_models: vec!["opus".into()],
            project_sort: ProjectSort::NameAsc,
            last_visited: std::collections::HashMap::from([("sess-a".to_string(), 42)]),
            acp_agents: BTreeMap::new(),
        };

        store.save(&expected).unwrap();

        assert_eq!(store.load(), expected);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_sort_defaults_and_cycles() {
        // Legacy files (field absent) default to recent-activity ordering.
        assert_eq!(ProjectSort::default(), ProjectSort::RecentActivity);
        // The button cycles RecentActivity → NameAsc → RecentActivity.
        assert_eq!(ProjectSort::RecentActivity.next(), ProjectSort::NameAsc);
        assert_eq!(ProjectSort::NameAsc.next(), ProjectSort::RecentActivity);
    }

    #[test]
    fn project_sort_persists() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-sort-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        let settings = Settings {
            project_sort: ProjectSort::NameAsc,
            ..Settings::default()
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load().project_sort, ProjectSort::NameAsc);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loads_legacy_file_and_migrates_binary_paths() {
        // A settings.json written before the `providers` map existed must still
        // parse: its flat binary overrides migrate into the per-provider card,
        // and the newer fields default off / enabled.
        let root =
            std::env::temp_dir().join(format!("tcode-settings-legacy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        fs::write(
            &store.path,
            r#"{"claude_binary":"/usr/bin/claude","theme_mode":"light","favorite_models":["opus"]}"#,
        )
        .unwrap();

        let loaded = store.load();
        assert_eq!(
            loaded.provider(ProviderKind::ClaudeCode).binary_path,
            Some(PathBuf::from("/usr/bin/claude"))
        );
        // The legacy keys are consumed, not echoed back.
        assert_eq!(loaded.claude_binary, None);
        assert_eq!(loaded.theme_mode, ThemeMode::Light);
        assert_eq!(loaded.favorite_models, vec!["opus".to_string()]);
        // Never-configured providers default to enabled with no overrides.
        let codex = loaded.provider(ProviderKind::Codex);
        assert!(codex.enabled);
        assert_eq!(codex.binary_path, None);
        // New fields tolerantly default to off.
        assert!(!loaded.word_wrap_diffs);
        assert!(!loaded.skip_delete_confirmation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sensitive_values_live_in_secrets_json_only() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-secret-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());

        let mut settings = Settings::default();
        settings.provider_mut(ProviderKind::ClaudeCode).env = vec![EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            // Sensitive rows carry no value in settings.json.
            value: String::new(),
            sensitive: true,
        }];
        store.save(&settings).unwrap();
        store
            .set_secret(
                ProviderKind::ClaudeCode,
                "ANTHROPIC_API_KEY",
                Some("sk-live"),
            )
            .unwrap();

        // settings.json never contains the secret; the reloaded row keeps its
        // name + sensitive flag and an empty value (nothing to echo back).
        let raw = fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains("sk-live"));
        let loaded = store.load();
        let env = loaded.provider(ProviderKind::ClaudeCode).env;
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "ANTHROPIC_API_KEY");
        assert!(env[0].sensitive);
        assert!(env[0].value.is_empty());

        // The value is only reachable through the secrets store, which is 0600.
        let secrets = store.provider_secrets(ProviderKind::ClaudeCode);
        assert_eq!(
            secrets.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-live")
        );
        assert!(store.provider_secrets(ProviderKind::Codex).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&store.secrets_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Clearing removes the entry (and the now-empty provider bucket).
        store
            .set_secret(ProviderKind::ClaudeCode, "ANTHROPIC_API_KEY", None)
            .unwrap();
        assert!(store.provider_secrets(ProviderKind::ClaudeCode).is_empty());
        let _ = fs::remove_dir_all(root);
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
}

#[cfg(test)]
mod sidebar_collapse_tests {
    use super::*;

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
}

#[cfg(test)]
mod unknown_field_tests {
    use super::*;

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
