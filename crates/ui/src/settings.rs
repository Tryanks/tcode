//! Persisted application settings presentation with core-owned semantics.

pub use crate::{LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE};

#[cfg(test)]
static TEST_LOCALE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestLocaleGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestLocaleGuard {
    pub(crate) fn acquire() -> Self {
        let lock = TEST_LOCALE_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        Self { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for TestLocaleGuard {
    fn drop(&mut self) {
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}

/// Resolve the persisted override against the current system preference and
/// update the process-global locale shared by tcode-ui and gpui-component.
pub fn apply_locale(override_locale: Option<&str>) {
    let locale = crate::apply_locale(override_locale);
    gpui_component::set_locale(locale);
}

pub use tcode_core::settings::{
    BrowserSettings, ChildApprovalMode, EnvVar, ImageMode, OrchestrateChildModel,
    OrchestrateSettings, OrchestratorIdentity, ProjectSort, ProviderSettings, Settings, ThemeMode,
    provider_label,
};
/// The six accent presets offered by the provider card (T3 §2).
pub const ACCENT_PRESETS: [&str; 6] = [
    "#2563eb", "#16a34a", "#ea580c", "#dc2626", "#7c3aed", "#0891b2",
];

/// A localized human label for the sidebar sort button's tooltip.
pub fn project_sort_label(sort: ProjectSort) -> String {
    match sort {
        ProjectSort::RecentActivity => crate::tr!("sidebar.sort_recent").into_owned(),
        ProjectSort::NameAsc => crate::tr!("sidebar.sort_name").into_owned(),
    }
}
