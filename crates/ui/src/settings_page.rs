//! Full-page settings route with section navigation and editable settings.

mod control;
mod disk_usage;
pub(crate) mod thread_behavior;

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

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.host_permissions = None;
        if !self.section_applies(&section, cx) {
            self.section = Section::General;
            self.sync_holds(cx);
            Self::return_to_settings_root(&self.window_state, cx);
            cx.notify();
            return;
        }
        self.section = section;
        self.sync_holds(cx);
        // Arriving from a deep link or the palette must not leave the selected
        // section hidden inside a folded subgroup.
        if section.advanced() {
            self.advanced_expanded = Some(true);
        }
        // Which section is open is this page's business; *that* a detail is
        // open is navigation, and belongs to the window's one history.
        self.window_state.update(cx, |state, cx| {
            if state.compact {
                state.go(crate::window_state::Destination::SettingsSection, cx);
            }
        });
        cx.notify();
    }

    /// The open section's name, which is also this page's compact nav-bar
    /// title and the label its child's Back control carries.
    pub(crate) fn section_title(&self, cx: &App) -> SharedString {
        if self.section == Section::ProjectThreadRules
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsThreadRules
        {
            return Section::Threads.label();
        }
        if self.section == Section::SourceControlHost {
            return match &self.source_control_host {
                Some(editor)
                    if self.window_state.read(cx).destination()
                        == crate::window_state::Destination::SettingsSourceControlHost =>
                {
                    editor.read(cx).host.clone().into()
                }
                _ => Section::SourceControl.label(),
            };
        }
        // The phone page names just the project: the nav bar has no room for
        // the dialog's sentence-long title.
        if self.section == Section::ProjectThreadRules
            && let Some(editor) = &self.project_rules_editor
        {
            return editor.read(cx).project.name.clone().into();
        }
        if self.section == Section::Harness {
            if self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsHarness
            {
                return Section::Providers.label();
            }
            if let Some(page) = &self.harness_page {
                return crate::harness_dialog::harness_name(
                    self.store.read(cx),
                    page.read(cx).harness(),
                )
                .into();
            }
        }
        self.section.label()
    }

    fn nav_item(&self, section: Section, cx: &mut Context<Self>) -> TabItem {
        TabItem::new(section.id(), section.label())
            .icon(section.icon())
            .selected(self.section == section)
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
    }

    fn group_caption(&self, group: SectionGroup, cx: &Context<Self>) -> AnyElement {
        let (label, selector) = match group {
            SectionGroup::Device => (
                crate::tr!("settings.this_device").into_owned(),
                "settings-device-caption",
            ),
            SectionGroup::Machine if self.settings_hosts(cx).len() > 1 => {
                return self.host_selector(cx);
            }
            SectionGroup::Machine => {
                let store = self.store.read(cx);
                let name = if store.is_remote() {
                    store.remote_host_name().map(str::to_owned)
                } else {
                    crate::remote::local_machine_name(cx)
                }
                .filter(|name| !name.trim().is_empty());
                // A member manages none of the machine's settings; what this
                // group holds for it is the space's own threads.
                let label = store.space_label().unwrap_or_else(|| {
                    name.map_or_else(
                        || crate::tr!("settings.machine_settings_fallback").into_owned(),
                        |name| crate::tr!("settings.machine_settings", name = name).into_owned(),
                    )
                });
                (label, "settings-machine-caption")
            }
        };
        crate::material::group_caption(label, cx)
            .debug_selector(move || selector.into())
            .into_any_element()
    }

    /// The machine group's caption over several hosts: it names the host
    /// whose settings the group holds and chooses another.
    fn host_selector(&self, cx: &Context<Self>) -> AnyElement {
        let shown = self.store.read(cx).host_key();
        let options = self
            .settings_hosts(cx)
            .into_iter()
            .map(|store| {
                let store = store.read(cx);
                let host = store.host_key();
                SelectRowOption {
                    id: format!("settings-host-{}", host.name()).into(),
                    label: store.host_title(),
                    description: None,
                    selected: host == shown,
                    value: host,
                }
            })
            .collect();
        let label = crate::tr!(
            "settings.machine_settings",
            name = self.store.read(cx).host_title()
        );
        let trigger = Button::new("settings-host-selector")
            .debug_selector(|| "settings-host-selector".into())
            .text()
            .xsmall()
            .aria_label(crate::tr!("settings.choose_host"))
            .child(
                gpui_base::h_flex()
                    .gap_1()
                    .text_caption(cx)
                    .child(label)
                    .child(
                        Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground),
                    ),
            );
        gpui_base::h_flex()
            .pl(px(crate::material::CARD_INSET))
            .pb(cx.theme().tokens.spacing.sm)
            .child(crate::settings_rows::select_menu(
                trigger,
                "settings-host-popover",
                "settings-host-menu",
                200.,
                crate::tr!("settings.choose_host").into_owned().into(),
                options,
                self.window_state.read(cx).compact,
                |host, _, cx| crate::shell::choose_settings_host(host, cx),
                cx,
            ))
            .into_any_element()
    }

    /// Draw the rail at `opacity` from this frame on.
    pub(crate) fn fade_rail(this: &Entity<Self>, opacity: f32, cx: &mut App) {
        if this.read(cx).rail_opacity != opacity {
            this.update(cx, |page, _| page.rail_opacity = opacity);
        }
    }

    fn advanced_expanded(&self) -> bool {
        self.advanced_expanded
            .unwrap_or(!self.capabilities.folds_advanced())
    }

    /// The **Advanced** subgroup's disclosure, shared by the rail and the
    /// compact section list: the same caption style as a group's, plus the
    /// chevron that opens it.
    fn advanced_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let spacing = &cx.theme().tokens.spacing;
        // A row, so the button is as wide as its label, not the rail.
        gpui_base::h_flex()
            .pl(px(crate::material::CARD_INSET))
            .pt(spacing.sm)
            .pb(spacing.sm)
            .child(AdvancedToggle {
                expanded: self.advanced_expanded(),
                on_click: Rc::new(cx.listener(|this, _, _, cx| {
                    this.advanced_expanded = Some(!this.advanced_expanded());
                    cx.notify();
                })),
            })
            .into_any_element()
    }

    /// The wide rail's way out of Settings. Compact has no such row: there, the
    /// shell's nav bar carries Back like it does on every other page.
    fn back_row(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::material::bar(
            "settings-back-bar",
            crate::material::Bar::Toolbar,
            false,
            cx,
        )
        .child(
            crate::material::selectable_row(
                crate::material::accessible_clickable(
                    gpui_base::h_flex(),
                    "settings-back",
                    Role::Button,
                    crate::tr!("settings.back"),
                    cx,
                ),
                false,
                false,
                cx,
            )
            .flex_1()
            .text_body(false, cx)
            .text_color(cx.theme().sidebar_foreground)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.close_settings(cx));
            }))
            .child(Icon::new(IconName::ArrowLeft).text_color(cx.theme().muted_foreground))
            .child(crate::tr!("settings.back")),
        )
        .into_any_element()
    }

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let spacing = cx.theme().tokens.spacing;
        // One selection over the device, machine and advanced rails: its
        // fill glides from a row of one to a row of another.
        let mark = Rc::new(crate::widgets::tabs::Glide::new(
            &"settings-rail".into(),
            Some(crate::widgets::tabs::item_key(&self.section.id().into())),
            window,
            cx,
        ));
        let mut tabs = v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .px(spacing.sm)
            .gap(spacing.xxs)
            .child(mark.rail(cx));
        for (index, group) in [SectionGroup::Device, SectionGroup::Machine]
            .into_iter()
            .enumerate()
        {
            let sections = SECTIONS
                .into_iter()
                .filter(|section| section.group() == group && self.section_applies(section, cx))
                .collect::<Vec<_>>();
            if sections.is_empty() {
                continue;
            }
            tabs = tabs.child(
                div()
                    .when(index > 0, |caption| caption.pt(spacing.md))
                    .child(self.group_caption(group, cx)),
            );
            let plain = sections
                .iter()
                .copied()
                .filter(|section| !section.advanced())
                .map(|section| self.nav_item(section, cx))
                .collect::<Vec<_>>();
            tabs = tabs.child(
                TabBar::new(if group == SectionGroup::Device {
                    "settings-nav-tabs"
                } else {
                    "settings-machine-tabs"
                })
                .rail()
                .shared_mark(mark.clone())
                .aria_label(crate::tr!("settings.title"))
                .items(plain),
            );
            if sections.iter().any(|section| section.advanced()) {
                tabs = tabs.child(self.advanced_toggle(cx)).child(
                    crate::widgets::Collapsible::new(
                        "settings-advanced-tabs-fold",
                        self.advanced_expanded(),
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(
                        TabBar::new("settings-advanced-tabs")
                            .rail()
                            .shared_mark(mark.clone())
                            .aria_label(crate::tr!("settings.advanced"))
                            .items(
                                sections
                                    .into_iter()
                                    .filter(|section| section.advanced())
                                    .map(|section| self.nav_item(section, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
                );
            }
        }
        // Settings replaces the whole window, so this rail is the window's
        // left column: it carries the wordmark and clears the platform's
        // own window controls, exactly as the workspace sidebar does, and
        // keeps its width beside a side cutout.
        let seam = crate::window_seam::insets_along(
            "settings-rail-seam",
            gpui::Edges {
                right: false,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        );
        v_flex()
            .flex_none()
            .w(px(NAV_WIDTH) + seam.left)
            .h_full()
            .pt(seam.top)
            .pb(seam.bottom)
            .pl(seam.left)
            .opacity(self.rail_opacity)
            .bg(cx.theme().sidebar)
            .child(
                window_drag_area(
                    "settings-nav-drag",
                    crate::material::bar(
                        "settings-nav-strip",
                        crate::material::Bar::Strip,
                        false,
                        cx,
                    )
                    .pl(window_caption::traffic_light_inset(window).max(spacing.sm)),
                    window,
                    cx,
                )
                .child(crate::material::brand_wordmark(cx)),
            )
            .child(tabs)
            .child(div().flex_none().child(self.back_row(cx)))
            .into_any_element()
    }

    /// Compact clients have no room for a 255px rail beside the content, so the
    /// same sections become a full-width list that pushes to a detail page.
    /// The page draws no header and no back row: in compact every page wears
    /// the shell's one nav bar (`crate::shell`).
    pub(crate) fn render_compact_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let advanced_expanded = self.advanced_expanded();
        let mut column = v_flex()
            .w_full()
            .p(px(crate::material::COMPACT_PAGE_INSET))
            .gap(crate::material::section_gap(cx));
        for group in [SectionGroup::Device, SectionGroup::Machine] {
            let rows = |advanced: bool, cx: &mut Context<Self>| {
                SECTIONS
                    .into_iter()
                    .filter(|section| {
                        section.group() == group
                            && section.advanced() == advanced
                            && self.section_applies(section, cx)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|section| self.compact_section_row(section, cx))
                    .collect::<Vec<_>>()
            };
            let plain = rows(false, cx);
            let advanced = rows(true, cx);
            if plain.is_empty() && advanced.is_empty() {
                continue;
            }
            let mut card = v_flex().w_full().child(self.group_caption(group, cx));
            if !plain.is_empty() {
                card = card.child(crate::material::grouped(plain, cx));
            }
            if !advanced.is_empty() {
                card = card.child(self.advanced_toggle(cx)).child(
                    crate::widgets::Collapsible::new(
                        "settings-advanced-list-fold",
                        advanced_expanded,
                    )
                    .motion(&self.motion)
                    .w_full()
                    .content(crate::material::grouped(advanced, cx)),
                );
            }
            column = column.child(card);
        }
        crate::scroll::page_viewport(
            "settings-section-bounce",
            &self.section_scroll,
            div()
                .id("settings-section-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.section_scroll)
                .child(column),
        )
        .into_any_element()
    }

    /// One tappable section row of the compact list.
    fn compact_section_row(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let label = section.label();
        crate::material::list_row(section.id(), label.clone(), cx)
            .debug_selector(move || section.id().into())
            .text_body(true, cx)
            .child(Icon::new(section.icon()).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().child(label))
            .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
            .into_any_element()
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

    fn render_general(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let language = DevicePreferences::language(cx);
        let theme = DevicePreferences::theme(cx);
        let device_name_overridden = DevicePreferences::device_name_overridden(cx);
        let attachment_limit_overridden =
            DevicePreferences::remote_attachment_limit_mib(cx).is_some();
        let mut appearance = vec![
            self.language_row(language.as_deref(), cx),
            self.theme_row(theme, cx),
        ];
        if self.settings_hosts(cx).len() <= 1 {
            appearance.push(self.provider_marks_row(cx));
        }
        let device = vec![
            self.device_name_row(device_name_overridden, cx),
            self.remote_attachment_limit_row(attachment_limit_overridden, cx),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.appearance_section"),
                appearance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.device_section"),
                device,
                cx,
            ))
    }

    /// Whether the sidebar marks this host's threads with their provider: a
    /// setting of the host, so over several hosts it is among the threads'.
    fn provider_marks_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let marks = self.store.read(cx).settings().sidebar_provider_marks;
        let reset = crate::settings_rows::reset_action(
            "reset-sidebar-provider-marks",
            marks,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_sidebar_provider_marks(false), cx)
            }),
        );
        self.toggle_row(
            "sidebar-provider-marks",
            crate::tr!("settings.sidebar_provider_marks.title"),
            crate::tr!("settings.sidebar_provider_marks.description"),
            marks,
            reset,
            cx,
            WorkspaceStore::set_sidebar_provider_marks,
        )
    }

    fn render_maintenance(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let throttle_disabled = self
            .store
            .read(cx)
            .settings()
            .inactive_frame_throttle_disabled;
        let frame_throttle_reset = crate::settings_rows::reset_action(
            "reset-inactive-frame-throttle",
            throttle_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_inactive_frame_throttle_disabled(false),
                    cx,
                )
            }),
        );
        let performance = vec![self.toggle_row(
            "inactive-frame-throttle",
            crate::tr!("settings.inactive_frame_throttle.title"),
            crate::tr!("settings.inactive_frame_throttle.description"),
            // Stored inverted: checked = enabled.
            !throttle_disabled,
            frame_throttle_reset,
            cx,
            |store, checked| store.set_inactive_frame_throttle_disabled(!checked),
        )];
        let migration = self.store.read(cx).storage_migration().cloned();
        let migration = crate::motion::shown("storage-migration", migration, SHOW, window, cx)
            .map(|shown| shown.fold(self.storage_migration_group(&shown.value, cx)));
        v_flex()
            .gap(crate::material::section_gap(cx))
            .children(migration)
            .children(self.disk_usage_groups(window, cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.performance_section"),
                performance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.reset_section"),
                vec![self.restore_all_row(cx)],
                cx,
            ))
    }

    /// The host moving an older build's threads into its current storage.
    fn storage_migration_group(
        &self,
        migration: &tcode_protocol::StorageMigration,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let compact = self.window_state.read(cx).compact;
        let percent = 100. * migration.moved as f32 / migration.total.max(1) as f32;
        let row = crate::material::group_row_stacked(cx)
            .debug_selector(|| "storage-migration".into())
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("settings.storage_migration.title")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_meta_mono(compact, cx)
                            .child(format!("{}%", percent.floor() as u32)),
                    ),
            )
            .child(
                crate::widgets::progress::Progress::new("storage-migration-progress")
                    .small()
                    .value(percent),
            )
            .when(migration.waiting_for_space, |row| {
                row.child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("settings.storage_migration.waiting_for_space")),
                )
            });
        crate::settings_rows::settings_group(
            crate::tr!("settings.storage_migration.section"),
            vec![row.into_any_element()],
            cx,
        )
    }

    /// The whole-settings escape hatch: the per-item buttons cover single
    /// settings, this one covers the rest and keeps its confirm dialog.
    fn restore_all_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let row = self.flip_row("restore-all", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("settings.reset_all"),
                Some(crate::tr!("settings.restore_description").into()),
                None,
                row.compact(),
                cx,
            ),
            Button::new("restore-defaults")
                .danger()
                .outline()
                .small()
                .label(crate::tr!("settings.restore"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.confirm_restore(window, cx);
                })),
            cx,
        );
        row.render(layout)
    }

    fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        // Provider, model and profile are one setting to the user, so they
        // reset together; the picker follows through the store observer.
        let overridden =
            self.store.read(cx).settings().title_generation != TitleGenerationSettings::default();
        let reset = crate::settings_rows::reset_action(
            "reset-title-generation",
            overridden,
            cx.listener(|this, _, _, cx| {
                let default = TitleGenerationSettings::default();
                this.dispatch_settings(
                    move |store| {
                        store.set_title_generation_provider(default.provider);
                        store.set_title_generation_model(default.model);
                        store.set_title_generation_profile_id(default.profile_id);
                    },
                    cx,
                );
            }),
        );
        let row = self.flip_row("title-generation", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("settings.title_generation.title"),
                Some(crate::tr!("settings.title_generation.description").into()),
                reset,
                row.compact(),
                cx,
            ),
            self.title_model_picker.clone(),
            cx,
        );
        row.render(layout)
    }

    fn device_name_row(&self, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let reset = crate::settings_rows::reset_action(
            "reset-device-name",
            overridden,
            cx.listener(|this, _, window, cx| {
                DevicePreferences::set_device_name(None, cx);
                let name = DevicePreferences::device_name(cx);
                this.device_name_input.push(name, window, cx);
            }),
        );
        let row = self.flip_row("device-name", cx);
        let layout = row
            .row(
                crate::settings_rows::row_labels(
                    crate::tr!("settings.device_name.title"),
                    Some(crate::tr!("settings.device_name.description").into()),
                    reset,
                    compact,
                    cx,
                ),
                row.field(
                    Input::new(&self.device_name_input.state)
                        .placeholder(crate::tr!("settings.device_name.placeholder")),
                    cx,
                ),
                cx,
            )
            .debug_selector(|| "settings-device-name-row".into());
        row.render(layout)
    }

    fn remote_attachment_limit_row(&self, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-remote-attachment-limit",
            overridden,
            cx.listener(|this, _, window, cx| {
                DevicePreferences::set_remote_attachment_limit_mib(None, cx);
                let limit = remote_attachment_limit_value(cx);
                this.remote_attachment_limit_input.push(limit, window, cx);
            }),
        );
        let row = self.flip_row("remote-attachment-limit", cx);
        let layout = row
            .row(
                crate::settings_rows::row_labels(
                    crate::tr!("settings.remote_attachment_limit.title"),
                    Some(crate::tr!("settings.remote_attachment_limit.description").into()),
                    reset,
                    row.compact(),
                    cx,
                ),
                crate::widgets::input::NumberInput::new(&self.remote_attachment_limit_input.state)
                    .accessibility_label(crate::tr!("settings.remote_attachment_limit.title")),
                cx,
            )
            .debug_selector(|| "settings-remote-attachment-limit-row".into());
        row.render(layout)
    }

    fn render_tcode_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.store.read(cx).tcode_update_status();
        let version = status.latest.unwrap_or_default();
        let release_url = status.release_url.unwrap_or_default();

        crate::material::overlay_popover("tcode-update-popover", cx)
            .p_3()
            .trigger(
                Button::new("tcode-update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .label(crate::tr!(
                        "providers.tcode_update_available",
                        version = version
                    ))
                    .tooltip(crate::tr!("providers.tcode_update_aria")),
            )
            .content(move |_, _, cx| {
                let release_url = release_url.clone();
                v_flex()
                    // A reading measure for the message; gpui-base has no popover width.
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_title(cx)
                            .child(crate::tr!("providers.tcode_update_title")),
                    )
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.tcode_update_message")),
                    )
                    .child(
                        Button::new("view-tcode-release")
                            .primary()
                            .small()
                            .label(crate::tr!("providers.view_release"))
                            .on_click(move |_, _, cx| cx.open_url(&release_url)),
                    )
            })
            .into_any_element()
    }

    /// Settings → Harnesses: native providers and installed ACP agents share one
    /// bordered list. The marketplace lives behind the Add agent dialog.
    fn render_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let checked_at = self.store.read(cx).providers_checked_at();
        let checking = self.store.read(cx).providers_checking();
        let run = self.store.read(cx).provider_update_run();
        let automatic = self.store.read(cx).automatic_provider_updates();
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .pl(px(crate::material::CARD_INSET))
                    .text_caption(cx)
                    .child(crate::tr!("settings.providers_section")),
            );
        let checked = checked_at.map(|checked_at| {
            let ago = humanize_ago(
                self.store
                    .read(cx)
                    .host_now_secs(cx)
                    .saturating_sub(checked_at),
            );
            crate::tr!("providers.checked", when = ago).into_owned()
        });
        if let Some(checked) = crate::motion::shown("providers-checked", checked, SHOW, window, cx)
        {
            header = header.child(
                checked.fade(
                    div()
                        .text_secondary(compact, cx)
                        .child(checked.value.clone()),
                ),
            );
        }
        let update_available = self.store.read(cx).tcode_update_status().update_available;
        if let Some(update) = crate::motion::shown(
            "tcode-update",
            update_available.then_some(()),
            SHOW,
            window,
            cx,
        ) {
            header = header.child(update.fade(self.render_tcode_update_popover(cx)));
        }
        header = header.child(
            Button::new("add-acp-agent")
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .label(crate::tr!("providers.acp.add_agent").into_owned())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_acp_dialog(window, cx);
                })),
        );
        if let Some(update_all) = crate::motion::shown(
            "update-all-providers",
            (!automatic.is_empty()).then(|| automatic.clone()),
            SHOW,
            window,
            cx,
        ) {
            let automatic = update_all.value.clone();
            header = header.child(
                update_all.fade(
                    Button::new("update-all-providers")
                        .outline()
                        .xsmall()
                        .loading(run.is_some())
                        .label(crate::tr!("providers.update_all"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, _cx| store.update_providers(automatic.clone()));
                        })),
                ),
            );
        }
        header = header.child(
            Button::new("refresh-providers")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("providers.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, _cx| {
                        store.refresh_provider_status();
                        store.check_provider_versions();
                    });
                })),
        );

        let update_checks_disabled = self
            .store
            .read(cx)
            .settings()
            .provider_update_checks_disabled;
        let provider_updates_reset = crate::settings_rows::reset_action(
            "reset-provider-update-checks",
            update_checks_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_provider_update_checks_disabled(false), cx)
            }),
        );
        let updates = vec![self.toggle_row(
            "provider-update-checks",
            crate::tr!("settings.provider_updates.title"),
            crate::tr!("settings.provider_updates.description"),
            // Stored inverted: checked = enabled.
            !update_checks_disabled,
            provider_updates_reset,
            cx,
            |store, checked| store.set_provider_update_checks_disabled(!checked),
        )];

        // One row per harness: each native one this client sees, then each
        // installed ACP agent.
        let harnesses: Vec<(SharedString, HarnessId)> = {
            let store = self.store.read(cx);
            let profiles = store.all_provider_profiles();
            agent::ProviderKind::NATIVE
                .into_iter()
                .filter(|kind| profiles.iter().any(|profile| profile.kind == *kind))
                .map(HarnessId::Native)
                .chain(
                    store
                        .settings_installed_acp_agents()
                        .into_iter()
                        .map(|agent| HarnessId::Acp(agent.id)),
                )
                .map(|harness| (SharedString::from(harness.key()), harness))
                .collect()
        };
        let page = cx.entity().downgrade();
        let store = self.store.clone();
        let provider_rows = crate::settings_rows::grouped_keyed(
            "harness-rows",
            harnesses,
            window,
            cx,
            |_, harness, _, cx| {
                let page = page.clone();
                let open = harness.clone();
                crate::provider_card::harness_row(
                    &store,
                    harness,
                    move |_, window, cx| {
                        _ = page.update(cx, |page, cx| page.open_harness(open.clone(), window, cx));
                    },
                    cx,
                )
            },
        );

        let mut section = v_flex().w_full().gap_3().child(header);
        if let Some(shown) = crate::motion::shown("provider-update-run", run, SHOW, window, cx) {
            let run = &shown.value;
            section = section.child(
                shown.fold(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .px(px(crate::material::CARD_INSET))
                        .child(
                            div().text_secondary(compact, cx).child(
                                crate::tr!(
                                    "providers.update_progress",
                                    provider = run
                                        .current
                                        .map(|provider| provider.display_name())
                                        .unwrap_or_default(),
                                    completed = run.completed,
                                    total = run.total
                                )
                                .into_owned(),
                            ),
                        )
                        .child(
                            crate::widgets::progress::Progress::new("provider-update-progress")
                                .value(100. * run.completed as f32 / run.total.max(1) as f32),
                        ),
                ),
            );
        }
        if let Some(error) = self.store.read(cx).login_shell_unread() {
            section = section.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "providers-login-shell-unread".into())
                    .child(div().flex_1().min_w_0().text_secondary(compact, cx).child(
                        crate::tr!("providers.login_shell_unread", error = error).into_owned(),
                    )),
            );
        }
        if let Some(error) = self.store.read(cx).tcode_update_status().check_error {
            section = section.child(
                crate::material::notice(cx.theme().warning, cx)
                    .debug_selector(|| "providers-tcode-update-unchecked".into())
                    .child(div().flex_1().min_w_0().text_secondary(compact, cx).child(
                        crate::tr!("providers.tcode_update_unchecked", error = error).into_owned(),
                    )),
            );
        }
        section = section.child(crate::material::group(cx).child(provider_rows));
        // The update toggle follows the cards it governs; above them its caption
        // would stack straight under the page's own header row.
        section.child(div().pt(crate::material::section_gap(cx)).child(
            crate::settings_rows::settings_group(
                crate::tr!("settings.updates_section"),
                updates,
                cx,
            ),
        ))
    }

    /// Show the rate-limit windows each enabled Codex / Claude Code profile
    /// reports; missing windows are not synthesized.
    fn render_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let store = self.store.read(cx);
        let profiles: Vec<_> = store
            .enabled_profiles()
            .into_iter()
            .filter(|profile| profile.supports_account_usage())
            .collect();
        let rows: Vec<(String, String, agent::ProviderKind, _, bool)> = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    store.provider_profile_display_name(&profile.id),
                    profile.kind,
                    store.provider_usage(&profile.id),
                    store.usage_checking(&profile.id),
                )
            })
            .collect();
        let updated_at = rows
            .iter()
            .filter_map(|(_, _, _, usage, _)| usage.as_ref().map(|usage| usage.fetched_at))
            .max();
        let checking = rows.iter().any(|(_, _, _, _, checking)| *checking);
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex().w_full().items_center().gap_2().child(
            div()
                .flex_1()
                .pl(px(crate::material::CARD_INSET))
                .text_caption(cx)
                .child(crate::tr!("usage.section")),
        );
        let updated = updated_at.map(|updated_at| {
            let ago = humanize_ago(
                self.store
                    .read(cx)
                    .host_now_secs(cx)
                    .saturating_sub(updated_at),
            );
            crate::tr!("usage.updated", when = ago).into_owned()
        });
        if let Some(updated) = crate::motion::shown("usage-updated", updated, SHOW, window, cx) {
            header = header.child(
                updated.fade(
                    div()
                        .text_secondary(compact, cx)
                        .child(updated.value.clone()),
                ),
            );
        }
        header = header.child(
            Button::new("refresh-usage")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("usage.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store
                        .update(cx, |store, _cx| store.refresh_provider_usage());
                })),
        );

        // Each card and the note carry the gap above them, so it comes and
        // goes with them.
        let mut section = v_flex().w_full().child(header);
        if let Some(none) = crate::motion::shown(
            "usage-no-profiles",
            rows.is_empty().then_some(()),
            SHOW,
            window,
            cx,
        ) {
            section = section.child(div().w_full().pt_3().child(none.fade(
                crate::material::grouped(
                    vec![self.usage_note_row(
                        crate::tr!("usage.no_profiles").into_owned(),
                        None,
                        cx,
                    )],
                    cx,
                ),
            )));
        }
        let cards = rows
            .into_iter()
            .map(|row| (SharedString::from(row.0.clone()), row));
        let cards = crate::motion::keyed(
            "usage-cards",
            cards,
            ROWS,
            window,
            cx,
            |_, (profile_id, name, kind, usage, checking), window, cx| {
                div()
                    .w_full()
                    .pt_3()
                    .child(self.usage_card(
                        profile_id,
                        name,
                        *kind,
                        usage.as_ref(),
                        *checking,
                        window,
                        cx,
                    ))
                    .into_any_element()
            },
        );
        section.child(cards.render(v_flex().w_full()))
    }

    /// One profile's usage: its header, then its windows, or why there are
    /// none.
    #[allow(clippy::too_many_arguments)]
    fn usage_card(
        &self,
        profile_id: &str,
        name: &str,
        kind: agent::ProviderKind,
        usage: Option<&tcode_core::usage::ProviderUsage>,
        checking: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card_id = gpui::ElementId::from(SharedString::from(format!("usage-card-{profile_id}")));
        let plan = crate::motion::shown(
            (card_id.clone(), "plan"),
            usage.and_then(|u| u.plan.clone()),
            SHOW,
            window,
            cx,
        );
        let header = self.usage_card_header(profile_id, name, kind, plan, cx);
        let reported = usage.filter(|usage| usage.error.is_some() || !usage.windows.is_empty());
        let error = crate::motion::shown(
            (card_id.clone(), "error"),
            reported.and_then(|usage| usage.error.clone()),
            SHOW,
            window,
            cx,
        );
        let hairline =
            |cx: &App| crate::material::row_hairline(px(crate::material::CARD_INSET), cx);
        let slot = match reported {
            Some(_) => "windows",
            None if checking => "checking",
            None => "no-data",
        };
        let body = crate::motion::cross_fade(
            (card_id.clone(), "body"),
            slot,
            CHANGE,
            window,
            cx,
            |slot, window, cx| match *slot {
                "windows" => {
                    // The error fold draws its own hairline, and each window
                    // the one above it, so each line folds away with its row.
                    let error = match error.as_ref() {
                        Some(error) => {
                            error.fold(div().child(hairline(cx)).child(self.usage_note_row(
                                crate::tr!("usage.unavailable").into_owned(),
                                Some(error.value.clone()),
                                cx,
                            )))
                        }
                        None => div().into_any_element(),
                    };
                    let now = self.store.read(cx).host_now_secs(cx);
                    let windows = reported
                        .into_iter()
                        .flat_map(|usage| usage.windows.iter())
                        .map(|window| {
                            (
                                SharedString::from(crate::usage::window_label(window)),
                                window.clone(),
                            )
                        });
                    let windows = crate::motion::keyed(
                        (card_id.clone(), "windows"),
                        windows,
                        ROWS,
                        window,
                        cx,
                        |_, window, _, cx| {
                            crate::settings_rows::hairlined(
                                self.usage_window_row(window, now, cx),
                                cx,
                            )
                        },
                    );
                    v_flex()
                        .w_full()
                        .child(error)
                        .child(windows.render(v_flex().w_full()))
                        .into_any_element()
                }
                slot => v_flex()
                    .w_full()
                    .child(hairline(cx))
                    .child(self.usage_note_row(
                        if slot == "checking" {
                            crate::tr!("usage.checking").into_owned()
                        } else {
                            crate::tr!("usage.no_data").into_owned()
                        },
                        None,
                        cx,
                    ))
                    .into_any_element(),
            },
        );
        div()
            .id(card_id)
            .child(crate::material::grouped(vec![header, body], cx))
            .into_any_element()
    }

    fn usage_card_header(
        &self,
        profile_id: &str,
        name: &str,
        kind: agent::ProviderKind,
        plan: Option<crate::motion::Shown<String>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row(cx)
            .text_body(compact, cx)
            .child(crate::provider_card::profile_glyph(
                self.store.read(cx),
                kind,
                Some(profile_id),
            ))
            .child(div().flex_1().child(name.to_owned()))
            .when_some(plan, |row, plan| {
                row.child(plan.fade(crate::material::semantic_chip(
                    crate::usage::plan_label(&plan.value),
                    cx.theme().muted,
                    muted,
                    cx,
                )))
            })
            .into_any_element()
    }

    fn usage_window_row(
        &self,
        window: &tcode_core::usage::UsageWindow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let fill = crate::usage::bar_color(window.used_percent, cx);
        let label = crate::usage::window_label(window);
        crate::material::group_row_stacked(cx)
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_caption(cx).child(label.clone()))
                            .when_some(
                                crate::usage::resets_label(window.resets_at, now),
                                |col, label| {
                                    col.child(div().text_secondary(compact, cx).child(label))
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_meta_mono(compact, cx)
                            .child(crate::usage::percent_label(window.used_percent)),
                    ),
            )
            .child(
                crate::widgets::progress::Progress::new(SharedString::from(format!(
                    "usage-window-{label}"
                )))
                .small()
                .color(fill)
                .value(window.used_percent),
            )
            .into_any_element()
    }

    fn usage_note_row(
        &self,
        label: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row_stacked(cx)
            .text_secondary(compact, cx)
            .child(label)
            .children(detail)
            .into_any_element()
    }

    /// Archived sessions: loading, a failed load, the empty state and the
    /// list cross-fade into one another.
    fn render_archived(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let groups = self.store.read(cx).archived_groups();
        let failure = self.store.read(cx).archived_failure().map(str::to_owned);
        let slot = if !groups.is_empty() {
            "list"
        } else if failure.is_some() {
            "failed"
        } else if self.store.read(cx).archived_loading() {
            "loading"
        } else {
            "empty"
        };
        let mut groups = Some(groups).filter(|groups| !groups.is_empty());
        let body = crate::motion::cross_fade(
            "archived-body",
            slot,
            CHANGE,
            window,
            cx,
            |slot, window, cx| {
                match *slot {
                    "list" => match groups.take() {
                        Some(groups) => self
                            .render_archived_list(groups, window, cx)
                            .into_any_element(),
                        // A list leaving for the empty state has no rows left.
                        None => div().into_any_element(),
                    },
                    slot => v_flex()
                        .gap(crate::material::section_gap(cx))
                        .child(crate::material::group_caption(
                            crate::tr!("settings.archived_section"),
                            cx,
                        ))
                        .child(if slot == "loading" {
                            crate::material::empty_row(crate::tr!("settings.archived_loading"), cx)
                        } else if slot == "failed" {
                            crate::material::empty_row(
                                crate::tr!(
                                    "settings.archived_failed",
                                    error = failure.clone().unwrap_or_default()
                                ),
                                cx,
                            )
                        } else {
                            crate::material::empty_state(
                                Icon::new(Section::Archived.icon()),
                                crate::tr!("settings.archived_empty"),
                                String::new(),
                                cx,
                            )
                        })
                        .into_any_element(),
                }
            },
        );
        v_flex().child(body)
    }

    fn render_archived_list(
        &mut self,
        groups: Vec<tcode_core::project::ProjectGroup>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let full = self.store.read(cx).scope().is_full();

        let flat: Vec<((SharedString, SharedString), SessionMeta)> = groups
            .into_iter()
            .flat_map(|group| {
                let project = (
                    SharedString::from(group.project.id),
                    SharedString::from(group.project.name),
                );
                group
                    .sessions
                    .into_iter()
                    .map(move |meta| (project.clone(), meta))
            })
            .collect();
        let total = flat.len();
        let (page, start, end) = page_bounds(total, self.archived_page, ARCHIVED_PER_PAGE);
        self.archived_page = page;

        let now = self.store.read(cx).host_now_secs(cx);
        // Each part below the controls carries the gap above it, so a project
        // whose last archived thread goes takes its gap with it.
        let gap = crate::material::section_gap(cx);
        let col = v_flex()
            .child(
                gpui_base::h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_secondary(self.window_state.read(cx).compact, cx)
                            .child(crate::tr!(
                                "settings.archived_range",
                                from = start + 1,
                                to = end,
                                count = total
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("archived-prev")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_prev"))
                            .disabled(page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page = this.archived_page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("archived-next")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_next"))
                            .disabled(end >= total)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page += 1;
                                cx.notify();
                            })),
                    )
                    .when(full, |row| {
                        row.child(
                            Button::new("archived-delete-all")
                                .danger()
                                .small()
                                .label(crate::tr!("settings.archived_delete_all"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_delete_all_archived(window, cx);
                                })),
                        )
                    }),
            )
            .child(div().pt(gap).child(crate::material::group_caption(
                crate::tr!("settings.archived_section"),
                cx,
            )));
        // The page's rows by project, in order; a page turned is a new set of
        // rows, so the old page folds away as the new one folds in.
        let mut projects: Vec<(SharedString, (SharedString, Vec<SessionMeta>))> = Vec::new();
        for ((project_id, project_name), meta) in &flat[start..end] {
            match projects.last_mut() {
                Some((id, (_, sessions))) if id == project_id => sessions.push(meta.clone()),
                _ => projects.push((
                    project_id.clone(),
                    (project_name.clone(), vec![meta.clone()]),
                )),
            }
        }
        let compact = self.window_state.read(cx).compact;
        let projects = crate::motion::keyed(
            "archived-projects",
            projects,
            ROWS,
            window,
            cx,
            |project_id, (project_name, sessions), window, cx| {
                let rows = sessions
                    .iter()
                    .map(|meta| (SharedString::from(meta.id.clone()), meta.clone()));
                let rows = crate::motion::keyed(
                    SharedString::from(format!("archived-rows-{project_id}")),
                    rows,
                    ROWS,
                    window,
                    cx,
                    |_, meta, _, cx| self.archived_row(meta, now, full, compact, cx),
                );
                v_flex()
                    .pt(gap)
                    .child(crate::material::group_caption(project_name.clone(), cx))
                    .child(rows.render(crate::material::group(cx)))
                    .into_any_element()
            },
        );
        col.child(projects.render(v_flex().w_full()))
    }

    fn archived_row(
        &self,
        meta: &SessionMeta,
        now: u64,
        full: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let created = crate::tr!(
            "settings.archived_created",
            when = humanize_ago(now.saturating_sub(meta.created_at))
        );
        let desc = match meta.archived_at {
            Some(archived_at) => format!(
                "{} · {created}",
                crate::tr!(
                    "settings.archived_at",
                    when = humanize_ago(now.saturating_sub(archived_at))
                ),
            ),
            None => created.into_owned(),
        };
        let id_unarchive = meta.id.clone();
        let id_delete = meta.id.clone();
        let title = meta.title.clone();
        let row =
            crate::settings_rows::flip_row(&self.motion, format!("archived-{}", meta.id), compact);
        let layout = row.row(
            crate::settings_rows::row_labels(
                meta.title.clone(),
                Some(desc.into()),
                None,
                compact,
                cx,
            ),
            gpui_base::h_flex()
                .flex_none()
                .gap_2()
                .child(
                    Button::new(SharedString::from(format!("unarchive-{}", meta.id)))
                        .outline()
                        .small()
                        .label(crate::tr!("settings.unarchive"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = id_unarchive.clone();
                            this.store.update(cx, |store, _cx| {
                                store.unarchive_session(id);
                            });
                        })),
                )
                .when(full, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("delete-perm-{}", meta.id)))
                            .danger()
                            .small()
                            .label(crate::tr!("settings.delete_permanently"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_archived(&id_delete, &title, window, cx);
                            })),
                    )
                }),
            cx,
        );
        row.render(layout)
    }

    fn confirm_delete_archived(
        &self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::sidebar::confirm_delete(
            self.store.clone(),
            session_id.to_string(),
            title.to_string(),
            crate::tr!("settings.delete_permanently").into(),
            window,
            cx,
        );
    }

    /// Bulk permanent delete. Always confirms, whatever
    /// `skip_delete_confirmation` says — this destroys every archived thread.
    fn confirm_delete_all_archived(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(deletion) = self.store.read(cx).archived_deletion() else {
            return;
        };
        let (title, description) = if deletion.unarchived == 0 {
            (
                crate::tr!(
                    "settings.archived_delete_all_title",
                    count = deletion.archived
                ),
                crate::tr!("settings.archived_delete_all_description"),
            )
        } else {
            (
                crate::tr!(
                    "settings.archived_delete_all_tree_title",
                    count = deletion.archived,
                    unarchived = deletion.unarchived
                ),
                crate::tr!(
                    "settings.archived_delete_all_tree_description",
                    total = deletion.archived + deletion.unarchived
                ),
            )
        };
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let deletion = deletion.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.delete_permanently"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let deletion = deletion.clone();
                    store.update(cx, |store, _cx| store.delete_archived(deletion));
                    true
                })
        });
    }

    fn render_computer_use(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let compact = self.window_state.read(cx).compact;
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-cu-enabled",
            settings.computer_use.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_enabled(false), cx)
            }),
        );
        let allow_input_reset = crate::settings_rows::reset_action(
            "reset-cu-allow-input",
            !settings.computer_use.allow_input,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_allow_input(true), cx)
            }),
        );
        let show_agent_cursor_reset = crate::settings_rows::reset_action(
            "reset-cu-show-agent-cursor",
            !settings.computer_use.show_agent_cursor,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_show_agent_cursor(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                crate::tr!("computer_use.enable.title"),
                crate::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_computer_use_enabled,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                crate::tr!("computer_use.allow_input.title"),
                crate::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                allow_input_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_input,
            ),
            self.toggle_row(
                "cu-show-agent-cursor",
                crate::tr!("computer_use.show_agent_cursor.title"),
                crate::tr!("computer_use.show_agent_cursor.description"),
                settings.computer_use.show_agent_cursor,
                show_agent_cursor_reset,
                cx,
                WorkspaceStore::set_computer_use_show_agent_cursor,
            ),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("computer_use.section"),
                        cx,
                    ))
                    .child(crate::material::group(cx).children(rows))
                    .children(
                        self.store
                            .read(cx)
                            .harness_descriptions()
                            .iter()
                            .filter(|description| description.native_computer_use)
                            .map(|description| description.provider)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .map(|kind| {
                                div()
                                    .pt(cx.theme().tokens.spacing.sm)
                                    .pl(px(crate::material::CARD_INSET))
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!(
                                        "providers.native_computer_use",
                                        name = kind.display_name()
                                    ))
                            }),
                    ),
            )
            .child(self.permissions_group(cx))
    }

    fn render_browser(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-browser-enabled",
            !settings.browser.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_enabled(true), cx)
            }),
        );
        let allow_eval_reset = crate::settings_rows::reset_action(
            "reset-browser-allow-eval",
            !settings.browser.allow_evaluate,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_allow_evaluate(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                crate::tr!("browser.enable.title"),
                crate::tr!("browser.enable.description"),
                settings.browser.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_browser_enabled,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                crate::tr!("browser.allow_evaluate.title"),
                crate::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                allow_eval_reset,
                cx,
                WorkspaceStore::set_browser_allow_evaluate,
            ),
        ];
        v_flex().gap(crate::material::section_gap(cx)).child(
            v_flex()
                .child(crate::material::group_caption(
                    crate::tr!("browser.section"),
                    cx,
                ))
                .child(crate::material::group(cx).children(rows)),
        )
    }

    fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let overridden = self.store.read(cx).settings().browser.home_url.is_some();
        let reset = crate::settings_rows::reset_action(
            "reset-browser-home-url",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_browser_home_url(None), cx);
                this.home_url_input.push(String::new(), window, cx);
            }),
        );
        let row = self.flip_row("browser-home-url", cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("browser.home_url.title"),
                Some(crate::tr!("browser.home_url.description").into()),
                reset,
                compact,
                cx,
            ),
            row.field(
                Input::new(&self.home_url_input.state)
                    .placeholder(crate::tr!("browser.home_url.placeholder")),
                cx,
            ),
            cx,
        );
        row.render(layout)
    }

    fn image_mode_row(&self, mode: ImageMode, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-cu-image-mode",
            mode != ImageMode::Auto,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_image_mode(ImageMode::Auto),
                    cx,
                );
            }),
        );
        let label = match mode {
            ImageMode::Auto => crate::tr!("computer_use.image_mode.auto"),
            ImageMode::Always => crate::tr!("computer_use.image_mode.always"),
            ImageMode::Never => crate::tr!("computer_use.image_mode.never"),
        };
        let option = |value, label_key: &'static str, desc_key: &'static str| SelectRowOption {
            value,
            id: label_key.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: Some(crate::tr!(desc_key).into_owned().into()),
            selected: value == mode,
        };
        crate::settings_rows::select_row(
            "cu-image-mode-dropdown",
            "cu-image-mode-popover",
            "cu-image-mode-menu",
            260.,
            crate::tr!("computer_use.image_mode.title")
                .into_owned()
                .into(),
            Some(
                crate::tr!("computer_use.image_mode.description")
                    .into_owned()
                    .into(),
            ),
            label.into_owned().into(),
            vec![
                option(
                    ImageMode::Auto,
                    "computer_use.image_mode.auto",
                    "computer_use.image_mode.auto_desc",
                ),
                option(
                    ImageMode::Always,
                    "computer_use.image_mode.always",
                    "computer_use.image_mode.always_desc",
                ),
                option(
                    ImageMode::Never,
                    "computer_use.image_mode.never",
                    "computer_use.image_mode.never_desc",
                ),
            ],
            reset,
            self.flip_row("cu-image-mode-dropdown", cx),
            {
                let page = cx.entity();
                move |mode, _, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(|store| store.set_computer_use_image_mode(mode), cx)
                    })
                }
            },
            cx,
        )
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
mod tests {
    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_protocol::SettingsPatch;
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    use super::*;
    use crate::store::WorkspaceAttachment;

    fn capabilities(
        preview_backend: bool,
        hosting: bool,
        local_permissions: bool,
        remote_attachment: bool,
    ) -> SettingsCapabilities {
        SettingsCapabilities {
            preview_backend,
            hosting,
            local_permissions,
            remote_attachment,
        }
    }

    /// A full-scope store whose host is scripted by the test: what the store
    /// sends arrives on the first channel, and lines sent on the second
    /// arrive from the host while the returned pump runs.
    fn scripted_store(
        attachment: WorkspaceAttachment,
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        async_channel::Receiver<String>,
        async_channel::Sender<String>,
        gpui::Task<()>,
    ) {
        cx.update(crate::test_client::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| WorkspaceStore::new_attached(link, attachment, None, None, cx));
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        (store, outgoing, incoming, pump)
    }

    #[gpui::test]
    fn archived_delete_uses_response_worktree_sharing_and_preserves_the_cleanup_choice(
        cx: &mut TestAppContext,
    ) {
        use tcode_protocol::{
            ArchivedSessions, ClientPayload, Command, HostMessage, Query, QueryResponse,
            decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
        store.update(cx, |store, cx| store.show_alone("live".into(), cx));
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        page.update(cx, |page, cx| page.select_section(Section::Archived, cx));
        cx.run_until_parked();
        let mut archived_query = None;
        while let Ok(line) = outgoing.try_recv() {
            let message = decode_client_line(&line).unwrap();
            match message.payload {
                ClientPayload::Query(Query::ArchivedSessions) => archived_query = Some(message.id),
                ClientPayload::Command(_) => incoming
                    .try_send(
                        encode_line(&HostMessage::Ack {
                            id: message.id,
                            result: Ok(tcode_protocol::CommandResponse::Unit),
                        })
                        .unwrap(),
                    )
                    .unwrap(),
                _ => {}
            }
        }
        let sessions = ["shared", "remove", "keep"]
            .into_iter()
            .map(|id| {
                let mut meta = tcode_core::project::SessionMeta::new(
                    agent::ProviderKind::Codex,
                    std::path::PathBuf::from(format!("/archive/worktree/{id}")),
                    None,
                );
                meta.id = id.into();
                meta.title = id.into();
                meta.archived_at = Some(1);
                meta.worktree = Some(tcode_core::project::WorktreeInfo {
                    root_project_path: "/archive/project".into(),
                    base: "main".into(),
                    branch: format!("tcode/archive/{id}"),
                });
                meta
            })
            .collect();
        incoming
            .try_send(
                encode_line(&HostMessage::QueryResult {
                    id: archived_query.expect("opening Archived queries the host"),
                    result: Ok(QueryResponse::ArchivedSessions(ArchivedSessions {
                        sessions,
                        worktree_shared: std::collections::HashSet::from(["shared".into()]),
                        revision: 0,
                    })),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        for (id, cleanup, remove_worktree) in [
            ("shared", false, false),
            ("remove", true, true),
            ("keep", true, false),
        ] {
            cx.update(|window, cx| {
                page.update(cx, |page, cx| {
                    page.confirm_delete_archived(id, id, window, cx)
                });
                _ = window.draw(cx);
                window.dispatch_action(
                    Box::new(gpui_base::actions::Confirm { secondary: false }),
                    cx,
                );
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
            if cleanup {
                assert!(outgoing.is_empty(), "delete waits for the cleanup choice");
                cx.update(|window, cx| {
                    if remove_worktree {
                        window.dispatch_action(
                            Box::new(gpui_base::actions::Confirm { secondary: false }),
                            cx,
                        );
                    } else {
                        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
                    }
                });
                cx.run_until_parked();
            }
            let request =
                decode_client_line(&outgoing.try_recv().expect("delete command")).unwrap();
            assert_eq!(
                request.payload,
                ClientPayload::Command(Command::DeleteSession {
                    session_id: id.into(),
                    remove_worktree,
                })
            );
            incoming
                .try_send(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: Ok(tcode_protocol::CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            cx.run_until_parked();
        }
    }

    /// A client attached to another machine shows that machine's storage
    /// migration in Maintenance only while the host says one runs: nothing
    /// before, a card while it runs, nothing once it is done; and it stops
    /// following once Maintenance closes.
    #[gpui::test]
    fn maintenance_shows_the_hosts_storage_migration_only_while_it_runs(cx: &mut TestAppContext) {
        use tcode_protocol::{
            ClientPayload, EventEnvelope, HostMessage, ServerEvent, StorageMigration, Subscription,
            Topic, decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let attachment = WorkspaceAttachment::Remote {
            host_id: "studio".into(),
            host_name: "Studio".into(),
        };
        let (store, outgoing, incoming, _pump) = scripted_store(attachment, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        let follows = |outgoing: &async_channel::Receiver<String>, subscribe: bool| {
            std::iter::from_fn(|| outgoing.try_recv().ok()).find_map(|line| {
                let message = decode_client_line(&line).unwrap();
                match message.payload {
                    ClientPayload::Subscribe(Subscription {
                        topic: Topic::StorageMigration,
                        ..
                    }) if subscribe => Some(message.id),
                    ClientPayload::Unsubscribe(Subscription {
                        topic: Topic::StorageMigration,
                        ..
                    }) if !subscribe => Some(message.id),
                    _ => None,
                }
            })
        };
        let send = |request_id, migration| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id,
                        topic: Topic::StorageMigration,
                        event: ServerEvent::StorageMigrationReplaced(migration),
                    }))
                    .unwrap(),
                )
                .unwrap();
        };
        let shown = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
            cx.debug_bounds("storage-migration").is_some()
        };

        page.update(cx, |page, cx| page.select_section(Section::Maintenance, cx));
        cx.run_until_parked();
        let request = follows(&outgoing, true).expect("Maintenance follows the migration");
        send(Some(request), None);
        assert!(!shown(cx), "no card while no migration runs");

        send(
            None,
            Some(StorageMigration {
                moved: 30,
                total: 120,
                waiting_for_space: false,
            }),
        );
        assert!(shown(cx), "a card while the host moves threads");

        send(None, None);
        assert!(!shown(cx), "the card leaves once the move is done");

        cx.update(|window, _| window.activate_window());
        page.update(cx, |page, cx| page.select_section(Section::General, cx));
        cx.run_until_parked();
        assert!(
            follows(&outgoing, false).is_some(),
            "only Maintenance follows it"
        );
    }

    /// Maintenance on a client attached to another machine shows that
    /// machine's disk usage as its host measured it: a compact loading state
    /// before the first measurement, then Tcode's total and a row for each
    /// provider the host listed, which stay while the host measures again.
    #[gpui::test]
    fn maintenance_shows_the_hosts_disk_usage_and_keeps_it_while_measuring_again(
        cx: &mut TestAppContext,
    ) {
        use agent::ProviderKind;
        use tcode_protocol::{
            AccountDiskUsage, ClientPayload, DiskUsage, DiskUsageState, EventEnvelope, HostMessage,
            ProviderDiskUsage, ServerEvent, Subscription, TcodeDiskUsage, Topic,
            decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let attachment = WorkspaceAttachment::Remote {
            host_id: "studio".into(),
            host_name: "Studio".into(),
        };
        let (store, outgoing, incoming, _pump) = scripted_store(attachment, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        // The spinner of a measurement running rests only with motion reduced.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        let send = |request_id, state| {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id,
                        topic: Topic::DiskUsage,
                        event: ServerEvent::DiskUsageReplaced(state),
                    }))
                    .unwrap(),
                )
                .unwrap();
        };
        let shown = |cx: &mut VisualTestContext, selectors: &[&'static str]| {
            cx.run_until_parked();
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
            selectors
                .iter()
                .filter(|selector| cx.debug_bounds(selector).is_some())
                .copied()
                .collect::<Vec<_>>()
        };
        let account = |profile_id: &str, dir: &str, bytes| AccountDiskUsage {
            profile_id: profile_id.into(),
            dirs: vec![dir.into()],
            bytes,
            parts: Vec::new(),
        };
        let usage = DiskUsage {
            measured_at: 0,
            tcode: TcodeDiskUsage {
                dir: "~/.tcode".into(),
                conversations: 40 << 30,
                images: 70 << 20,
                worktrees: 0,
                other: 5 << 20,
            },
            providers: vec![
                ProviderDiskUsage {
                    provider: ProviderKind::Codex,
                    bytes: 30 << 30,
                    accounts: vec![account("codex", "~/.codex", 30 << 30)],
                },
                ProviderDiskUsage {
                    provider: ProviderKind::ClaudeCode,
                    bytes: 6 << 30,
                    accounts: vec![
                        account("claude", "~/.claude", 6 << 30),
                        account("work", "~/.tcode/profile-homes/work", 10 << 20),
                    ],
                },
            ],
        };
        let selectors = [
            "disk-usage-measuring",
            "disk-usage-remeasuring",
            "disk-usage-tcode",
            "disk-usage-tcode-conversations",
            "disk-usage-tcode-worktrees",
            "disk-usage-provider-codex",
            "disk-usage-provider-claude",
            "disk-usage-provider-pi",
        ];

        page.update(cx, |page, cx| page.select_section(Section::Maintenance, cx));
        cx.run_until_parked();
        let request = std::iter::from_fn(|| outgoing.try_recv().ok())
            .find_map(|line| match decode_client_line(&line).unwrap() {
                message @ tcode_protocol::ClientMessage {
                    payload:
                        ClientPayload::Subscribe(Subscription {
                            topic: Topic::DiskUsage,
                            ..
                        }),
                    ..
                } => Some(message.id),
                _ => None,
            })
            .expect("Maintenance follows the disk usage");
        send(
            Some(request),
            DiskUsageState {
                last: None,
                measuring: true,
            },
        );
        assert_eq!(shown(cx, &selectors), ["disk-usage-measuring"]);

        send(
            None,
            DiskUsageState {
                last: Some(usage.clone()),
                measuring: false,
            },
        );
        let rows = [
            "disk-usage-tcode",
            "disk-usage-tcode-conversations",
            "disk-usage-tcode-worktrees",
            "disk-usage-provider-codex",
            "disk-usage-provider-claude",
        ];
        assert_eq!(shown(cx, &selectors), rows);

        send(
            None,
            DiskUsageState {
                last: Some(usage),
                measuring: true,
            },
        );
        assert_eq!(
            shown(cx, &selectors),
            [&["disk-usage-remeasuring"][..], &rows[..]].concat()
        );
    }

    struct CompactSettingsProbe {
        page: Entity<SettingsPage>,
    }

    impl Render for CompactSettingsProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.page
                .update(cx, |page, cx| page.render_compact_list(cx))
        }
    }

    #[gpui::test]
    fn compact_list_groups_sections_omits_inapplicable_and_folds_advanced(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
        let root = crate::store::tests::scratch_root("settings-sections");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn settings test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::default().with_compact(true));
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let page = cx.new(|cx| {
                let mut page = SettingsPage::new(store.clone(), window_state.clone(), window, cx);
                page.capabilities = capabilities(false, false, false, true);
                page
            });
            CompactSettingsProbe { page }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(393.), px(852.)));
        // The navigation consumes capability values supplied by platform
        // owners. Exercise each policy row through its rendered section list;
        // local permission eligibility is a policy check, not an OS grant test.
        // `machine_spaces`: a desktop over another machine manages that
        // machine's shared spaces, whether or not it hosts itself.
        for (capabilities, local_permissions, folded, browser, remote, machine_spaces) in [
            (
                capabilities(true, true, true, false),
                true,
                false,
                true,
                true,
                false,
            ),
            (
                capabilities(true, true, true, true),
                false,
                false,
                true,
                true,
                true,
            ),
            (
                capabilities(false, false, false, true),
                false,
                true,
                false,
                false,
                true,
            ),
        ] {
            probe.update(cx, |probe, cx| {
                probe.page.update(cx, |page, cx| {
                    page.capabilities = capabilities;
                    page.advanced_expanded = None;
                    assert_eq!(
                        page.capabilities.can_manage_local_permissions(),
                        local_permissions
                    );
                    cx.notify();
                });
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
            for selector in [
                "settings-device-caption",
                "settings-machine-caption",
                "settings-nav-general",
                "settings-nav-threads",
                "settings-nav-providers",
                "settings-nav-usage",
                "settings-nav-archived",
            ] {
                assert!(
                    cx.debug_bounds(selector).is_some(),
                    "{capabilities:?}: {selector}"
                );
            }
            assert_eq!(cx.debug_bounds("settings-nav-browser").is_some(), browser);
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            assert_eq!(cx.debug_bounds("settings-nav-remote").is_some(), remote);
            #[cfg(not(any(feature = "remote-hosting", target_family = "wasm")))]
            let _ = remote;
            #[cfg(feature = "remote-hosting")]
            assert_eq!(
                cx.debug_bounds("settings-nav-machine-spaces").is_some(),
                machine_spaces,
                "{capabilities:?}"
            );
            #[cfg(not(feature = "remote-hosting"))]
            let _ = machine_spaces;
            assert!(
                cx.debug_bounds("settings-advanced-toggle").is_some(),
                "{capabilities:?}: advanced disclosure remains available in either state"
            );
            for advanced in ["settings-nav-computer-use", "settings-nav-maintenance"] {
                assert_eq!(cx.debug_bounds(advanced).is_some(), !folded, "{advanced}");
            }
            if folded {
                let toggle = cx.debug_bounds("settings-advanced-toggle").unwrap();
                cx.simulate_click(toggle.center(), gpui::Modifiers::default());
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.refresh();
                    _ = window.draw(cx);
                });
                assert!(cx.debug_bounds("settings-nav-computer-use").is_some());
                assert!(cx.debug_bounds("settings-nav-maintenance").is_some());
                assert!(cx.debug_bounds("settings-nav-browser").is_none());
            }
        }

        // A stale palette/deep-link target is rejected by the same predicate
        // and returns a compact detail route to the root list.
        window_state.update(cx, |state, cx| {
            state.enter_workspace(cx);
            state.open_settings(cx);
            state.go(crate::window_state::Destination::SettingsSection, cx);
        });
        probe.update(cx, |probe, cx| {
            probe
                .page
                .update(cx, |page, cx| page.open_section("browser", cx))
        });
        cx.run_until_parked();
        assert_eq!(
            window_state.read_with(cx, |state, _| state.destination()),
            crate::window_state::Destination::Settings
        );

        driver.shutdown(cx, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// "Show more" asks for a longer newest-first list, and a new filter asks
    /// for the first page of what it matches: the two the host answers.
    #[gpui::test]
    fn the_activity_list_pages_and_filters_through_the_host(cx: &mut TestAppContext) {
        use tcode_protocol::{
            ClientPayload, ControlActivityEntry, ControlActivityFilter, ControlActivityPage,
            ControlActivityThread, HostMessage, Query, QueryResponse, decode_client_line,
            encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        cx.simulate_resize(gpui::size(px(1024.), px(3000.)));
        let asked = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    let message = decode_client_line(&line).unwrap();
                    match message.payload {
                        ClientPayload::Query(Query::ControlActivity { limit, filter }) => {
                            Some((message.id, limit, filter))
                        }
                        _ => None,
                    }
                })
                .last()
                .expect("the page asked the host")
        };
        let answer = |id, total, cx: &mut VisualTestContext| {
            let entry = ControlActivityEntry {
                target_thread_id: "target".into(),
                target_title: "Target".into(),
                at: 1,
                action: agent::ControlAction {
                    actor_thread_id: "actor".into(),
                    actor_title: "Actor".into(),
                    action: "send_message".into(),
                    detail: None,
                },
            };
            incoming
                .try_send(
                    encode_line(&HostMessage::QueryResult {
                        id,
                        result: Ok(QueryResponse::ControlActivity(ControlActivityPage {
                            entries: vec![entry],
                            total,
                            threads: vec![ControlActivityThread {
                                id: "target".into(),
                                title: "Target".into(),
                            }],
                            actions: vec!["send_message".into()],
                        })),
                    })
                    .unwrap(),
                )
                .unwrap();
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
        };

        page.update(cx, |page, cx| page.select_section(Section::Control, cx));
        crate::test_frames::draw(cx);
        let (id, limit, filter) = asked(cx);
        assert_eq!((limit, &filter), (50, &ControlActivityFilter::default()));

        answer(id, 1, cx);
        assert!(
            cx.debug_bounds("control-activity-more").is_none(),
            "every match is listed"
        );

        let refresh = cx.debug_bounds("control-activity-refresh").unwrap();
        cx.simulate_click(refresh.center(), Default::default());
        let (id, ..) = asked(cx);
        answer(id, 60, cx);
        let more = cx
            .debug_bounds("control-activity-more")
            .expect("more match");
        cx.simulate_click(more.center(), Default::default());
        let (id, limit, _) = asked(cx);
        assert_eq!(limit, 100);
        answer(id, 60, cx);

        let thread = cx.debug_bounds("control-activity-thread").unwrap();
        cx.simulate_click(thread.center(), Default::default());
        crate::test_frames::draw_settled(cx);
        let option = cx
            .debug_bounds("control-activity-thread-Target")
            .expect("the log's threads are offered");
        cx.simulate_click(option.center(), Default::default());
        let (_, limit, filter) = asked(cx);
        assert_eq!(limit, 50, "a filter starts over at the first page");
        assert_eq!(filter.thread_id.as_deref(), Some("target"));
    }

    /// A client that renders before its host answers must not present its own
    /// defaults as the host's configuration, must adopt the host's values the
    /// moment they arrive, and must never overwrite what the user has since
    /// typed — no matter how many snapshots follow.
    #[gpui::test]
    fn editable_settings_wait_for_the_host_and_then_keep_the_user_edit(cx: &mut TestAppContext) {
        let root = crate::store::tests::scratch_root("settings-hydration");
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = tcode_core::settings::Settings::default();
        settings.browser.home_url = Some("https://host.example".into());
        tcode_services::settings::SettingsStore::new(root.clone())
            .save(&settings)
            .expect("seed host settings");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn settings test host");

        // No blocking seed: this is the portable client's timeline, where the
        // first snapshot lands after the page already exists.
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::default());
        let (page, cx) = cx.add_window_view(|window, cx| {
            SettingsPage::new(store.clone(), window_state.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        page.read_with(cx, |page, cx| {
            assert_eq!(
                page.home_url_input.state.read(cx).value(),
                "",
                "an unhydrated field must stay empty rather than show a local default"
            );
        });

        driver.wait_until(cx, "the field to show the host's value", |cx| {
            page.read_with(cx, |page, cx| {
                page.home_url_input.state.read(cx).value() == "https://host.example"
            })
        });

        // The user edits the field: `replace_all` takes the same path typing
        // does, emitting the change the page listens for.
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.home_url_input.state.update(cx, |input, cx| {
                    input.replace_all("https://edited", window, cx)
                });
            });
        });
        cx.run_until_parked();

        let link = driver.link.clone();
        driver
            .await_with_events(
                cx,
                link.command(tcode_protocol::Command::PatchSettings {
                    patch: SettingsPatch::BrowserHomeUrl(Some("https://elsewhere.example".into())),
                }),
            )
            .expect("patch settings");
        driver.wait_until(cx, "the later settings snapshot", |cx| {
            store.read_with(cx, |store, _| {
                store.settings().browser.home_url.as_deref() == Some("https://elsewhere.example")
            })
        });

        page.read_with(cx, |page, cx| {
            assert_eq!(
                page.home_url_input.state.read(cx).value(),
                "https://edited",
                "a later snapshot must not clobber the field the user is editing"
            );
        });

        driver.shutdown(cx, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The new project folder's placeholder is written under the host's
    /// home, which the page asks the host for; a failed ask says why and is
    /// not made again with every drawing.
    #[gpui::test]
    fn a_failed_ask_for_the_host_home_says_why_once(cx: &mut TestAppContext) {
        use tcode_protocol::{ClientPayload, HostMessage, Query, decode_client_line, encode_line};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        let (store, outgoing, incoming, _pump) = scripted_store(WorkspaceAttachment::Local, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        let reported = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|_, cx| {
            let reported = reported.clone();
            cx.subscribe(
                &store,
                move |_, event: &tcode_protocol::RuntimeNotification, _| {
                    reported.borrow_mut().push(event.clone());
                },
            )
            .detach();
        });
        let asked_home = |outgoing: &async_channel::Receiver<String>| {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    let message = decode_client_line(&line).unwrap();
                    match message.payload {
                        ClientPayload::Query(Query::BrowseDirectories { path }) if path == "~" => {
                            Some(message.id)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let answer = |id, result| {
            incoming
                .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
                .unwrap();
        };
        let drawn = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.run_until_parked();
            crate::test_frames::draw_settled(cx);
            page.read_with(cx, |page, cx| {
                page.new_project_dir_input
                    .state
                    .read(cx)
                    .presentation()
                    .placeholder()
                    .to_string()
            })
        };

        page.update(cx, |page, cx| page.select_section(Section::Threads, cx));
        drawn(cx);
        let [first] = asked_home(&outgoing)[..] else {
            panic!("drawing the Projects rows asks for the host's home once");
        };
        answer(
            first,
            Err(tcode_protocol::ProtocolError {
                code: "transport_closed".into(),
                message: "The host went away.".into(),
            }),
        );
        assert_eq!(drawn(cx), "~/TcodeProjects");
        assert_eq!(drawn(cx), "~/TcodeProjects");
        assert!(
            asked_home(&outgoing).is_empty(),
            "drawing does not ask again"
        );
        assert_eq!(
            reported.borrow().as_slice(),
            [tcode_protocol::RuntimeNotification::Error(
                tcode_protocol::RuntimeError::External(
                    crate::tr!("sidebar.folders_failed", reason = "The host went away.")
                        .into_owned()
                )
            )]
        );
    }

    /// The attachment limit is taken when the user leaves the field. A value
    /// it cannot take is not saved, and the field shows the limit in effect
    /// rather than what was typed.
    #[gpui::test]
    fn an_attachment_limit_the_field_cannot_take_shows_the_one_in_effect(cx: &mut TestAppContext) {
        use tcode_client::host::ClientHost as _;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::test_client::init);
        let root = crate::store::tests::scratch_root("attachment-limit-field");
        std::fs::create_dir_all(&root).unwrap();
        let device = std::rc::Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        cx.update(|cx| DevicePreferences::install(device.clone(), cx));
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        cx.update(|window, _| window.activate_window());
        page.update(cx, |page, cx| page.select_section(Section::General, cx));
        crate::test_frames::draw_settled(cx);
        assert!(
            cx.debug_bounds("settings-remote-attachment-limit-row")
                .is_some()
        );
        let in_effect = cx.update(|_, cx| remote_attachment_limit_value(cx));
        let field = page.read_with(cx, |page, _| {
            page.remote_attachment_limit_input.state.clone()
        });
        // What the user types after clearing the field, then leaving it.
        let type_and_leave = |typed: &str, cx: &mut gpui::VisualTestContext| {
            let row = cx
                .debug_bounds("settings-remote-attachment-limit-row")
                .expect("the limit's row is drawn");
            cx.simulate_click(
                gpui::point(row.right() - gpui::px(40.), row.center().y),
                gpui::Modifiers::default(),
            );
            crate::test_frames::draw(cx);
            cx.simulate_keystrokes("end backspace backspace backspace");
            cx.simulate_input(typed);
            crate::test_frames::draw(cx);
            cx.update(|window, cx| window.blur(cx));
            crate::test_frames::draw_settled(cx);
            (
                device
                    .load_preferences()
                    .unwrap()
                    .remote_attachment_limit_mib,
                cx.update(|_, cx| field.read(cx).value().to_string()),
            )
        };
        assert_eq!(type_and_leave("", cx), (None, in_effect));
        assert_eq!(type_and_leave("12", cx), (Some(12), "12".to_owned()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_device_name_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::test_client::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::default());
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        crate::settings::assert_follows_language_switch(
            cx,
            "settings.device_name.placeholder",
            |cx| {
                page.read_with(cx, |page, cx| {
                    let input = page.device_name_input.state.read(cx);
                    input.presentation().placeholder().to_string()
                })
            },
        );
    }
}
