//! Persisted application settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[cfg(test)]
use agent::ProviderKind;
#[cfg(test)]
use tcode_core::settings::{EnvVar, ThemeMode};
use tcode_core::{pull_request::HostKind, settings::Settings};

type Secrets = BTreeMap<String, BTreeMap<String, String>>;

/// Saved host tokens by kind, then by host. Profile ids are built-in ids or
/// slugs of ASCII alphanumerics and hyphens, so these keys never name a
/// profile, and a build that predates one keeps the entry as an unknown
/// profile's secrets.
fn token_secrets(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Github => "@github",
        // One API and one token per server, whichever of the two it runs.
        HostKind::Forgejo | HostKind::Gitea => "@forgejo",
        HostKind::Gitlab => "@gitlab",
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

    /// A file of the host's own beside settings.json.
    pub(crate) fn data_file(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    /// Fails on an unreadable file so a write never replaces secrets it could not read.
    fn read_secrets(&self) -> std::io::Result<Secrets> {
        match fs::read(&self.secrets_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Secrets::new()),
            Err(error) => Err(error),
        }
    }

    /// Every stored provider secret, keyed by profile id then variable name.
    pub fn load_secrets(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut all = self.read_secrets().unwrap_or_default();
        for kind in HostKind::ALL {
            all.remove(token_secrets(kind));
        }
        all
    }

    /// The sensitive env values for one profile id: a built-in profile's
    /// [`Settings::builtin_profile_id`] or a user profile's slug.
    pub fn profile_secrets(&self, profile_id: &str) -> BTreeMap<String, String> {
        self.load_secrets().remove(profile_id).unwrap_or_default()
    }

    pub(crate) fn token(&self, kind: HostKind, host: &str) -> Option<String> {
        self.read_secrets()
            .ok()?
            .remove(token_secrets(kind))?
            .remove(host)
    }

    pub fn set_token(
        &self,
        kind: HostKind,
        host: &str,
        token: Option<&str>,
    ) -> std::io::Result<()> {
        let host = kind
            .authority(host)
            .map_err(|refusal| std::io::Error::other(format!("{refusal:?}")))?;
        let mut all = self.read_secrets()?;
        let secrets = token_secrets(kind);
        let tokens = all.entry(secrets.to_owned()).or_default();
        match token.map(str::trim).filter(|token| !token.is_empty()) {
            Some(token) => {
                tokens.insert(host, token.to_owned());
            }
            None => {
                tokens.remove(&host);
            }
        }
        if tokens.is_empty() {
            all.remove(secrets);
        }
        self.write_secrets(&all)
    }

    /// Store (`Some`) or clear (`None`) one profile secret, by profile id.
    pub fn set_profile_secret(
        &self,
        profile_id: &str,
        name: &str,
        value: Option<&str>,
    ) -> std::io::Result<()> {
        let mut all = self.read_secrets()?;
        let entry = all.entry(profile_id.to_string()).or_default();
        match value {
            Some(value) => {
                entry.insert(name.to_string(), value.to_string());
            }
            None => {
                entry.remove(name);
            }
        }
        if entry.is_empty() {
            all.remove(profile_id);
        }
        self.write_secrets(&all)
    }

    /// Drop every secret stored for a profile id (used when deleting a profile).
    pub fn clear_profile_secrets(&self, profile_id: &str) -> std::io::Result<()> {
        let mut all = self.read_secrets()?;
        if all.remove(profile_id).is_some() {
            return self.write_secrets(&all);
        }
        Ok(())
    }

    fn write_secrets(&self, all: &Secrets) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(all)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.secrets_path.with_extension("json.tmp");
        use std::io::Write as _;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        restrict_permissions(&tmp)?;
        file.write_all(&data)?;
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
            r#"{"claude_binary":"/usr/bin/claude","theme_mode":"light","favorite_models":["opus"],"remote_hosting_enabled":true,"remote_port":47421}"#,
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
        // The HTTP listener's port is gone; hosting continues and the next
        // save no longer carries the port.
        assert!(loaded.remote_hosting_enabled);
        store.save(&loaded).unwrap();
        assert!(
            !fs::read_to_string(&store.path)
                .unwrap()
                .contains("remote_port")
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Hosts saved before they had kinds are GitHub hosts, with their switch and account.
    #[test]
    fn github_hosts_from_before_kinds_load_as_github_source_control_hosts() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-hosts-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        fs::write(
            &store.path,
            r#"{"github":{"hosts":{"github.com":{"enabled":false,"account":"octocat"},"github.example.com":{}}}}"#,
        )
        .unwrap();

        let loaded = store.load();
        let hosts = &loaded.source_control.hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["github.com"].kind, HostKind::Github);
        assert!(!hosts["github.com"].enabled);
        assert_eq!(hosts["github.com"].account.as_deref(), Some("octocat"));
        assert_eq!(hosts["github.example.com"].kind, HostKind::Github);
        assert!(hosts["github.example.com"].enabled);
        store.save(&loaded).unwrap();
        let saved = fs::read_to_string(&store.path).unwrap();
        assert!(!saved.contains("\"github\":"), "{saved}");
        assert_eq!(store.load().source_control.hosts, *hosts);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_persist_privately_and_clear_only_the_selected_profile() {
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
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
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
        let secrets = store.profile_secrets("claude");
        assert_eq!(
            secrets.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-live")
        );
        assert!(store.profile_secrets("codex").is_empty());
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
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
                "ANTHROPIC_API_KEY",
                None,
            )
            .unwrap();
        assert!(store.profile_secrets("claude").is_empty());
        // A user profile "klaude-kode" stores its own key, isolated from the
        // built-in "claude" profile.
        store
            .set_profile_secret("klaude-kode", "ANTHROPIC_API_KEY", Some("sk-kimi-xyz"))
            .unwrap();
        store
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
                "ANTHROPIC_API_KEY",
                Some("sk-official"),
            )
            .unwrap();

        assert_eq!(
            store
                .profile_secrets("klaude-kode")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-kimi-xyz")
        );
        // The built-in Claude profile keeps its `claude` bucket.
        assert_eq!(
            store
                .profile_secrets("claude")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-official")
        );

        // Deleting a profile drops only its bucket.
        store.clear_profile_secrets("klaude-kode").unwrap();
        assert!(store.profile_secrets("klaude-kode").is_empty());
        assert_eq!(
            store
                .profile_secrets("claude")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-official")
        );
        let _ = fs::remove_dir_all(root);
    }
}
