//! Persisted application settings.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// None follows the operating-system language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_binary: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_binary: Option<PathBuf>,
    #[serde(default)]
    pub theme_mode: ThemeMode,
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
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            path: data_dir.join("settings.json"),
        }
    }

    pub fn load(&self) -> Settings {
        let Ok(bytes) = fs::read(&self.path) else {
            return Settings::default();
        };
        match serde_json::from_slice(&bytes) {
            Ok(settings) => settings,
            Err(err) => {
                log::warn!("failed to parse settings.json: {err}");
                Settings::default()
            }
        }
    }

    pub fn save(&self, settings: &Settings) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(tmp, &self.path)
    }
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
        let expected = Settings {
            language: Some(LANGUAGE_SIMPLIFIED_CHINESE.into()),
            codex_binary: Some(PathBuf::from("/opt/tools/codex")),
            claude_binary: Some(PathBuf::from("/opt/tools/claude")),
            theme_mode: ThemeMode::Dark,
            word_wrap_diffs: true,
            skip_delete_confirmation: true,
            auto_open_task_panel: true,
            collapsed_projects: vec!["proj-a".into(), "proj-b".into()],
            favorite_models: vec!["opus".into()],
            project_sort: ProjectSort::NameAsc,
            last_visited: std::collections::HashMap::from([("sess-a".to_string(), 42)]),
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
    fn loads_legacy_file_without_new_fields() {
        // A settings.json written before the word-wrap / delete-confirmation
        // fields existed must still parse, with the new fields defaulting off.
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
        assert_eq!(loaded.claude_binary, Some(PathBuf::from("/usr/bin/claude")));
        assert_eq!(loaded.theme_mode, ThemeMode::Light);
        assert_eq!(loaded.favorite_models, vec!["opus".to_string()]);
        // New fields tolerantly default to off.
        assert!(!loaded.word_wrap_diffs);
        assert!(!loaded.skip_delete_confirmation);
        let _ = fs::remove_dir_all(root);
    }
}
