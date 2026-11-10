//! This device's own preferences. One owner above every host's store reads
//! them from the client's [`ClientHost`] and writes each change through it;
//! no store keeps a copy.

use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, EventEmitter, Global, Window};
use tcode_client::host::{ClientHost, ClientPreferences, FileFailure, WorkspacePreference};
use tcode_core::settings::{Settings, SidebarLayout, ThemeMode};

use crate::store::HostKey;

pub(crate) struct DevicePreferences {
    client: Option<Rc<dyn ClientHost>>,
    saved: ClientPreferences,
    /// Why what the client saved could not be read. This device then runs on
    /// the defaults, changes them for as long as it runs, and saves nothing
    /// over the file.
    unreadable: Option<FileFailure>,
}

/// What the window tells the user about this device's preferences.
pub(crate) enum PreferencesFailure {
    Unreadable(FileFailure),
    Unsaved(FileFailure),
}

impl EventEmitter<PreferencesFailure> for DevicePreferences {}

struct Installed(Entity<DevicePreferences>);

impl Global for Installed {}

impl DevicePreferences {
    /// Make what `client` saved the preferences every view reads.
    pub(crate) fn install(client: Rc<dyn ClientHost>, cx: &mut App) {
        let loaded = client.load_preferences();
        Self::entity(cx).update(cx, |this, cx| {
            this.client = Some(client);
            match loaded {
                Ok(saved) => {
                    this.saved = saved;
                    this.unreadable = None;
                }
                Err(error) => {
                    log::error!("running on default device preferences: {error}");
                    this.saved = ClientPreferences::default();
                    this.unreadable = Some(error.clone());
                    cx.emit(PreferencesFailure::Unreadable(error));
                }
            }
            cx.notify();
        });
    }

    /// The owner, holding nothing saved until a shell installs its client.
    pub(crate) fn entity(cx: &mut App) -> Entity<Self> {
        if let Some(installed) = cx.try_global::<Installed>() {
            return installed.0.clone();
        }
        let entity = cx.new(|_| Self {
            client: None,
            saved: ClientPreferences::default(),
            unreadable: None,
        });
        cx.set_global(Installed(entity.clone()));
        entity
    }

    fn saved(cx: &App) -> Option<&ClientPreferences> {
        Some(&cx.try_global::<Installed>()?.0.read(cx).saved)
    }

    fn update(cx: &mut App, change: impl FnOnce(&mut ClientPreferences)) {
        Self::entity(cx).update(cx, |this, cx| {
            let mut preferences = this.saved.clone();
            change(&mut preferences);
            if preferences == this.saved {
                return;
            }
            if this.unreadable.is_none()
                && let Some(client) = &this.client
                && let Err(error) = client.save_preferences(&preferences)
            {
                log::error!("could not save device preferences: {error}");
                cx.emit(PreferencesFailure::Unsaved(error));
            }
            this.saved = preferences;
            cx.notify();
        });
    }

    /// A preference this device has never set takes `host`'s, once: the
    /// host kept these four before they were the device's. Returns whether
    /// any did.
    pub(crate) fn adopt(host: &Settings, cx: &mut App) -> bool {
        let unset = Self::saved(cx).is_none_or(|saved| {
            saved.appearance.is_none()
                || saved.language.is_none()
                || saved.sidebar_collapsed.is_none()
                || saved.sidebar_layout.is_none()
        });
        if unset {
            Self::update(cx, |saved| {
                saved.appearance.get_or_insert(host.theme_mode);
                saved
                    .language
                    .get_or_insert_with(|| host.language.clone().unwrap_or("system".into()));
                saved
                    .sidebar_collapsed
                    .get_or_insert(host.sidebar_collapsed);
                saved.sidebar_layout.get_or_insert(host.sidebar_layout);
            });
        }
        unset
    }

    /// The theme this device chose, `None` before it chose one.
    pub(crate) fn appearance(cx: &App) -> Option<ThemeMode> {
        Self::saved(cx)?.appearance
    }

    pub(crate) fn theme(cx: &App) -> ThemeMode {
        Self::appearance(cx).unwrap_or_default()
    }

    pub(crate) fn set_theme(mode: ThemeMode, window: &mut Window, cx: &mut App) {
        Self::update(cx, |saved| saved.appearance = Some(mode));
        crate::settings_page::apply_theme(mode, window, cx);
    }

    /// The language this device asks for; `None` is the system's.
    pub(crate) fn language(cx: &App) -> Option<String> {
        Self::saved(cx).and_then(language).map(str::to_owned)
    }

    pub(crate) fn set_language(language: Option<&str>, window: &mut Window, cx: &mut App) {
        Self::update(cx, |saved| {
            saved.language = Some(language.unwrap_or("system").to_owned())
        });
        crate::settings::apply_locale(language);
        window.refresh();
    }

    pub(crate) fn sidebar_collapsed(cx: &App) -> bool {
        Self::saved(cx)
            .and_then(|saved| saved.sidebar_collapsed)
            .unwrap_or(false)
    }

    pub(crate) fn toggle_sidebar_collapsed(cx: &mut App) {
        let collapsed = !Self::sidebar_collapsed(cx);
        Self::update(cx, |saved| saved.sidebar_collapsed = Some(collapsed));
    }

    pub(crate) fn sidebar_layout(cx: &App) -> SidebarLayout {
        Self::saved(cx)
            .and_then(|saved| saved.sidebar_layout)
            .unwrap_or_default()
    }

    pub(crate) fn set_sidebar_layout(layout: SidebarLayout, cx: &mut App) {
        Self::update(cx, |saved| saved.sidebar_layout = Some(layout));
    }

    /// The workspace as this device arranged it, `None` before it did.
    pub(crate) fn workspace(cx: &App) -> Option<WorkspacePreference> {
        Self::saved(cx)?.workspace.clone()
    }

    pub(crate) fn arrange_workspace(
        cx: &mut App,
        change: impl FnOnce(&mut Option<WorkspacePreference>),
    ) {
        Self::update(cx, |saved| change(&mut saved.workspace));
    }

    /// Whether this device folded `host`'s section away.
    pub(crate) fn host_collapsed(host: &HostKey, cx: &App) -> bool {
        Self::saved(cx)
            .and_then(|saved| saved.workspace.as_ref())
            .is_some_and(|workspace| {
                workspace
                    .hosts
                    .iter()
                    .any(|listed| listed.host_id == host.name() && listed.collapsed)
            })
    }

    pub(crate) fn set_host_collapsed(host: &HostKey, collapsed: bool, cx: &mut App) {
        Self::arrange_workspace(cx, |workspace| {
            crate::workspace::set_collapsed(
                workspace.get_or_insert_with(Default::default),
                host,
                collapsed,
            )
        });
    }

    /// The name this device shows the machines it connects to.
    pub(crate) fn device_name(cx: &App) -> String {
        let this = cx
            .try_global::<Installed>()
            .map(|installed| installed.0.read(cx));
        this.and_then(|this| this.saved.device_name.clone())
            .or_else(|| Some(this?.client.as_ref()?.device_name()))
            .unwrap_or_else(|| crate::tr!("app.name").into_owned())
    }

    pub(crate) fn device_name_overridden(cx: &App) -> bool {
        Self::saved(cx).is_some_and(|saved| saved.device_name.is_some())
    }

    pub(crate) fn set_device_name(name: Option<String>, cx: &mut App) {
        Self::update(cx, |saved| saved.device_name = name);
    }

    /// The per-image ceiling, in bytes, for attachments this device sends
    /// across the internet.
    pub(crate) fn remote_attachment_limit_bytes(cx: &App) -> u64 {
        Self::remote_attachment_limit_mib(cx)
            .map_or(tcode_core::attachments::DEFAULT_REMOTE_BYTES, |mib| {
                u64::from(mib) * 1024 * 1024
            })
    }

    pub(crate) fn remote_attachment_limit_mib(cx: &App) -> Option<u32> {
        Self::saved(cx)?.remote_attachment_limit_mib
    }

    pub(crate) fn set_remote_attachment_limit_mib(limit: Option<u32>, cx: &mut App) {
        Self::update(cx, |saved| saved.remote_attachment_limit_mib = limit);
    }

    /// The width the user dragged a right panel to, in points.
    pub(crate) fn right_panel_width(cx: &App) -> Option<u32> {
        Self::saved(cx)?.right_panel_width
    }

    pub(crate) fn set_right_panel_width(width: u32, cx: &mut App) {
        if Self::right_panel_width(cx) != Some(width) {
            Self::update(cx, |saved| saved.right_panel_width = Some(width));
        }
    }

    /// Whether this device folded the stack map's note about GitHub stacks away.
    pub(crate) fn stack_note_collapsed(cx: &App) -> bool {
        Self::saved(cx)
            .and_then(|saved| saved.navigation.as_ref())
            .and_then(|navigation| navigation["stack_note_collapsed"].as_bool())
            .unwrap_or(false)
    }

    pub(crate) fn set_stack_note_collapsed(collapsed: bool, cx: &mut App) {
        Self::update(cx, |saved| {
            saved
                .navigation
                .get_or_insert_with(|| serde_json::json!({}))["stack_note_collapsed"] =
                serde_json::json!(collapsed);
        });
    }

    /// Whether this device asked not to see the note before Open in Zed
    /// reaches a machine over Tcode again.
    pub(crate) fn remote_zed_note_dismissed(cx: &App) -> bool {
        Self::saved(cx)
            .and_then(|saved| saved.navigation.as_ref())
            .and_then(|navigation| navigation["remote_zed_note_dismissed"].as_bool())
            .unwrap_or(false)
    }

    pub(crate) fn dismiss_remote_zed_note(cx: &mut App) {
        Self::update(cx, |saved| {
            saved
                .navigation
                .get_or_insert_with(|| serde_json::json!({}))["remote_zed_note_dismissed"] =
                serde_json::json!(true);
        });
    }

    /// The settings this device keeps as a member of the space `key` names;
    /// the space's store owns their schema.
    pub(crate) fn member_settings(key: &str, cx: &App) -> Option<serde_json::Value> {
        Self::saved(cx)?
            .navigation
            .as_ref()?
            .get("member_settings")?
            .get(key)
            .cloned()
    }

    pub(crate) fn set_member_settings(key: &str, settings: serde_json::Value, cx: &mut App) {
        Self::update(cx, |saved| {
            saved
                .navigation
                .get_or_insert_with(|| serde_json::json!({}))["member_settings"][key] = settings;
        });
    }

    /// The window's navigation as last saved; the shell owns its schema.
    pub(crate) fn navigation(cx: &App) -> Option<serde_json::Value> {
        Self::saved(cx)?.navigation.clone()
    }

    /// Save the window's navigation, keeping what others keep beside it.
    pub(crate) fn save_navigation(snapshot: serde_json::Value, cx: &mut App) {
        Self::update(cx, |saved| {
            let mut navigation = snapshot;
            for kept in [
                "member_settings",
                "stack_note_collapsed",
                "remote_zed_note_dismissed",
            ] {
                if let Some(value) = saved
                    .navigation
                    .as_ref()
                    .and_then(|navigation| navigation.get(kept))
                {
                    navigation[kept] = value.clone();
                }
            }
            saved.navigation = Some(navigation);
        });
    }

    /// Restore this device's theme, language, name, attachment limit and
    /// panel width to their defaults; its sidebar and workspace are kept.
    pub(crate) fn restore_defaults(window: &mut Window, cx: &mut App) {
        Self::update(cx, |saved| {
            *saved = ClientPreferences {
                appearance: Some(ThemeMode::System),
                language: Some("system".into()),
                navigation: saved.navigation.take(),
                sidebar_collapsed: saved.sidebar_collapsed,
                sidebar_layout: saved.sidebar_layout.take(),
                workspace: saved.workspace.take(),
                ..ClientPreferences::default()
            };
        });
        crate::settings::apply_locale(None);
        crate::settings_page::apply_theme(ThemeMode::System, window, cx);
        window.refresh();
    }
}

/// The language `preferences` ask for; `None` is the system's.
pub(crate) fn language(preferences: &ClientPreferences) -> Option<&str> {
    preferences
        .language
        .as_deref()
        .filter(|language| *language != "system")
}
