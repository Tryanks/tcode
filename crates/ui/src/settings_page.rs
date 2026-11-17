//! Full-page settings route with section navigation and editable settings.

mod archived;
mod browser;
mod computer_use;
mod control;
mod disk_usage;
mod general;
mod nav;
mod providers;
pub(crate) mod thread_behavior;
mod usage;

use std::rc::Rc;

use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::tabs::{TabBar, TabItem};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, v_flex};

use crate::acp_panel::AcpPanel;
use crate::device::DevicePreferences;
use crate::harness_dialog::{HarnessDialog, HarnessId, ManagePlugins};
use crate::plugins_settings::PluginsSettingsPanel;
use crate::provider_model_picker::ProviderModelPicker;
use crate::settings::{
    ImageMode, LANGUAGE_ENGLISH, LANGUAGE_JAPANESE, LANGUAGE_SIMPLIFIED_CHINESE, ThemeMode,
};
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::theme::{self, ActiveTheme as _, ThemeMode as UiThemeMode};
use crate::time::humanize_ago;
use crate::typography::TextRole as _;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::WindowState;
use tcode_core::project::SessionMeta;
use tcode_core::settings::TitleGenerationSettings;

use crate::settings_rows::{SelectRowOption, ToggleRow};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

const NAV_WIDTH: f32 = 255.;
/// Archived rows rendered per page. Each row carries two buttons with click
/// listeners, so the whole list is unaffordable once thousands are archived.
const ARCHIVED_PER_PAGE: usize = 50;

/// Clamp `page` so its first row still exists and return `(page, start, end)`
/// for the rows it covers. `total == 0` yields the empty range on page 0.
fn page_bounds(total: usize, page: usize, per_page: usize) -> (usize, usize, usize) {
    let last_page = total.saturating_sub(1) / per_page.max(1);
    let page = page.min(last_page);
    let start = page * per_page;
    (page, start, start.saturating_add(per_page).min(total))
}

/// The Advanced disclosure's button, its chevron turning with the fold.
#[derive(IntoElement)]
struct AdvancedToggle {
    expanded: bool,
    on_click: crate::widgets::button::ClickHandler,
}

impl gpui::RenderOnce for AdvancedToggle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let on_click = self.on_click;
        Button::new("settings-advanced-toggle")
            .text()
            .xsmall()
            .text_caption(cx)
            .icon(crate::widgets::Chevron::icon(
                "settings-advanced-toggle",
                self.expanded,
                window,
                cx,
            ))
            .label(crate::tr!("settings.advanced"))
            .aria_expanded(self.expanded)
            .debug_selector(|| "settings-advanced-toggle".into())
            .on_click(move |event, window, cx| on_click(event, window, cx))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Threads,
    ProjectThreadRules,
    Providers,
    /// One harness's settings, on a compact window.
    Harness,
    SourceControl,
    /// One platform of Git Platforms, managed on a phone's pushed page.
    SourceControlHost,
    Usage,
    Browser,
    ComputerUse,
    Control,
    Plugins,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Remote,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Spaces,
    /// On a desktop, the shared spaces of the other machine this page
    /// shows; [`Self::Spaces`] are this device's own.
    #[cfg(feature = "remote-hosting")]
    MachineSpaces,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Group,
    #[cfg(feature = "remote-hosting")]
    Traverse,
    Archived,
    Maintenance,
}

/// Navigation order, shared by the desktop rail and the compact section list.
/// Device-local settings come first; replicated settings for the attached
/// machine follow. Remote, Shared spaces, Machine group and Traverse configure
/// the local desktop listener, or the serving headless listener in a browser;
/// Traverse, the least used, comes last. A desktop over another machine also
/// manages that machine's shared spaces, first of its settings. Choosing a
/// machine lives in `crate::remote`.
#[cfg(feature = "remote-hosting")]
const SECTIONS: [Section; 16] = [
    Section::General,
    Section::Remote,
    Section::Spaces,
    Section::Group,
    Section::Traverse,
    Section::MachineSpaces,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];
#[cfg(all(target_family = "wasm", not(feature = "remote-hosting")))]
const SECTIONS: [Section; 14] = [
    Section::General,
    Section::Remote,
    Section::Spaces,
    Section::Group,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];
#[cfg(not(any(feature = "remote-hosting", target_family = "wasm")))]
const SECTIONS: [Section; 11] = [
    Section::General,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionGroup {
    Device,
    Machine,
}

/// The group of the remote sections, which configure the listener of the
/// machine this client runs on: a desktop's own, under This device. A browser
/// has none, so they configure the machine serving it.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
fn remote_group() -> SectionGroup {
    if cfg!(target_family = "wasm") {
        SectionGroup::Machine
    } else {
        SectionGroup::Device
    }
}

/// The remote sections' views: the machine's hosting switches, invitation
/// and devices; its shared spaces; its group; and on a desktop, its Traverse
/// sources.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
struct RemotePanels {
    remote: gpui::AnyView,
    spaces: Entity<crate::remote::SpacesSection>,
    /// The page's machine's spaces, where it is another machine.
    #[cfg(feature = "remote-hosting")]
    machine_spaces: Entity<crate::remote::SpacesSection>,
    group: Entity<crate::remote::GroupSection>,
    /// Where this process hosts; the section is not offered elsewhere.
    #[cfg(feature = "remote-hosting")]
    traverse: Option<Entity<crate::remote::TraversePanel>>,
}

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
impl RemotePanels {
    /// A desktop manages its own listener; a browser, the machine serving it,
    /// with that machine's pairing switch on the Remote page.
    fn new(store: &Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) -> Self {
        #[cfg(feature = "remote-hosting")]
        let (remote, resolve): (gpui::AnyView, crate::remote::spaces::Resolve) = (
            cx.new(|cx| crate::remote::HostingPanel::new(store.clone(), window, cx))
                .into(),
            crate::remote::spaces::this_machine,
        );
        #[cfg(not(feature = "remote-hosting"))]
        let (remote, resolve): (gpui::AnyView, crate::remote::spaces::Resolve) = {
            let _ = window;
            let resolve: crate::remote::spaces::Resolve = crate::remote::spaces::for_store;
            (
                cx.new(|_| crate::remote::AccessSection::new(store.clone(), resolve, true))
                    .into(),
                resolve,
            )
        };
        Self {
            remote,
            spaces: cx.new(|_| crate::remote::SpacesSection::new(store.clone(), resolve)),
            #[cfg(feature = "remote-hosting")]
            machine_spaces: cx.new(|_| {
                crate::remote::SpacesSection::new(store.clone(), crate::remote::spaces::for_store)
            }),
            group: cx.new(|_| crate::remote::GroupSection::new(store.clone(), resolve)),
            #[cfg(feature = "remote-hosting")]
            traverse: cx
                .has_global::<crate::remote::RemoteController>()
                .then(|| cx.new(|cx| crate::remote::TraversePanel::new(window, cx))),
        }
    }
}

/// Capabilities relevant to Settings applicability. The OS-specific questions
/// are answered by the capability owners; Settings only consumes their result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettingsCapabilities {
    preview_backend: bool,
    hosting: bool,
    local_permissions: bool,
    remote_attachment: bool,
}

impl SettingsCapabilities {
    #[cfg_attr(not(feature = "remote-hosting"), allow(unused_variables))]
    fn current(store: &WorkspaceStore, cx: &App) -> Self {
        // A desktop whose own host did not start has no listener to configure.
        #[cfg(feature = "remote-hosting")]
        let hosting = cx.has_global::<crate::remote::RemoteController>();
        #[cfg(not(feature = "remote-hosting"))]
        let hosting = cfg!(target_family = "wasm");
        Self {
            preview_backend: crate::preview_panel::preview_backend(),
            hosting,
            local_permissions: cfg!(all(feature = "local-permissions", target_os = "macos")),
            remote_attachment: store.is_remote(),
        }
    }

    fn can_manage_local_permissions(self) -> bool {
        self.local_permissions && !self.remote_attachment
    }

    /// Whether the advanced subgroup starts collapsed. Computer Use and Browser
    /// configure a *screen* — the one the agent drives. A client attached to
    /// another machine, with no computer use of its own, has no screen in the
    /// picture, so those sections fold out of the way instead of leading the
    /// machine's list. The local desktop drives its own screen: expanded.
    fn folds_advanced(self) -> bool {
        self.remote_attachment && !self.local_permissions
    }
}

impl Section {
    fn group(self) -> SectionGroup {
        match self {
            Self::General => SectionGroup::Device,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote | Self::Spaces | Self::Group => remote_group(),
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => remote_group(),
            #[cfg(feature = "remote-hosting")]
            Self::MachineSpaces => SectionGroup::Machine,
            Self::Threads
            | Self::ProjectThreadRules
            | Self::SourceControl
            | Self::SourceControlHost
            | Self::Providers
            | Self::Harness
            | Self::Usage
            | Self::Browser
            | Self::ComputerUse
            | Self::Control
            | Self::Plugins
            | Self::Archived
            | Self::Maintenance => SectionGroup::Machine,
        }
    }

    /// Sections that configure the screen and browser the agent drives, and
    /// the machine's upkeep, which share the machine group's collapsible
    /// **Advanced** subgroup.
    fn advanced(self) -> bool {
        matches!(self, Self::ComputerUse | Self::Browser | Self::Maintenance)
    }

    /// A section is navigable when at least one row can do real work for this
    /// client and attachment. Replicated host settings remain applicable over
    /// remote links; the Browser section has only embedded-preview rows.
    fn applies(&self, cx: &SettingsCapabilities) -> bool {
        match self {
            Self::Browser => cx.preview_backend,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote | Self::Spaces | Self::Group => cx.hosting,
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => cx.hosting,
            #[cfg(feature = "remote-hosting")]
            Self::MachineSpaces => cx.remote_attachment,
            Self::Threads
            | Self::ProjectThreadRules
            | Self::General
            | Self::SourceControl
            | Self::SourceControlHost
            | Self::Providers
            | Self::Harness
            | Self::Usage
            | Self::ComputerUse
            | Self::Control
            | Self::Plugins
            | Self::Archived
            | Self::Maintenance => true,
        }
    }

    /// Whether this client may open the section: it [`applies`](Self::applies),
    /// and a shared-space member (`full_scope == false`) manages only this
    /// device's settings and the space's archived threads.
    fn available(self, cx: &SettingsCapabilities, full_scope: bool) -> bool {
        self.applies(cx) && (full_scope || matches!(self, Self::General | Self::Archived))
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "settings-nav-general",
            Self::Threads => "settings-nav-threads",
            Self::ProjectThreadRules => "settings-nav-project-rules",
            Self::SourceControl => "settings-nav-source-control",
            Self::SourceControlHost => "settings-nav-source-control-host",
            Self::Providers => "settings-nav-providers",
            Self::Harness => "settings-nav-harness",
            Self::Usage => "settings-nav-usage",
            Self::Browser => "settings-nav-browser",
            Self::ComputerUse => "settings-nav-computer-use",
            Self::Control => "settings-nav-control",
            Self::Plugins => "settings-nav-plugins",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => "settings-nav-remote",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => "settings-nav-spaces",
            #[cfg(feature = "remote-hosting")]
            Self::MachineSpaces => "settings-nav-machine-spaces",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => "settings-nav-group",
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => "settings-nav-traverse",
            Self::Archived => "settings-nav-archived",
            Self::Maintenance => "settings-nav-maintenance",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Threads | Self::ProjectThreadRules => IconName::MessageSquare,
            Self::SourceControl | Self::SourceControlHost => IconName::Github,
            Self::Providers | Self::Harness => IconName::Bot,
            Self::Usage => IconName::ChartPie,
            Self::Browser => IconName::Globe,
            Self::ComputerUse => IconName::LayoutDashboard,
            Self::Control => IconName::Layers,
            Self::Plugins => IconName::Puzzle,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => IconName::Network,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => IconName::Users,
            #[cfg(feature = "remote-hosting")]
            Self::MachineSpaces => IconName::Users,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => IconName::HardDrive,
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => IconName::Link,
            Self::Archived => IconName::Inbox,
            Self::Maintenance => IconName::Settings2,
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::General => crate::tr!("settings.general"),
            Self::Threads | Self::ProjectThreadRules => crate::tr!("settings.threads"),
            Self::SourceControl | Self::SourceControlHost => {
                crate::tr!("settings.source_control")
            }
            Self::Providers | Self::Harness => crate::tr!("settings.providers"),
            Self::Usage => crate::tr!("settings.usage"),
            Self::Browser => crate::tr!("settings.browser"),
            Self::ComputerUse => crate::tr!("settings.computer_use"),
            Self::Control => crate::tr!("control.title"),
            Self::Plugins => crate::tr!("settings.plugins"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => crate::tr!("settings.remote"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => crate::tr!("settings.spaces"),
            #[cfg(feature = "remote-hosting")]
            Self::MachineSpaces => crate::tr!("settings.spaces"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => crate::tr!("settings.group"),
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => crate::tr!("settings.traverse"),
            Self::Archived => crate::tr!("settings.archived"),
            Self::Maintenance => crate::tr!("settings.maintenance"),
        }
        .into_owned()
        .into()
    }
}

/// The remote attachment ceiling as the settings field shows it, in MiB.
fn remote_attachment_limit_value(cx: &App) -> String {
    (DevicePreferences::remote_attachment_limit_bytes(cx) / (1024 * 1024)).to_string()
}

/// An input seeded from replicated settings.
///
/// `pushed` is the last value this page wrote into the field, so an echoed
/// change event can be told apart from a real edit; `dirty` then freezes the
/// field against later snapshots so a host update cannot overwrite what the
/// user is in the middle of typing.
struct SettingsInput {
    state: Entity<InputState>,
    pushed: String,
    dirty: bool,
}

impl SettingsInput {
    fn new(state: Entity<InputState>) -> Self {
        Self {
            state,
            pushed: String::new(),
            dirty: false,
        }
    }

    /// `true` when the change came from the user and should be committed.
    fn is_user_edit(&mut self, value: &str) -> bool {
        if value == self.pushed {
            return false;
        }
        self.dirty = true;
        true
    }

    fn follow(&mut self, value: String, window: &mut Window, cx: &mut App) {
        if crate::settings_rows::follow_input(&self.state, &value, window, cx) {
            self.pushed = value;
        }
    }

    fn push(&mut self, value: String, window: &mut Window, cx: &mut App) {
        self.pushed = value.clone();
        self.state
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }
}

/// Apply a settings theme mode to the live window (shared with the palette's
/// "Toggle theme" action), its colours gliding to the new palette.
pub(crate) fn apply_theme(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    let mode = match mode {
        ThemeMode::Light => UiThemeMode::Light,
        ThemeMode::Dark => UiThemeMode::Dark,
        ThemeMode::System => window.appearance().into(),
    };
    theme::glide_mode(mode, window, cx);
}

pub struct SettingsPage {
    store: Entity<WorkspaceStore>,
    /// The window's hosts, whose settings the machine group chooses between
    /// when more than one is this device's to manage.
    hosts: Vec<Entity<WorkspaceStore>>,
    window_state: Entity<WindowState>,
    /// The harness shown as a page of the compact settings stack.
    harness_page: Option<Entity<HarnessDialog>>,
    harness_page_scroll: ScrollHandle,
    /// The open harness dialog's or page's requests to this page.
    harness_subscriptions: Vec<Subscription>,
    /// Long-lived state for the modal ACP marketplace and custom form.
    acp_panel: Entity<AcpPanel>,
    /// Editable main-model identities and child-model routing matrix.
    /// Each enabled profile's native plugin catalog.
    plugins_panel: Entity<PluginsSettingsPanel>,
    source_control_panel: Entity<crate::source_control_settings::SourceControlPanel>,
    /// The remote sections. Absent where the client cannot listen at all.
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    remote: RemotePanels,
    /// Shared provider/model picker configured for background thread titles.
    title_model_picker: Entity<ProviderModelPicker>,
    /// Editable name this client presents to machines it connects to.
    device_name_input: SettingsInput,
    section: Section,
    /// Whether this page holds the store's archived threads.
    archived_held: bool,
    /// Whether this page holds what Maintenance shows of the host: its
    /// storage migration and disk usage.
    maintenance_held: bool,
    /// Whether this page shows the harnesses, whose ACP rows say when the
    /// registry has a newer version.
    providers_shown: bool,
    /// The providers whose disk usage is unfolded.
    disk_usage_open: std::collections::HashSet<agent::ProviderKind>,
    capabilities: SettingsCapabilities,
    /// Whether the machine group's **Advanced** subgroup is open, once the user
    /// has said. `None` keeps the capability-derived default.
    advanced_expanded: Option<bool>,
    /// The rail's opacity, below 1 while a route that draws its own left
    /// column rises over Settings or Settings over it.
    rail_opacity: f32,
    /// Latches the one usage refresh fired when Usage becomes the active
    /// section; cleared as soon as the page shows anything else.
    usage_refresh_sent: bool,
    /// Editable "Home URL" for the Browser page; committed on change.
    home_url_input: SettingsInput,
    /// Editable "New project folder"; committed on change.
    new_project_dir_input: SettingsInput,
    /// Editable custom "Add Project start folder"; committed on change.
    project_start_custom_input: SettingsInput,
    /// The host's home, which the Projects rows write `~` paths under. Asked
    /// of the host when those rows are drawn, until it answers.
    host_home: Option<SharedString>,
    /// An ask for the host's home is on its way or has been answered.
    host_home_requested: bool,
    auto_settle_input: SettingsInput,
    project_rules_editor: Option<Entity<thread_behavior::ProjectRulesEditor>>,
    project_rules_subscription: Option<Subscription>,
    control: control::ControlSection,
    /// The page's size-class motion: the window draws the page under
    /// another parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
    remote_attachment_limit_input: SettingsInput,
    /// Zero-based page shown by the Archived Threads list.
    archived_page: usize,
    /// Whether the editable fields have been seeded from the host's settings.
    /// A portable client renders before its first snapshot arrives, and the
    /// local defaults it starts with must never be shown as the host's answer.
    hydrated: bool,
    /// The native permission group. `Some` only where this build can read TCC
    /// *and* the workspace is this machine's.
    #[cfg(all(feature = "local-permissions", target_os = "macos"))]
    local_permissions: Option<Entity<crate::local_permissions::LocalPermissions>>,
    host_permissions: Option<Entity<crate::host_permissions::HostPermissions>>,
    /// The compact section list and the section content page each keep their
    /// reading position across rerenders.
    section_scroll: ScrollHandle,
    content_scroll: ScrollHandle,
    project_rules_scroll: ScrollHandle,
    /// The platform a phone manages on its own page.
    source_control_host: Option<Entity<crate::source_control_settings::HostEditor>>,
    source_control_host_subscription: Option<Subscription>,
    source_control_host_scroll: ScrollHandle,
    _source_control_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    /// The row `id` of this page, gliding when the window turns compact or
    /// wide.
    fn flip_row(&self, id: impl Into<SharedString>, cx: &App) -> crate::settings_rows::FlipRow {
        crate::settings_rows::flip_row(&self.motion, id, self.window_state.read(cx).compact)
    }

    pub(crate) fn set_hosts(&mut self, hosts: Vec<Entity<WorkspaceStore>>, cx: &mut Context<Self>) {
        self.hosts = hosts;
        cx.notify();
    }

    /// The hosts the machine group can show, in the window's order. One that
    /// is down is left out unless the page shows it, or a page over a down
    /// host would lose its way to the others.
    fn settings_hosts(&self, cx: &App) -> Vec<&Entity<WorkspaceStore>> {
        self.hosts
            .iter()
            .filter(|store| {
                let host = store.read(cx);
                host.scope().is_full()
                    && (*store == &self.store || !crate::sidebar::host_down(host))
            })
            .collect()
    }

    fn section_applies(&self, section: &Section, cx: &App) -> bool {
        section.available(&self.capabilities, self.store.read(cx).scope().is_full())
    }
    /// Open the section a link elsewhere in the app names, the way choosing
    /// it here would.
    pub(crate) fn open_section(&mut self, name: &str, cx: &mut Context<Self>) {
        let section = match name {
            "providers" => Section::Providers,
            "usage" => Section::Usage,
            "source_control" => Section::SourceControl,
            "browser" => Section::Browser,
            "computer_use" => Section::ComputerUse,
            "control" => Section::Control,
            "plugins" => Section::Plugins,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            "remote" => Section::Remote,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            "spaces" => Section::Spaces,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            "group" => Section::Group,
            #[cfg(feature = "remote-hosting")]
            "traverse" => Section::Traverse,
            "archived" => Section::Archived,
            "thread_behavior" | "threads" => Section::Threads,
            "maintenance" => Section::Maintenance,
            _ => Section::General,
        };
        self.select_section(section, cx);
    }

    /// Show on `to` the section `from` shows, so choosing another host keeps
    /// the page where it was.
    pub(crate) fn carry_section(from: &Entity<Self>, to: &Entity<Self>, cx: &mut App) {
        let section = match from.read(cx).section {
            Section::ProjectThreadRules => Section::Threads,
            Section::Harness => Section::Providers,
            Section::SourceControlHost => Section::SourceControl,
            section => section,
        };
        to.update(cx, |page, cx| {
            if page.section == section || !page.section_applies(&section, cx) {
                return;
            }
            page.section = section;
            page.sync_holds(cx);
            if section.advanced() {
                page.advanced_expanded = Some(true);
            }
            cx.notify();
        });
    }

    fn return_to_settings_root(window_state: &Entity<WindowState>, cx: &mut Context<Self>) {
        window_state.update(cx, |state, cx| {
            if state.compact
                && state.destination() == crate::window_state::Destination::SettingsSection
            {
                state.back(cx);
            }
        });
    }

    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let capabilities = SettingsCapabilities::current(store.read(cx), cx);
        let title_generation = store.read(cx).settings().title_generation;
        let title_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                store.clone(),
                "title-model-popover",
                "title-model-dropdown",
                title_generation.provider,
                title_generation.model,
                title_generation.profile_id,
                cx,
            )
        });
        let device = DevicePreferences::entity(cx);
        let subscriptions = vec![
            cx.subscribe(&store, |this, _, change: &StoreChange, cx| {
                if change.topic != TopicKind::Settings {
                    return;
                }
                let settings = this.store.read(cx).settings();
                let selection = settings.title_generation;
                this.title_model_picker.update(cx, |picker, cx| {
                    picker.set_selected(
                        selection.provider,
                        selection.model,
                        selection.profile_id,
                        cx,
                    );
                });
                cx.notify();
            }),
            cx.observe(&window_state, |_, _, cx| cx.notify()),
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.hydrate_inputs(window, cx);
            }),
            // Each host's page draws this device's fields: what is set on one,
            // or anywhere else, shows on the others, but for the field being
            // typed in.
            cx.observe_in(&device, window, |this, _, window, cx| {
                let name = DevicePreferences::device_name(cx);
                this.device_name_input.follow(name, window, cx);
                let limit = remote_attachment_limit_value(cx);
                this.remote_attachment_limit_input.follow(limit, window, cx);
            }),
            cx.subscribe(&title_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.dispatch_settings(
                    move |store| {
                        store.set_title_generation_provider(selected.provider);
                        store.set_title_generation_model(selected.id);
                        store.set_title_generation_profile_id(selected.profile_id);
                    },
                    cx,
                );
            }),
        ];

        let acp_panel = cx.new(|cx| AcpPanel::new(store.clone(), window, cx));
        let content_scroll = ScrollHandle::new();
        let plugins_panel = cx.new(|cx| {
            PluginsSettingsPanel::new(
                store.clone(),
                window_state.clone(),
                content_scroll.clone(),
                window,
                cx,
            )
        });
        #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
        let remote = RemotePanels::new(&store, window, cx);
        // Editable fields start empty and are seeded by `hydrate_inputs` once
        // the host's settings actually arrive, so a portable client never shows
        // its local defaults as if they were the host's configuration.
        let home_url_input = cx.new(|cx| InputState::new(window, cx));
        let new_project_dir_input = cx.new(|cx| InputState::new(window, cx));
        let project_start_custom_input = cx.new(|cx| InputState::new(window, cx));
        let device_name = DevicePreferences::device_name(cx);
        let device_name_input =
            cx.new(|cx| InputState::new(window, cx).default_value(device_name.clone()));
        let remote_attachment_limit = remote_attachment_limit_value(cx);
        let remote_attachment_limit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.)
                .min(1.)
                .max(u32::MAX as f64)
                .default_value(remote_attachment_limit.clone())
        });
        #[cfg(all(feature = "local-permissions", target_os = "macos"))]
        let local_permissions = capabilities.can_manage_local_permissions().then(|| {
            let store = store.clone();
            cx.new(|cx| crate::local_permissions::LocalPermissions::new(store, window, cx))
        });
        let auto_settle_input = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.)
                .min(*tcode_core::settings::AUTO_SETTLE_DAYS.start())
                .max(*tcode_core::settings::AUTO_SETTLE_DAYS.end())
        });
        let source_control_panel = cx.new(|cx| {
            crate::source_control_settings::SourceControlPanel::new(
                store.clone(),
                window_state.clone(),
                cx,
            )
        });
        let source_control_subscription = cx.subscribe_in(
            &source_control_panel,
            window,
            |page: &mut Self, _, crate::source_control_settings::ManageHost(host), window, cx| {
                page.manage_source_control_host(host.clone(), window, cx)
            },
        );
        let mut page = Self {
            store,
            hosts: Vec::new(),
            window_state,
            harness_page: None,
            harness_page_scroll: ScrollHandle::new(),
            harness_subscriptions: Vec::new(),
            acp_panel,
            plugins_panel,
            source_control_panel,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            remote,
            title_model_picker,
            device_name_input: SettingsInput {
                state: device_name_input.clone(),
                pushed: device_name,
                dirty: false,
            },
            section: Section::General,
            archived_held: false,
            maintenance_held: false,
            providers_shown: false,
            disk_usage_open: Default::default(),
            capabilities,
            advanced_expanded: None,
            rail_opacity: 1.,
            usage_refresh_sent: false,
            home_url_input: SettingsInput::new(home_url_input.clone()),
            new_project_dir_input: SettingsInput::new(new_project_dir_input.clone()),
            project_start_custom_input: SettingsInput::new(project_start_custom_input.clone()),
            host_home: None,
            host_home_requested: false,
            auto_settle_input: SettingsInput::new(auto_settle_input.clone()),
            project_rules_editor: None,
            project_rules_subscription: None,
            control: control::ControlSection::default(),
            motion: Default::default(),
            remote_attachment_limit_input: SettingsInput {
                state: remote_attachment_limit_input.clone(),
                pushed: remote_attachment_limit,
                dirty: false,
            },
            archived_page: 0,
            hydrated: false,
            #[cfg(all(feature = "local-permissions", target_os = "macos"))]
            local_permissions,
            host_permissions: None,
            section_scroll: ScrollHandle::new(),
            content_scroll,
            project_rules_scroll: ScrollHandle::new(),
            source_control_host: None,
            source_control_host_subscription: None,
            source_control_host_scroll: ScrollHandle::new(),
            _source_control_subscription: source_control_subscription,
            _subscriptions: subscriptions,
        };
        page._subscriptions
            .push(cx.subscribe(&home_url_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.home_url_input.is_user_edit(&value) {
                        this.commit_home_url(cx);
                    }
                }
            }));
        page._subscriptions.push(
            cx.subscribe(&new_project_dir_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.new_project_dir_input.is_user_edit(&value) {
                        let dir = Some(value.trim().to_owned()).filter(|dir| !dir.is_empty());
                        this.dispatch_settings(move |store| store.set_new_project_dir(dir), cx);
                    }
                }
            }),
        );
        page._subscriptions.push(cx.subscribe(
            &project_start_custom_input,
            |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.project_start_custom_input.is_user_edit(&value) {
                        let dir = Some(value.trim().to_owned()).filter(|dir| !dir.is_empty());
                        this.dispatch_settings(
                            move |store| store.set_project_start_custom(dir),
                            cx,
                        );
                    }
                }
            },
        ));
        page._subscriptions
            .push(cx.subscribe(&device_name_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.device_name_input.is_user_edit(&value) {
                        this.commit_device_name(cx);
                    }
                }
            }));
        page._subscriptions.push(cx.subscribe_in(
            &remote_attachment_limit_input,
            window,
            |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.remote_attachment_limit_input
                        .is_user_edit(input.read(cx).value().as_ref());
                } else if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. })
                    && this.remote_attachment_limit_input.dirty
                {
                    this.commit_remote_attachment_limit(window, cx);
                }
            },
        ));

        page._subscriptions.push(cx.subscribe_in(
            &auto_settle_input,
            window,
            |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.auto_settle_input
                        .is_user_edit(input.read(cx).value().as_ref());
                } else if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_auto_settle_days(window, cx);
                }
            },
        ));
        page.hydrate_inputs(window, cx);
        page._subscriptions.push(cx.on_release(|this, cx| {
            this.store.update(cx, |store, _| {
                if this.archived_held {
                    store.release_archived_sessions();
                }
                if this.maintenance_held {
                    store.release_storage_migration();
                    store.release_disk_usage();
                }
            });
        }));
        page.sync_holds(cx);
        page
    }

    /// Seed the editable fields from the host's settings, exactly once, the
    /// first time a real snapshot exists.
    ///
    /// A desktop client already has one at construction; a portable client gets
    /// it milliseconds later. Untouched fields adopt it; a field the user has
    /// already edited keeps that edit, and no later snapshot rewrites either.
    fn hydrate_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hydrated || !self.store.read(cx).settings_hydrated() {
            return;
        }
        self.hydrated = true;
        let settings = self.store.read(cx).settings();
        if !self.auto_settle_input.dirty {
            self.auto_settle_input.push(
                settings
                    .auto_settle_after_days
                    .unwrap_or(tcode_core::settings::AUTO_SETTLE_DEFAULT_DAYS)
                    .to_string(),
                window,
                cx,
            );
        }
        if !self.home_url_input.dirty {
            let value = settings.browser.home_url.clone().unwrap_or_default();
            self.home_url_input.push(value, window, cx);
        }
        if !self.new_project_dir_input.dirty {
            let value = settings.new_project_dir.clone().unwrap_or_default();
            self.new_project_dir_input.push(value, window, cx);
        }
        if !self.project_start_custom_input.dirty {
            let value = settings.project_start_custom.clone().unwrap_or_default();
            self.project_start_custom_input.push(value, window, cx);
        }
    }

    /// Ask the host where its home is, once: listing `~` answers with it. A
    /// failed ask says why, and the rows keep `~`.
    pub(super) fn fetch_host_home(&mut self, cx: &mut Context<Self>) {
        if self.host_home_requested || !self.store.read(cx).can_manage_host() {
            return;
        }
        self.host_home_requested = true;
        let listing = self
            .store
            .update(cx, |store, cx| store.browse_directories("~".into(), cx));
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            let _ = this.update(cx, |page, cx| match listing {
                Ok(listing) => {
                    page.host_home = listing
                        .home
                        .map(|home| home.to_string_lossy().into_owned().into());
                    cx.notify();
                }
                Err(error) => page.store.update(cx, |_, cx| {
                    cx.emit(tcode_protocol::RuntimeNotification::Error(
                        tcode_protocol::RuntimeError::External(
                            crate::tr!("sidebar.folders_failed", reason = error).into_owned(),
                        ),
                    ))
                }),
            });
        })
        .detach();
    }

    fn commit_home_url(&self, cx: &mut Context<Self>) {
        let value = self
            .home_url_input
            .state
            .read(cx)
            .value()
            .trim()
            .to_string();
        let home_url = (!value.is_empty()).then_some(value);
        self.dispatch_settings(move |store| store.set_browser_home_url(home_url), cx);
    }

    fn commit_device_name(&self, cx: &mut Context<Self>) {
        let value = self
            .device_name_input
            .state
            .read(cx)
            .value()
            .trim()
            .to_string();
        let name = (!value.is_empty()).then_some(value);
        DevicePreferences::set_device_name(name, cx);
    }

    /// Take what the field holds once the user leaves it, which the field
    /// has brought within its range. What is not a number of MiB is not
    /// taken, and the field shows the limit in effect again.
    fn commit_remote_attachment_limit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self
            .remote_attachment_limit_input
            .state
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok();
        if let Some(limit) = typed {
            DevicePreferences::set_remote_attachment_limit_mib(Some(limit), cx);
        }
        self.remote_attachment_limit_input.dirty = false;
        let limit = remote_attachment_limit_value(cx);
        self.remote_attachment_limit_input.push(limit, window, cx);
    }

    /// Open a harness's settings: a dialog on a wide window, a page of the
    /// settings stack on a compact one.
    fn open_harness(&mut self, harness: HarnessId, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.window_state.read(cx).compact;
        let store = self.store.clone();
        let dialog = if compact {
            let page = cx.new(|cx| HarnessDialog::new(store, harness, true, window, cx));
            self.harness_page = Some(page.clone());
            self.section = Section::Harness;
            self.window_state.update(cx, |state, cx| {
                state.go(crate::window_state::Destination::SettingsHarness, cx)
            });
            page
        } else {
            crate::harness_dialog::open(store, harness, window, cx)
        };
        self.harness_subscriptions = vec![
            cx.subscribe(&dialog, |this, _, ManagePlugins(profile_id), cx| {
                if this.section == Section::Harness {
                    this.window_state.update(cx, |state, cx| state.back(cx));
                    this.harness_page = None;
                }
                this.select_section(Section::Plugins, cx);
                let profile_id = profile_id.clone();
                this.plugins_panel
                    .update(cx, |panel, cx| panel.focus_profile(profile_id, cx));
            }),
            cx.subscribe(&dialog, |this, _, _: &gpui::DismissEvent, cx| {
                if this.section == Section::Harness {
                    this.window_state.update(cx, |state, cx| state.back(cx));
                    this.section = Section::Providers;
                    this.harness_page = None;
                    cx.notify();
                }
            }),
        ];
        cx.notify();
    }

    fn open_acp_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.acp_panel
            .update(cx, |panel, cx| panel.prepare_to_open(cx));
        let panel = self.acp_panel.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            dialog
                .wide()
                .title(crate::tr!("providers.acp.add_agent").into_owned())
                .content(move |content, _, _| {
                    // The catalog is a GPUI `list`, which needs a definite height, and the
                    // dialog's flex height is not readable from its builder.
                    content
                        .h(px(crate::overlay::DIALOG_LIST_HEIGHT))
                        .child(div().size_full().child(panel.clone()))
                })
        });
    }

    fn dispatch_settings(&self, intent: impl FnOnce(&mut WorkspaceStore), cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            intent(store);
            store.local_settings_changed(cx);
        });
    }

    /// What a section shows only while it is open: the archived threads,
    /// which the index leaves out, and the host's storage migration and
    /// disk usage.
    fn sync_holds(&mut self, cx: &mut Context<Self>) {
        let archived = self.section == Section::Archived;
        if archived != self.archived_held {
            self.archived_held = archived;
            self.store.update(cx, |store, cx| {
                if archived {
                    store.hold_archived_sessions(cx);
                } else {
                    store.release_archived_sessions();
                }
            });
        }
        let maintenance = self.section == Section::Maintenance;
        if maintenance != self.maintenance_held {
            self.maintenance_held = maintenance;
            self.store.update(cx, |store, _| {
                if maintenance {
                    store.hold_storage_migration();
                    store.hold_disk_usage();
                } else {
                    store.release_storage_migration();
                    store.release_disk_usage();
                }
            });
        }
        // Asked for each time the harnesses come into view, and not before:
        // a page is built for every host the window links, shown or not. The
        // host answers from its cache while that is fresh.
        let providers = matches!(self.section, Section::Providers | Section::Harness);
        if providers && !self.providers_shown {
            self.store.update(cx, |store, _| {
                if store.can_manage_host() {
                    store.refresh_acp_registry();
                }
            });
        }
        self.providers_shown = providers;
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Windows: the settings route replaces the workspace, so this header is
        // the window's top-right corner and hosts the caption buttons. The
        // centered column must keep its position (it aligns with the content
        // below), so the cluster is placed out of flow at the strip's right edge
        // and the column reserves matching trailing room for it — its actions
        // therefore end left of the buttons rather than under them.
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Settings,
            self.window_state.read(cx).route(),
            false,
            Default::default(),
        );
        // Align the header title and actions with the centered content column.
        window_drag_area(
            "settings-header-drag",
            crate::settings_rows::content_header(
                crate::material::bar("settings-header", crate::material::Bar::Strip, false, cx),
                cx,
            )
            .when(hosts_caption, |strip| strip.relative()),
            window,
            cx,
        )
        .child(
            crate::settings_rows::content_header_column()
                .when(hosts_caption, |column| {
                    column.pr(px(window_caption::CAPTION_CLUSTER_WIDTH))
                })
                // The strip carries no controls, so the whole title doubles as
                // the window's native drag handle.
                .child(window_caption::drag_region(crate::material::bar_title(
                    crate::tr!("settings.title"),
                    cx,
                ))),
        )
        // Painted last so the cluster stays on top of the strip.
        .children(hosts_caption.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .child(window_caption::caption_controls(window, cx))
        }))
        .into_any_element()
    }

    fn confirm_restore(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let page = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let page = page.clone();
            alert
                .title(crate::tr!("settings.restore_title"))
                .description(crate::tr!("settings.restore_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.restore"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    store.update(cx, |store, _cx| store.reset_settings());
                    DevicePreferences::restore_defaults(window, cx);
                    // Provider profiles and installed agents survive the reset,
                    // so their cards need no rebuild — only the panels and
                    // inputs holding a copy of a reset preference do.
                    page.update(cx, |page, cx| {
                        // The Home URL input now holds a stale override.
                        let home_url = page
                            .store
                            .read(cx)
                            .settings()
                            .browser
                            .home_url
                            .clone()
                            .unwrap_or_default();
                        page.home_url_input.push(home_url, window, cx);
                        page.new_project_dir_input.push(String::new(), window, cx);
                        page.project_start_custom_input
                            .push(String::new(), window, cx);
                        let device_name = DevicePreferences::device_name(cx);
                        page.device_name_input.push(device_name, window, cx);
                        let limit = remote_attachment_limit_value(cx);
                        page.remote_attachment_limit_input.push(limit, window, cx);
                    });
                    true
                })
        });
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.section_applies(&self.section, cx) {
            self.section = Section::General;
        }
        // Usage is fetched on demand: kick one refresh off on the transition
        // into the section (any entry path — nav click, route key, restore),
        // not on every frame it renders.
        if self.section == Section::Usage {
            if !self.usage_refresh_sent {
                self.usage_refresh_sent = true;
                self.store
                    .update(cx, |store, _cx| store.refresh_provider_usage());
            }
        } else {
            self.usage_refresh_sent = false;
        }
        if self.section != Section::ComputerUse {
            self.host_permissions = None;
        }
        if self.section != Section::Control {
            self.control.hidden();
        }
        if self.section != Section::Plugins {
            self.plugins_panel.update(cx, |panel, _| panel.hide());
        }
        if matches!(
            self.section,
            Section::SourceControl | Section::SourceControlHost
        ) {
            self.source_control_panel
                .update(cx, |panel, cx| panel.show(cx));
        } else {
            self.source_control_panel
                .update(cx, |panel, _| panel.hide());
        }
        let column = match self.section {
            Section::General => self.render_general(cx),
            Section::Threads => self.render_threads(window, cx),
            Section::ProjectThreadRules => v_flex().children(self.project_rules_editor.clone()),
            Section::SourceControl => self.render_source_control(cx),
            Section::SourceControlHost => v_flex().children(self.source_control_host.clone()),
            Section::Providers => self.render_providers(window, cx),
            Section::Harness => v_flex().children(self.harness_page.clone()),
            Section::Usage => self.render_usage(window, cx),
            Section::Browser => self.render_browser(cx),
            Section::ComputerUse => self.render_computer_use(cx),
            Section::Control => self.render_control(window, cx),
            Section::Plugins => v_flex().child(self.plugins_panel.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Remote => v_flex().child(self.remote.remote.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Spaces => v_flex().child(self.remote.spaces.clone()),
            #[cfg(feature = "remote-hosting")]
            Section::MachineSpaces => v_flex().child(self.remote.machine_spaces.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Group => v_flex().child(self.remote.group.clone()),
            #[cfg(feature = "remote-hosting")]
            Section::Traverse => v_flex().children(self.remote.traverse.clone()),
            Section::Archived => self.render_archived(window, cx),
            Section::Maintenance => self.render_maintenance(window, cx),
        };
        // The rail swaps sections in place, so a newly chosen section fades in;
        // compact sections arrive through the shell's navigation stack. The
        // fade is the page's, so the window turning wide mid-way carries it on
        // rather than fading the section in again under the page's new parent.
        let entering = self
            .motion
            .entered("section", self.section, CHANGE, window, cx);
        let column = if self.window_state.read(cx).compact {
            column
        } else {
            v_flex().opacity(entering).child(column)
        };
        let scroll = match self.section {
            Section::ProjectThreadRules => &self.project_rules_scroll,
            Section::SourceControlHost => &self.source_control_host_scroll,
            Section::Harness => &self.harness_page_scroll,
            _ => &self.content_scroll,
        };
        self.render_content_column(column, scroll, window, cx)
    }

    fn render_content_column(
        &self,
        column: gpui::Div,
        scroll: &ScrollHandle,
        window: &mut Window,
        cx: &App,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::scroll::page_viewport(
            "settings-scroll-bounce",
            scroll,
            div()
                .id("settings-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(scroll)
                .child(crate::settings_rows::content_column(
                    column,
                    compact,
                    &self.motion,
                    window,
                    cx,
                )),
        )
        .into_any_element()
    }

    /// Status belongs to the attached host; native grant controls stay local.
    fn permissions_group(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let column = v_flex().child(crate::material::group_caption(
            crate::tr!("computer_use.permissions_section"),
            cx,
        ));
        if self.capabilities.can_manage_local_permissions()
            && let Some(rows) = self.local_permission_rows()
        {
            return column.child(rows).into_any_element();
        }
        let rows = self.host_permissions.get_or_insert_with(|| {
            cx.new(|cx| crate::host_permissions::HostPermissions::new(self.store.clone(), cx))
        });
        column.child(rows.clone()).into_any_element()
    }

    #[cfg(all(feature = "local-permissions", target_os = "macos"))]
    fn local_permission_rows(&self) -> Option<AnyElement> {
        self.local_permissions
            .clone()
            .map(gpui::IntoElement::into_any_element)
    }

    #[cfg(not(all(feature = "local-permissions", target_os = "macos")))]
    fn local_permission_rows(&self) -> Option<AnyElement> {
        None
    }

    /// A phone manages one platform on a page pushed over Git Platforms.
    fn manage_source_control_host(
        &mut self,
        host: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let editor = cx.new(|cx| {
            crate::source_control_settings::HostEditor::new(store, host, false, true, window, cx)
        });
        self.source_control_host_subscription = Some(cx.subscribe(
            &editor,
            |this, _, _: &gpui::DismissEvent, cx| {
                this.window_state.update(cx, |state, cx| state.back(cx));
                this.section = Section::SourceControl;
                this.source_control_host = None;
                cx.notify();
            },
        ));
        self.source_control_host = Some(editor);
        self.section = Section::SourceControlHost;
        self.window_state.update(cx, |state, cx| {
            state.go(
                crate::window_state::Destination::SettingsSourceControlHost,
                cx,
            )
        });
        cx.notify();
    }

    fn render_source_control(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let remove_credits = self.store.read(cx).settings().remove_agent_credits_on_merge;
        let reset = crate::settings_rows::reset_action(
            "reset-remove-agent-credits",
            remove_credits,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_remove_agent_credits_on_merge(false), cx)
            }),
        );
        let row = self.toggle_row(
            "remove-agent-credits",
            crate::tr!("source_control.remove_agent_credits"),
            crate::tr!("source_control.remove_agent_credits_description"),
            remove_credits,
            reset,
            cx,
            WorkspaceStore::set_remove_agent_credits_on_merge,
        );
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(self.source_control_panel.clone())
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("source_control.merge_section"),
                        cx,
                    ))
                    .child(crate::material::group(cx).child(row)),
            )
    }

    /// A [`ToggleRow`] for one local setting, written through `intent`.
    #[allow(clippy::too_many_arguments)]
    fn toggle_row(
        &self,
        id: &'static str,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        checked: bool,
        reset: impl Into<Option<AnyElement>>,
        cx: &mut Context<Self>,
        intent: fn(&mut WorkspaceStore, bool),
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        ToggleRow::new(
            id,
            title,
            checked,
            compact,
            cx.listener(move |this, checked: &bool, _, cx| {
                this.dispatch_settings(|store| intent(store, *checked), cx);
            }),
        )
        .description(desc)
        .reset(reset)
        .into_any_element()
    }

    fn theme_row(&self, mode: ThemeMode, cx: &mut Context<Self>) -> AnyElement {
        let label = match mode {
            ThemeMode::System => crate::tr!("settings.theme.system"),
            ThemeMode::Light => crate::tr!("settings.theme.light"),
            ThemeMode::Dark => crate::tr!("settings.theme.dark"),
        };
        let option = |value, id: &'static str, label_key: &'static str| SelectRowOption {
            value,
            id: id.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: None,
            selected: value == mode,
        };
        crate::settings_rows::select_row(
            "theme-dropdown",
            "theme-popover",
            "theme-options-menu",
            160.,
            crate::tr!("settings.theme.title").into_owned().into(),
            None,
            label.into_owned().into(),
            vec![
                option(
                    ThemeMode::System,
                    "theme-option-system",
                    "settings.theme.system",
                ),
                option(
                    ThemeMode::Light,
                    "theme-option-light",
                    "settings.theme.light",
                ),
                option(ThemeMode::Dark, "theme-option-dark", "settings.theme.dark"),
            ],
            None,
            self.flip_row("theme-dropdown", cx),
            DevicePreferences::set_theme,
            cx,
        )
    }

    fn language_row(&self, language: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let selected = language.map(str::to_owned);
        let label = match language {
            Some(LANGUAGE_ENGLISH) => crate::tr!("settings.language.english"),
            Some(LANGUAGE_SIMPLIFIED_CHINESE) => crate::tr!("settings.language.chinese"),
            Some(LANGUAGE_JAPANESE) => crate::tr!("settings.language.japanese"),
            _ => crate::tr!("settings.language.system"),
        };
        let option = |value, key: &'static str| SelectRowOption {
            value,
            id: key.into(),
            label: crate::tr!(key).into_owned().into(),
            description: None,
            selected: selected.as_deref() == value,
        };
        crate::settings_rows::select_row(
            "language-dropdown",
            "language-popover",
            "language-options-menu",
            160.,
            crate::tr!("settings.language.title").into_owned().into(),
            None,
            label.into_owned().into(),
            vec![
                option(None, "settings.language.system"),
                option(Some(LANGUAGE_ENGLISH), "settings.language.english"),
                option(
                    Some(LANGUAGE_SIMPLIFIED_CHINESE),
                    "settings.language.chinese",
                ),
                option(Some(LANGUAGE_JAPANESE), "settings.language.japanese"),
            ],
            None,
            self.flip_row("language-dropdown", cx),
            DevicePreferences::set_language,
            cx,
        )
    }
}

impl SettingsPage {
    /// One section's content, as the compact detail page's body.
    pub(crate) fn render_compact_section(
        &mut self,
        destination: crate::window_state::Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // SettingsSection and its project-rules child share SettingsPage. Render
        // the real parent controls without mounting the child editor twice or
        // changing the selection. Each page retains its own scroll handle.
        if destination == crate::window_state::Destination::SettingsSection
            && self.window_state.read(cx).destination()
                == crate::window_state::Destination::SettingsThreadRules
        {
            let column = self.render_threads(window, cx);
            return self.render_content_column(column, &self.content_scroll, window, cx);
        }
        if destination == crate::window_state::Destination::SettingsSection
            && self.window_state.read(cx).destination()
                == crate::window_state::Destination::SettingsSourceControlHost
        {
            let column = self.render_source_control(cx);
            return self.render_content_column(column, &self.content_scroll, window, cx);
        }
        if self.section == Section::SourceControlHost
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsSourceControlHost
        {
            self.section = Section::SourceControl;
            self.source_control_host = None;
            self.source_control_host_subscription = None;
        }
        if self.section == Section::ProjectThreadRules
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsThreadRules
        {
            self.section = Section::Threads;
            self.project_rules_editor = None;
        }
        // The harness list under a harness's page, likewise.
        if destination == crate::window_state::Destination::SettingsSection
            && self.window_state.read(cx).destination()
                == crate::window_state::Destination::SettingsHarness
        {
            let column = self.render_providers(window, cx);
            return self.render_content_column(column, &self.content_scroll, window, cx);
        }
        if self.section == Section::Harness
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsHarness
        {
            self.section = Section::Providers;
            self.harness_page = None;
        }
        self.render_content(window, cx)
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No opaque full-page fill: the nav must sit on the same translucent
        // glass canvas the chat sidebar does (its `sidebar` token shows the
        // T0 blur through its own translucency), so navigating chat↔settings
        // never flips the window material. Only the content column is paper.
        gpui_base::h_flex()
            .size_full()
            .text_color(cx.theme().foreground)
            .child(self.render_nav(window, cx))
            .child(
                crate::window_seam::inset_from_seam(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .bg(crate::material::content_surface(cx))
                        .shadow_sm(),
                    "settings-content-seam",
                    gpui::Edges {
                        left: false,
                        ..gpui::Edges::all(true)
                    },
                    window,
                    cx,
                )
                .child(self.render_header(window, cx))
                .child(self.render_content(window, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests;
