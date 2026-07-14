//! Persisted application settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use agent::ProviderKind;
#[cfg(test)]
use tcode_core::settings::{EnvVar, ProjectSort, ProviderSettings, ThemeMode};
use tcode_core::settings::{Settings, provider_key};

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
            language: Some("zh-CN".into()),
            providers,
            codex_binary: None,
            claude_binary: None,
            theme_mode: ThemeMode::Dark,
            sidebar_collapsed: true,
            word_wrap_diffs: true,
            skip_delete_confirmation: true,
            auto_open_task_panel: true,
            provider_update_checks_disabled: true,
            orchestrate: Default::default(),
            title_generation: Default::default(),
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
}
