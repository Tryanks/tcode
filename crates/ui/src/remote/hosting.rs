//! Hosting this machine: the Traverse endpoint, its settings and the devices
//! that have paired with it. The invitation those settings mint is shown on
//! the Hosts page, where connections are made; `invitation_card` draws it.
//!
//! [`RemoteController`] is the process-wide handle the composition root installs.
//! It owns the local [`HostMux`] and endpoint independently of whichever host
//! the window is currently attached to, so **Connect** and **Back to local**
//! never stop it and never disturb another attached client.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, ClipboardItem, Context, Entity,
    Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_client::HostLink;
use tcode_core::settings::{Settings, TraverseInstance, TraverseSetting, TraverseSource};
use tcode_protocol::{Command, DeviceAccess, HostingAction, HostingState, SettingsPatch};
use tcode_traverse::manifest::ManifestSource;
use tcode_traverse::{DeviceInfo, HostConfig, HostMux, Invitation, TraverseHost};

use super::qr::beside_qr;
use super::spaces::SpacesSection;
use crate::icon::IconName;
use crate::overlay::{Notification, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;

/// How often the devices list re-reads which path each connection is on.
const DEVICE_REFRESH: Duration = Duration::from_secs(2);

/// A caption above one group of this settings-like page. Grouped cards, not
/// the plain content-list rows Machines uses.
fn section_caption(label: SharedString, cx: &App) -> AnyElement {
    div()
        .pl_3()
        .pb(px(6.))
        .text_size(px(11.))
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(label)
        .into_any_element()
}

/// A line of explanation inside a group.
fn note(text: SharedString, cx: &App) -> AnyElement {
    div()
        .w_full()
        .px_3()
        .py_3()
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

pub struct RemoteController {
    mux: HostMux,
    host: Option<TraverseHost>,
    data_dir: PathBuf,
    local_settings_link: HostLink,
    local_settings: Settings,
}

impl Global for RemoteController {}

impl RemoteController {
    pub fn new(
        mux: HostMux,
        data_dir: PathBuf,
        local_settings_link: HostLink,
        local_settings: Settings,
    ) -> Self {
        Self {
            mux,
            host: None,
            data_dir,
            local_settings_link,
            local_settings,
        }
    }

    pub fn local_settings(&self) -> &Settings {
        &self.local_settings
    }

    pub fn save_hosting_settings(
        &mut self,
        enabled: bool,
        traverse: TraverseSetting,
        name: Option<String>,
    ) {
        self.local_settings.remote_hosting_enabled = enabled;
        self.local_settings.traverse = traverse.clone();
        self.local_settings.remote_host_name = name.clone();
        for patch in [
            SettingsPatch::RemoteHostingEnabled(enabled),
            SettingsPatch::Traverse(traverse),
            SettingsPatch::RemoteHostName(name),
        ] {
            if let Err(error) = self
                .local_settings_link
                .dispatch(Command::PatchSettings { patch })
            {
                log::error!(
                    "could not persist local hosting settings: {}",
                    error.message
                );
            }
        }
    }

    pub fn is_hosting(&self) -> bool {
        self.host.is_some()
    }

    /// This machine's id while hosting.
    pub fn endpoint_id(&self) -> Option<String> {
        self.host.as_ref().map(TraverseHost::endpoint_id)
    }

    /// Bind the endpoint, publish to `traverse` and mint a first invitation.
    pub fn start_hosting(
        &mut self,
        traverse: &TraverseSetting,
        host_name: String,
    ) -> Result<(), String> {
        if self.host.is_some() {
            return Ok(());
        }
        let host = TraverseHost::start(
            self.mux.clone(),
            HostConfig {
                host_name,
                data_dir: self.data_dir.clone(),
                traverse: traverse_sources(traverse)?,
                pairing_enabled: true,
                // Fixed, so invite addresses, firewall rules and LAN probes
                // survive restarts.
                bind_port: Some(tcode_traverse::lan::DEFAULT_PORT),
            },
        )
        .map_err(|error| error.to_string())?;
        if host.pairing_enabled() {
            host.new_invitation();
        }
        self.host = Some(host);
        Ok(())
    }

    /// Adopt a host started elsewhere: tests bind a random port, since the
    /// desktop's fixed one may be taken on the machine running them.
    #[cfg(test)]
    pub(crate) fn adopt_host(&mut self, host: TraverseHost) {
        self.host = Some(host);
    }

    pub fn stop_hosting(&mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
    }

    pub fn new_invitation(&mut self) {
        if let Some(host) = self.host.as_ref() {
            host.new_invitation();
        }
    }

    /// Whether new devices can pair while hosting. The transport persists
    /// the choice, so it outlives a restart and applies to headless too.
    pub fn pairing_enabled(&self) -> bool {
        self.host
            .as_ref()
            .is_some_and(TraverseHost::pairing_enabled)
    }

    /// Turning pairing on mints an invitation at once; turning it off drops
    /// the active one, and paired devices keep working.
    pub fn set_pairing_enabled(&self, enabled: bool) {
        if let Some(host) = self.host.as_ref() {
            host.set_pairing_enabled(enabled);
            if enabled {
                host.new_invitation();
            }
        }
    }

    /// The active invitation with its remaining lifetime in seconds, or
    /// `None` once it has expired. It carries where this machine is
    /// reachable *now*: an invitation is minted the moment hosting starts,
    /// before the endpoint has found its home relay, so the QR is composed
    /// at paint time from [`TraverseHost::invitation`], as the hosting query
    /// answers a remote client.
    pub fn invitation(&self) -> Option<(Invitation, u64)> {
        let (invitation, remaining) = self.host.as_ref()?.invitation()?;
        (remaining.as_secs() > 0).then_some((invitation, remaining.as_secs()))
    }

    /// What the Hosts page can offer another device right now.
    pub fn invitation_offer(&self) -> InvitationOffer {
        if !self.is_hosting() {
            return InvitationOffer::NotHosting;
        }
        if !self.pairing_enabled() {
            return InvitationOffer::PairingOff;
        }
        match self.invitation() {
            Some((invitation, remaining)) => InvitationOffer::Live {
                invitation,
                remaining,
            },
            None => InvitationOffer::Expired,
        }
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.host
            .as_ref()
            .map(TraverseHost::devices)
            .unwrap_or_default()
    }

    /// Run a hosting action against this machine, as a remote client's
    /// `Query::Hosting` would.
    pub fn hosting(&self, action: HostingAction) -> Result<HostingState, String> {
        self.host
            .as_ref()
            .ok_or_else(|| crate::tr!("spaces.not_hosting").into_owned())?
            .hosting(action)
            .map_err(|error| error.message)
    }

    /// Remove a device. When the allow list cannot be written the device
    /// stays paired and connected, and the error says so.
    pub fn revoke_device(&self, id: &str) -> Result<(), String> {
        match self.host.as_ref() {
            Some(host) => host.revoke(id).map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }
}

/// Whether this machine has an invitation to show, and if not, why: the
/// Hosts page names the setting that would change that.
#[derive(Debug, Clone, PartialEq)]
pub enum InvitationOffer {
    NotHosting,
    PairingOff,
    Expired,
    Live {
        invitation: Invitation,
        /// Seconds left.
        remaining: u64,
    },
}

/// The transport's view of a Traverse setting: its enabled sources. A
/// self-hosted instance needs a usable base URL; the page validates it
/// before saving, so a failure here comes from a settings file edited by
/// hand.
fn traverse_sources(setting: &TraverseSetting) -> Result<Vec<ManifestSource>, String> {
    setting
        .enabled()
        .map(|instance| match instance {
            TraverseInstance::Official => Ok(ManifestSource::Official),
            TraverseInstance::Custom { url } => custom_traverse_url(url)
                .map(ManifestSource::Custom)
                .ok_or_else(|| crate::tr!("remote.traverse.invalid_url").into_owned()),
        })
        .collect()
}

/// A self-hosted Traverse base URL as typed: `http(s)` with a host.
fn custom_traverse_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(url)
}

/// This machine's default advertised host name.
pub fn machine_name() -> String {
    tcode_traverse::native_host::default_device_name()
}

/// One hosting settings row: label and description left, control right. A
/// compact page has no room for two columns — the description would be squeezed
/// to a word per line — so it puts the control full width underneath the text,
/// which is the same rule `SettingsPage::row_frame` applies.
fn row(compact: bool) -> gpui::Div {
    if compact {
        v_flex()
            .w_full()
            .min_h(px(44.))
            .px_3()
            .py_2p5()
            .gap_2()
            .items_start()
    } else {
        switch_row()
    }
}

/// A row whose control is a fixed 44pt affordance (a switch): it never squeezes
/// the label, so it stays beside it at both widths.
fn switch_row() -> gpui::Div {
    h_flex()
        .w_full()
        .min_h(px(44.))
        .px_3()
        .py_2p5()
        .gap_3()
        .items_center()
}

/// A row's title, with a description only where it says something the
/// title and the control do not.
fn labels(title: SharedString, description: Option<SharedString>, cx: &App) -> gpui::Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(div().text_size(px(15.)).font_medium().child(title))
        .children(description.map(|description| {
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(description)
        }))
}

fn countdown(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// One Traverse source as the page edits it.
struct TraverseRow {
    enabled: bool,
    /// A self-hosted instance's base URL as typed; `None` is the official
    /// service.
    url: Option<(Entity<InputState>, gpui::Subscription)>,
}

/// Settings → Remote: the editable hosting controls for *this machine*.
/// Their live state lives in the process-wide [`RemoteController`]; only the
/// in-progress edits belong here.
pub struct HostingPanel {
    host_name_input: Entity<InputState>,
    /// The Traverse sources in order, the official service first.
    traverse_rows: Vec<TraverseRow>,
    /// The URL of a self-hosted instance about to be added.
    traverse_add_input: Entity<InputState>,
    /// Repaint while hosting, every [`DEVICE_REFRESH`], for the devices' paths.
    ticker: Option<Task<()>>,
    spaces: Entity<SpacesSection>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl HostingPanel {
    /// `store` is the window's attachment: it names this machine's projects
    /// while the window shows this machine.
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = cx
            .try_global::<RemoteController>()
            .map(RemoteController::local_settings)
            .cloned()
            .unwrap_or_default();
        let host_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(machine_name())
                .default_value(settings.remote_host_name.clone().unwrap_or_default())
        });
        let traverse_rows = settings
            .traverse
            .sources
            .iter()
            .map(|source| TraverseRow {
                enabled: source.enabled,
                url: match &source.instance {
                    TraverseInstance::Official => None,
                    TraverseInstance::Custom { url } => Some(url_input(url, window, cx)),
                },
            })
            .collect();
        let traverse_add_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::tr!("remote.traverse.url_placeholder").into_owned())
        });
        let subscriptions = vec![
            edit_on_leave(&host_name_input, window, cx),
            // Enter adds the typed instance; the field repaints as the URL
            // turns valid.
            cx.subscribe_in(
                &traverse_add_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.add_traverse(window, cx),
                    InputEvent::Change => cx.notify(),
                    InputEvent::Blur | InputEvent::Focus => {}
                },
            ),
        ];
        Self {
            host_name_input,
            traverse_rows,
            traverse_add_input,
            ticker: None,
            spaces: cx.new(|_| SpacesSection::new(store, super::spaces::this_machine)),
            _subscriptions: subscriptions,
        }
    }
}

/// A typed name or URL takes effect when the field is left or Enter is
/// pressed; the field also repaints as it turns valid.
fn edit_on_leave(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<HostingPanel>,
) -> gpui::Subscription {
    cx.subscribe_in(
        input,
        window,
        |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Blur | InputEvent::PressEnter { .. } => this.apply_edits(window, cx),
            InputEvent::Change => cx.notify(),
            InputEvent::Focus => {}
        },
    )
}

/// The field holding a self-hosted instance's URL, with its subscription.
fn url_input(
    url: &str,
    window: &mut Window,
    cx: &mut Context<HostingPanel>,
) -> (Entity<InputState>, gpui::Subscription) {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(crate::tr!("remote.traverse.url_placeholder").into_owned())
            .default_value(url.to_owned())
    });
    let subscription = edit_on_leave(&input, window, cx);
    (input, subscription)
}

impl HostingPanel {
    /// Run the repaint loop exactly while hosting.
    fn sync_ticker(&mut self, cx: &mut Context<Self>) {
        let hosting = cx
            .try_global::<RemoteController>()
            .is_some_and(RemoteController::is_hosting);
        match (hosting, self.ticker.is_some()) {
            (true, false) => {
                self.ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(DEVICE_REFRESH).await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                }));
            }
            (false, true) => self.ticker = None,
            _ => {}
        }
    }

    fn typed_host_name(&self, cx: &App) -> String {
        self.host_name_input.read(cx).value().trim().to_owned()
    }

    /// The self-hosted URLs as typed, one per row (`None` for the official
    /// service), each a usable base URL or why it is not one.
    fn typed_urls(&self, cx: &App) -> Vec<Option<Result<url::Url, SharedString>>> {
        let mut seen = Vec::new();
        self.traverse_rows
            .iter()
            .map(|row| {
                let (input, _) = row.url.as_ref()?;
                Some(
                    match custom_traverse_url(&input.read(cx).value()) {
                        None => Err(crate::tr!("remote.traverse.invalid_url")),
                        Some(url) if seen.contains(&url) => {
                            Err(crate::tr!("remote.traverse.duplicate"))
                        }
                        Some(url) => {
                            seen.push(url.clone());
                            Ok(url)
                        }
                    }
                    .map_err(|error| error.into_owned().into()),
                )
            })
            .collect()
    }

    /// The Traverse setting as edited, or `None` while a self-hosted URL is
    /// not a usable one.
    fn typed_traverse(&self, cx: &App) -> Option<TraverseSetting> {
        let sources = self
            .traverse_rows
            .iter()
            .zip(self.typed_urls(cx))
            .map(|(row, url)| {
                Some(TraverseSource {
                    instance: match url {
                        None => TraverseInstance::Official,
                        Some(url) => TraverseInstance::Custom {
                            url: url.ok()?.to_string(),
                        },
                    },
                    enabled: row.enabled,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(TraverseSetting::new(sources))
    }

    /// The instance typed into the add field, or why it cannot be added;
    /// `None` while the field is empty.
    fn typed_addition(&self, cx: &App) -> Option<Result<url::Url, SharedString>> {
        let value = self.traverse_add_input.read(cx).value();
        if value.trim().is_empty() {
            return None;
        }
        Some(match custom_traverse_url(&value) {
            None => Err(crate::tr!("remote.traverse.invalid_url")
                .into_owned()
                .into()),
            Some(url)
                if self
                    .typed_urls(cx)
                    .into_iter()
                    .flatten()
                    .any(|typed| typed.as_ref() == Ok(&url)) =>
            {
                Err(crate::tr!("remote.traverse.duplicate").into_owned().into())
            }
            Some(url) => Ok(url),
        })
    }

    /// Whether the edits differ from the saved settings and are complete.
    fn edits_pending(&self, cx: &App) -> bool {
        let Some(controller) = cx.try_global::<RemoteController>() else {
            return false;
        };
        let saved = controller.local_settings();
        let typed_name = self.typed_host_name(cx);
        let name_changed = saved.remote_host_name.clone().unwrap_or_default() != typed_name;
        match self.typed_traverse(cx) {
            Some(traverse) => name_changed || traverse != saved.traverse,
            None => false,
        }
    }

    fn set_hosting(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(traverse) = self.typed_traverse(cx) else {
            window.push_notification(
                Notification::error(crate::tr!("remote.traverse.invalid_url").into_owned()),
                cx,
            );
            return;
        };
        let typed_name = self.typed_host_name(cx);
        let name = if typed_name.is_empty() {
            machine_name()
        } else {
            typed_name.clone()
        };
        let mut failure = None;
        cx.update_global::<RemoteController, _>(|controller, _| {
            if enabled {
                if let Err(error) = controller.start_hosting(&traverse, name) {
                    failure = Some(error);
                }
            } else {
                controller.stop_hosting();
            }
            if failure.is_none() {
                controller.save_hosting_settings(
                    enabled,
                    traverse.clone(),
                    (!typed_name.is_empty()).then_some(typed_name.clone()),
                );
            }
        });
        if let Some(error) = failure {
            window.push_notification(Notification::error(error), cx);
            return;
        }
        self.sync_ticker(cx);
        cx.notify();
    }

    /// Re-bind the endpoint so an edited name or Traverse choice takes effect
    /// at once; while not hosting, just save it for the next start. Nothing
    /// happens while the edits match what is saved or are incomplete, so
    /// leaving a field untouched never restarts the host.
    fn apply_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.edits_pending(cx) {
            return;
        }
        if cx
            .try_global::<RemoteController>()
            .is_some_and(RemoteController::is_hosting)
        {
            self.set_hosting(false, window, cx);
            self.set_hosting(true, window, cx);
        } else {
            let Some(traverse) = self.typed_traverse(cx) else {
                return;
            };
            let typed_name = self.typed_host_name(cx);
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller.save_hosting_settings(
                    false,
                    traverse,
                    (!typed_name.is_empty()).then_some(typed_name),
                );
            });
            cx.notify();
        }
    }

    fn set_pairing_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        cx.update_global::<RemoteController, _>(|controller, _| {
            controller.set_pairing_enabled(enabled);
        });
        self.sync_ticker(cx);
        cx.notify();
    }

    /// A source's switch applies at once.
    fn set_traverse_enabled(
        &mut self,
        index: usize,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.traverse_rows.get_mut(index) {
            row.enabled = enabled;
        }
        self.apply_edits(window, cx);
        cx.notify();
    }

    /// Remove a self-hosted instance; the official service is only ever
    /// switched off.
    fn remove_traverse(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .traverse_rows
            .get(index)
            .is_some_and(|row| row.url.is_some())
        {
            self.traverse_rows.remove(index);
        }
        self.apply_edits(window, cx);
        cx.notify();
    }

    /// Add the typed instance, enabled, and apply it at once.
    fn add_traverse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(url)) = self.typed_addition(cx) else {
            return;
        };
        let input = url_input(url.as_str(), window, cx);
        self.traverse_rows.push(TraverseRow {
            enabled: true,
            url: Some(input),
        });
        self.traverse_add_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.apply_edits(window, cx);
        cx.notify();
    }

    fn render_hosting(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        self.sync_ticker(cx);
        let (hosting, pairing) = cx
            .try_global::<RemoteController>()
            .map(|controller| (controller.is_hosting(), controller.pairing_enabled()))
            .unwrap_or_default();
        let toggle = switch_row()
            .child(labels(
                crate::tr!("remote.host.title").into_owned().into(),
                None,
                cx,
            ))
            .child(
                Switch::new("remote-hosting")
                    .checked(hosting)
                    .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                        this.set_hosting(*checked, window, cx);
                    })),
            )
            .into_any_element();
        let pairing_row = hosting.then(|| {
            switch_row()
                .debug_selector(|| "remote-pairing".into())
                .child(labels(
                    crate::tr!("remote.pairing.title").into_owned().into(),
                    None,
                    cx,
                ))
                .child(
                    Switch::new("remote-pairing")
                        .checked(pairing)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_pairing_enabled(*checked, cx);
                        })),
                )
                .into_any_element()
        });
        let name_row = row(compact)
            .child(labels(
                crate::tr!("remote.host_name.title").into_owned().into(),
                Some(
                    crate::tr!("remote.host_name.description")
                        .into_owned()
                        .into(),
                ),
                cx,
            ))
            .child(
                div()
                    .when(compact, |field| field.w_full())
                    .when(!compact, |field| field.w(px(240.)))
                    .child(
                        Input::new(&self.host_name_input)
                            .small()
                            .rounded(crate::material::radius_input(cx)),
                    ),
            )
            .into_any_element();
        let traverse_rows = self.render_traverse(compact, cx);
        let mut column = v_flex().w_full().gap_3().child(
            v_flex()
                .child(section_caption(
                    crate::tr!("remote.host.section").into_owned().into(),
                    cx,
                ))
                .child(
                    crate::material::group(cx)
                        .child(toggle)
                        .children(pairing_row)
                        .child(name_row)
                        .children(traverse_rows),
                ),
        );
        if hosting {
            column = column
                .child(self.render_devices(compact, cx))
                .child(self.spaces.clone());
        }
        column.into_any_element()
    }

    /// The Traverse sources, each with its switch, and the row that adds a
    /// self-hosted instance.
    fn render_traverse(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let field = |compact: bool| {
            div()
                .when(compact, |field| field.flex_1().min_w_0())
                .when(!compact, |field| field.w(px(240.)))
        };
        let mut rows = vec![
            div()
                .w_full()
                .px_3()
                .pt_3()
                .child(labels(
                    crate::tr!("remote.traverse.title").into_owned().into(),
                    Some(
                        crate::tr!("remote.traverse.description")
                            .into_owned()
                            .into(),
                    ),
                    cx,
                ))
                .into_any_element(),
        ];
        let urls = self.typed_urls(cx);
        for (index, (source, url)) in self.traverse_rows.iter().zip(urls).enumerate() {
            let switch = Switch::new(SharedString::from(format!("remote-traverse-{index}")))
                .checked(source.enabled)
                .on_click(cx.listener(move |this, checked: &bool, window, cx| {
                    this.set_traverse_enabled(index, *checked, window, cx);
                }));
            let element = match (&source.url, url) {
                (Some((input, _)), Some(url)) => {
                    let description = match url {
                        Ok(_) => crate::tr!("remote.traverse.url_description")
                            .into_owned()
                            .into(),
                        Err(error) => error,
                    };
                    let remove = Button::new(SharedString::from(format!(
                        "remote-traverse-remove-{index}"
                    )))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .aria_label(crate::tr!("remote.traverse.remove"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove_traverse(index, window, cx);
                    }));
                    row(compact)
                        .child(labels(
                            crate::tr!("remote.traverse.custom").into_owned().into(),
                            Some(description),
                            cx,
                        ))
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .when(compact, |controls| controls.w_full())
                                .child(
                                    field(compact).child(
                                        Input::new(input)
                                            .small()
                                            .rounded(crate::material::radius_input(cx)),
                                    ),
                                )
                                .child(remove)
                                .child(switch),
                        )
                }
                _ => switch_row()
                    .child(labels(
                        crate::tr!("remote.traverse.official").into_owned().into(),
                        None,
                        cx,
                    ))
                    .child(switch),
            };
            rows.push(element.into_any_element());
        }
        let addition = self.typed_addition(cx);
        let add_description = match &addition {
            Some(Err(error)) => error.clone(),
            _ => crate::tr!("remote.traverse.url_description")
                .into_owned()
                .into(),
        };
        rows.push(
            row(compact)
                .child(labels(
                    crate::tr!("remote.traverse.add_title").into_owned().into(),
                    Some(add_description),
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .when(compact, |controls| controls.w_full())
                        .child(
                            field(compact).child(
                                Input::new(&self.traverse_add_input)
                                    .small()
                                    .rounded(crate::material::radius_input(cx)),
                            ),
                        )
                        .child(
                            Button::new("remote-traverse-add")
                                .ghost()
                                .outline()
                                .compact()
                                .icon(IconName::Plus)
                                .label(crate::tr!("remote.traverse.add"))
                                .disabled(!matches!(addition, Some(Ok(_))))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_traverse(window, cx);
                                })),
                        ),
                )
                .into_any_element(),
        );
        rows
    }

    fn render_devices(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let devices = cx
            .try_global::<RemoteController>()
            .map(RemoteController::devices)
            .unwrap_or_default();
        let space_names: HashMap<String, String> = super::spaces::local(cx)
            .map(|spaces| {
                spaces
                    .read(cx)
                    .spaces()
                    .iter()
                    .map(|space| (space.id.clone(), space.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut group = crate::material::group(cx);
        if devices.is_empty() {
            group = group.child(note(
                crate::tr!("remote.devices.empty").into_owned().into(),
                cx,
            ));
        }
        for device in devices {
            let id = device.id.clone();
            let status = super::path_label(device.live.as_ref());
            let status_color = match &device.live {
                Some(_) => cx.theme().success,
                None => cx.theme().muted_foreground,
            };
            let space = match &device.access {
                DeviceAccess::Space { space_id } => space_names.get(space_id),
                DeviceAccess::Full => None,
            };
            group = group.child(
                row(compact)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(labels(
                                super::device_label(&device.name, device.platform.as_deref())
                                    .into(),
                                None,
                                cx,
                            ))
                            .children(space.map(|name| super::spaces::member_badge(name, cx))),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .when(compact, |controls| controls.w_full().justify_between())
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(status_color)
                                    .child(status),
                            )
                            .child(
                                Button::new(SharedString::from(format!("revoke-{id}")))
                                    .ghost()
                                    .compact()
                                    .danger()
                                    .label(crate::tr!("remote.devices.revoke"))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        let id = id.clone();
                                        let revoked = cx.update_global::<RemoteController, _>(
                                            |controller, _| controller.revoke_device(&id),
                                        );
                                        if let Err(error) = revoked {
                                            window.push_notification(
                                                Notification::error(
                                                    crate::tr!(
                                                        "remote.devices.revoke_failed",
                                                        error = error
                                                    )
                                                    .into_owned(),
                                                ),
                                                cx,
                                            );
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        v_flex()
            .child(section_caption(
                crate::tr!("remote.devices.section").into_owned().into(),
                cx,
            ))
            .child(group)
            .into_any_element()
    }
}

/// Mint a new invitation. The row that offers it repaints on its next tick.
fn new_invitation_button<V: 'static>(id: &'static str, cx: &mut Context<V>) -> Button {
    Button::new(id)
        .compact()
        .label(crate::tr!("remote.invite.new"))
        .on_click(cx.listener(|_, _, _, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller.new_invitation();
            });
            cx.notify();
        }))
}

/// The invitation this machine shows other devices: the QR to scan, the link
/// to copy, how long it lasts and the way to a fresh one. Drawn on the Hosts
/// page, in its plain-list vocabulary; `inset` is that page's margin.
pub(super) fn invitation_card<V: 'static>(
    invitation: &Invitation,
    remaining: u64,
    compact: bool,
    inset: f32,
    cx: &mut Context<V>,
) -> AnyElement {
    let link = invitation.url();
    let text = v_flex()
        .flex_1()
        .min_w_0()
        .gap_3()
        .child(
            div()
                .text_size(px(15.))
                .line_height(px(20.))
                .child(crate::tr!("hosts.invite.description")),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!(
                    "remote.invite.expires",
                    time = countdown(remaining)
                )),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("remote-copy-invitation")
                        .ghost()
                        .outline()
                        .compact()
                        .label(crate::tr!("remote.invite.copy"))
                        .on_click(cx.listener({
                            let link = link.clone();
                            move |_, _, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                                window.push_notification(
                                    Notification::info(
                                        crate::tr!("remote.invite.copied").into_owned(),
                                    ),
                                    cx,
                                );
                            }
                        })),
                )
                .child(
                    new_invitation_button("remote-new-invitation", cx)
                        .ghost()
                        .outline(),
                ),
        );
    beside_qr(text, &link, compact, cx)
        .px(px(inset))
        .py(px(8.))
        .debug_selector(|| "remote-invitation".into())
        .into_any_element()
}

/// The invitation ran out: say so, next to the button that mints another.
pub(super) fn expired_invitation_row<V: 'static>(inset: f32, cx: &mut Context<V>) -> AnyElement {
    h_flex()
        .w_full()
        .px(px(inset))
        .py(px(8.))
        .gap_3()
        .items_center()
        .debug_selector(|| "remote-invitation-expired".into())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(15.))
                .child(crate::tr!("remote.invite.expired")),
        )
        .child(new_invitation_button("remote-new-invitation", cx).primary())
        .into_any_element()
}

impl Render for HostingPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        div()
            .w_full()
            .min_w_0()
            .debug_selector(|| "hosting-settings".into())
            .child(self.render_hosting(compact, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    struct Probe(Entity<HostingPanel>);

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(
                self.0
                    .update(cx, |panel, cx| panel.render_hosting(false, cx)),
            )
        }
    }

    /// Edits apply themselves: a Traverse source's switch, addition or
    /// removal when it is made, a typed name when its field is left. The pairing switch exists only while
    /// hosting; flipping it reaches the transport, which drops or mints the
    /// invitation. The invitation itself is the Hosts page's to show, never
    /// this settings panel's.
    #[gpui::test]
    fn edits_apply_themselves_and_the_pairing_switch_drives_the_transport(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = std::env::temp_dir().join(format!(
            "tcode-hosting-pairing-{}",
            tcode_services::store::now_millis()
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Idle pipes: the host is never attached to here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let mux = HostMux::new(to_host.clone(), from_host.clone());
        cx.update(crate::theme::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                mux.clone(),
                root.clone(),
                HostLink::new(to_host.clone(), from_host.clone()),
                Settings::default(),
            ))
        });
        let window = cx.open_window(gpui::size(px(900.), px(700.)), |window, cx| {
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    HostLink::new(to_host.clone(), from_host.clone()),
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            Probe(cx.new(|cx| HostingPanel::new(store, window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
        };
        draw(cx);
        assert!(
            cx.debug_bounds("remote-pairing").is_none(),
            "no pairing switch while not hosting"
        );
        let panel = window.read_with(cx, |probe, _| probe.0.clone()).unwrap();
        let saved_traverse = |cx: &mut gpui::VisualTestContext| {
            cx.read(|cx| {
                serde_json::to_value(&cx.global::<RemoteController>().local_settings().traverse)
                    .unwrap()
            })
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.set_traverse_enabled(0, false, window, cx);
            panel
                .host_name_input
                .update(cx, |input, cx| input.set_value("Studio", window, cx));
            panel.apply_edits(window, cx);
        });
        cx.read(|cx| {
            let saved = cx.global::<RemoteController>().local_settings();
            assert_eq!(saved.traverse.enabled().count(), 0);
            assert_eq!(saved.remote_host_name.as_deref(), Some("Studio"));
        });
        // A self-hosted instance joins the list enabled once its URL is
        // usable; one already listed is not added twice.
        for typed in [
            "traverse.example",
            "https://traverse.example",
            "https://traverse.example/",
        ] {
            panel.update_in(cx, |panel, window, cx| {
                panel
                    .traverse_add_input
                    .update(cx, |input, cx| input.set_value(typed, window, cx));
                panel.add_traverse(window, cx);
            });
        }
        assert_eq!(
            saved_traverse(cx),
            serde_json::json!({"sources": [
                {"kind": "official", "enabled": false},
                {"kind": "custom", "url": "https://traverse.example/", "enabled": true},
            ]})
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.set_traverse_enabled(0, true, window, cx);
            panel.remove_traverse(1, window, cx);
        });
        assert_eq!(
            saved_traverse(cx),
            serde_json::json!({"sources": [{"kind": "official", "enabled": true}]})
        );

        // A random port: the desktop's fixed one may be taken on this machine.
        let host = TraverseHost::start(
            mux,
            HostConfig {
                host_name: "Test Host".into(),
                data_dir: root.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        host.new_invitation();
        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.host = Some(host));
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing").is_some());
        assert!(
            cx.debug_bounds("remote-invitation").is_none(),
            "the invitation is made on the Hosts page, not in Settings"
        );
        cx.read(|cx| {
            assert!(matches!(
                cx.global::<RemoteController>().invitation_offer(),
                InvitationOffer::Live { .. }
            ));
        });

        panel.update(cx, |panel, cx| panel.set_pairing_enabled(false, cx));
        cx.read(|cx| {
            let controller = cx.global::<RemoteController>();
            assert!(!controller.pairing_enabled());
            assert!(controller.invitation().is_none());
            assert_eq!(
                controller.invitation_offer(),
                InvitationOffer::PairingOff,
                "no invitation is offered while pairing is off"
            );
        });

        panel.update(cx, |panel, cx| panel.set_pairing_enabled(true, cx));
        cx.read(|cx| {
            let controller = cx.global::<RemoteController>();
            assert!(controller.pairing_enabled());
            assert!(
                controller.invitation().is_some(),
                "turning pairing on mints an invitation at once"
            );
        });

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing").is_none());
        cx.read(|cx| {
            assert_eq!(
                cx.global::<RemoteController>().invitation_offer(),
                InvitationOffer::NotHosting
            );
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The transport publishes to the enabled sources, in order. A
    /// self-hosted instance is named by the base URL its manifest is served
    /// from; anything a browser would not fetch is refused.
    #[test]
    fn a_self_hosted_traverse_needs_a_fetchable_base_url() {
        let custom = |url: &str, enabled| TraverseSource {
            instance: TraverseInstance::Custom { url: url.into() },
            enabled,
        };
        let official = |enabled| TraverseSource {
            instance: TraverseInstance::Official,
            enabled,
        };
        assert_eq!(
            traverse_sources(&TraverseSetting::new(vec![
                official(true),
                custom(" https://traverse.example/ ", true),
                custom("https://disabled.example/", false),
            ])),
            Ok(vec![
                ManifestSource::Official,
                ManifestSource::Custom(url::Url::parse("https://traverse.example/").unwrap())
            ])
        );
        assert_eq!(
            traverse_sources(&TraverseSetting::new(vec![official(false)])),
            Ok(Vec::new())
        );
        for rejected in [
            "",
            "traverse.example",
            "ftp://traverse.example/",
            "https://",
        ] {
            assert!(
                traverse_sources(&TraverseSetting::new(vec![
                    official(true),
                    custom(rejected, true)
                ]))
                .is_err(),
                "{rejected:?}"
            );
        }
    }
}
