//! Persisted application settings.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    pub fn label(self) -> &'static str {
        match self {
            ProjectSort::RecentActivity => "Recent activity",
            ProjectSort::NameAsc => "Name A-Z",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
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
            codex_binary: Some(PathBuf::from("/opt/tools/codex")),
            claude_binary: Some(PathBuf::from("/opt/tools/claude")),
            theme_mode: ThemeMode::Dark,
            word_wrap_diffs: true,
            skip_delete_confirmation: true,
            collapsed_projects: vec!["proj-a".into(), "proj-b".into()],
            favorite_models: vec!["opus".into()],
            project_sort: ProjectSort::NameAsc,
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
        let root = std::env::temp_dir()
            .join(format!("tcode-settings-sort-{}", uuid::Uuid::new_v4()));
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
        let root = std::env::temp_dir()
            .join(format!("tcode-settings-legacy-{}", uuid::Uuid::new_v4()));
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
