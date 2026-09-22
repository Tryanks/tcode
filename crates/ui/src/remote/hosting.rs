//! Hosting this machine: the Traverse endpoint, minted invitations and the
//! devices that have paired with it.
//!
//! [`RemoteController`] is the process-wide handle the composition root installs.
//! It owns the local [`HostMux`] and endpoint independently of whichever host
//! the window is currently attached to, so **Connect** and **Back to local**
//! never stop it and never disturb another attached client.

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Action, AnyElement, App, AppContext as _, BorrowAppContext as _, ClipboardItem, Context,
    Entity, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_client::HostLink;
use tcode_core::settings::{Settings, TraverseSetting};
use tcode_protocol::{Command, SettingsPatch};
use tcode_traverse::{DeviceInfo, HostConfig, HostMux, Invitation, TraverseHost, TraverseMode};

use super::qr::qr_element;
use crate::icon::{Icon, IconName};
use crate::overlay::{Notification, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::DropdownMenu as _;
use crate::widgets::switch::Switch;
use crate::widgets::tooltip::Tooltip;

/// How often the devices list re-reads which path each connection is on.
const DEVICE_REFRESH: Duration = Duration::from_secs(2);

/// Pick a Traverse mode from the selector. The URL of a self-hosted instance
/// is typed into its own field, so the choice carries no URL.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosting, no_json)]
enum SelectTraverse {
    Official,
    Custom,
    Off,
}

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
                traverse: traverse_mode(traverse)?,
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

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.host
            .as_ref()
            .map(TraverseHost::devices)
            .unwrap_or_default()
    }

    pub fn revoke_device(&self, id: &str) {
        if let Some(host) = self.host.as_ref() {
            host.revoke(id);
        }
    }
}

/// The transport's view of a Traverse setting. A self-hosted instance needs
/// a usable base URL; the page validates it before offering Apply, so a
/// failure here comes from a settings file edited by hand.
fn traverse_mode(setting: &TraverseSetting) -> Result<TraverseMode, String> {
    match setting {
        TraverseSetting::Official => Ok(TraverseMode::Official),
        TraverseSetting::Off => Ok(TraverseMode::Off),
        TraverseSetting::Custom { url } => custom_traverse_url(url)
            .map(TraverseMode::Custom)
            .ok_or_else(|| crate::tr!("remote.traverse.invalid_url").into_owned()),
    }
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

fn labels(title: SharedString, description: SharedString, cx: &App) -> gpui::Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(div().text_size(px(15.)).font_medium().child(title))
        .child(
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
}

fn countdown(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The selector's label for a mode.
fn traverse_label(setting: &TraverseSetting) -> SharedString {
    match setting {
        TraverseSetting::Official => crate::tr!("remote.traverse.official"),
        TraverseSetting::Custom { .. } => crate::tr!("remote.traverse.custom"),
        TraverseSetting::Off => crate::tr!("remote.traverse.off"),
    }
    .into_owned()
    .into()
}

/// One line on what the selected mode means for the devices connecting here.
fn traverse_description(setting: &TraverseSetting) -> SharedString {
    match setting {
        TraverseSetting::Official => crate::tr!("remote.traverse.official_description"),
        TraverseSetting::Custom { .. } => crate::tr!("remote.traverse.custom_description"),
        TraverseSetting::Off => crate::tr!("remote.traverse.off_description"),
    }
    .into_owned()
    .into()
}

/// Settings → Other devices: the editable hosting controls for *this machine*.
/// Their live state lives in the process-wide [`RemoteController`]; only the
/// in-progress edits belong here.
pub struct HostingPanel {
    host_name_input: Entity<InputState>,
    /// The selector's choice; the URL of a self-hosted instance is in
    /// `traverse_url_input`.
    traverse_choice: SelectTraverse,
    traverse_url_input: Entity<InputState>,
    /// Repaint while hosting: every second while a code counts down, every
    /// [`DEVICE_REFRESH`] otherwise for the devices' paths.
    ticker: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl HostingPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let (traverse_choice, url) = match &settings.traverse {
            TraverseSetting::Official => (SelectTraverse::Official, String::new()),
            TraverseSetting::Custom { url } => (SelectTraverse::Custom, url.clone()),
            TraverseSetting::Off => (SelectTraverse::Off, String::new()),
        };
        let traverse_url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::tr!("remote.traverse.url_placeholder").into_owned())
                .default_value(url)
        });
        // Apply lights up as soon as an edit differs from what is saved.
        let subscriptions = [&host_name_input, &traverse_url_input]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            host_name_input,
            traverse_choice,
            traverse_url_input,
            ticker: None,
            _subscriptions: subscriptions,
        }
    }
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
                        let counting = cx.update(|cx| {
                            cx.try_global::<RemoteController>()
                                .is_some_and(|controller| controller.invitation().is_some())
                        });
                        let interval = if counting {
                            Duration::from_secs(1)
                        } else {
                            DEVICE_REFRESH
                        };
                        cx.background_executor().timer(interval).await;
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

    /// The Traverse setting as edited, or `None` while the self-hosted URL
    /// is not one.
    fn typed_traverse(&self, cx: &App) -> Option<TraverseSetting> {
        Some(match self.traverse_choice {
            SelectTraverse::Official => TraverseSetting::Official,
            SelectTraverse::Off => TraverseSetting::Off,
            SelectTraverse::Custom => TraverseSetting::Custom {
                url: custom_traverse_url(&self.traverse_url_input.read(cx).value())?.to_string(),
            },
        })
    }

    /// Whether the edits differ from the saved settings and are complete.
    fn has_pending_edits(&self, cx: &App) -> bool {
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
    /// at once; while not hosting, just save it for the next start.
    fn apply_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn on_select_traverse(
        &mut self,
        choice: &SelectTraverse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.traverse_choice = choice.clone();
        if *choice == SelectTraverse::Custom {
            self.traverse_url_input
                .update(cx, |state, cx| state.focus(window, cx));
        }
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
                crate::tr!("remote.host.description").into_owned().into(),
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
                    crate::tr!("remote.pairing.description").into_owned().into(),
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
                crate::tr!("remote.host_name.description")
                    .into_owned()
                    .into(),
                cx,
            ))
            .child(
                div()
                    .when(compact, |field| field.w_full())
                    .when(!compact, |field| field.w(px(240.)))
                    .child(
                        Input::new(&self.host_name_input)
                            .small()
                            .rounded(crate::material::radius_input()),
                    ),
            )
            .into_any_element();
        let selected = match self.traverse_choice {
            SelectTraverse::Official => TraverseSetting::Official,
            SelectTraverse::Custom => TraverseSetting::Custom { url: String::new() },
            SelectTraverse::Off => TraverseSetting::Off,
        };
        let traverse_row = row(compact)
            .child(labels(
                crate::tr!("remote.traverse.title").into_owned().into(),
                traverse_description(&selected),
                cx,
            ))
            .child(
                Button::new("remote-traverse")
                    .ghost()
                    .outline()
                    .compact()
                    .child(
                        h_flex()
                            .w(px(180.))
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .text_size(px(13.))
                            .child(traverse_label(&selected))
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .xsmall()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .dropdown_menu({
                        let choice = self.traverse_choice.clone();
                        move |menu, _window, _cx| {
                            let mut menu = menu;
                            for (option, key) in [
                                (SelectTraverse::Official, "remote.traverse.official"),
                                (SelectTraverse::Custom, "remote.traverse.custom"),
                                (SelectTraverse::Off, "remote.traverse.off"),
                            ] {
                                menu = menu.menu_with_check(
                                    crate::tr!(key).into_owned(),
                                    option == choice,
                                    Box::new(option),
                                );
                            }
                            menu
                        }
                    }),
            )
            .into_any_element();
        let url_valid = custom_traverse_url(&self.traverse_url_input.read(cx).value()).is_some();
        let url_row = (self.traverse_choice == SelectTraverse::Custom).then(|| {
            row(compact)
                .child(labels(
                    crate::tr!("remote.traverse.url").into_owned().into(),
                    if url_valid {
                        crate::tr!("remote.traverse.url_description")
                    } else {
                        crate::tr!("remote.traverse.invalid_url")
                    }
                    .into_owned()
                    .into(),
                    cx,
                ))
                .child(
                    div()
                        .when(compact, |field| field.w_full())
                        .when(!compact, |field| field.w(px(240.)))
                        .child(
                            Input::new(&self.traverse_url_input)
                                .small()
                                .rounded(crate::material::radius_input()),
                        ),
                )
                .into_any_element()
        });
        let apply_row = h_flex()
            .w_full()
            .px_3()
            .py_2p5()
            .justify_end()
            .child(
                Button::new("remote-apply")
                    .ghost()
                    .outline()
                    .compact()
                    .disabled(!self.has_pending_edits(cx))
                    .label(crate::tr!("remote.apply"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_edits(window, cx);
                    })),
            )
            .into_any_element();

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
                        .child(traverse_row)
                        .children(url_row)
                        .child(apply_row),
                ),
        );
        if hosting {
            column = column.child(self.render_pairing_card(compact, cx));
            column = column.child(self.render_devices(compact, cx));
        }
        column.into_any_element()
    }

    fn render_pairing_card(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(controller) = cx.try_global::<RemoteController>() else {
            return div().into_any_element();
        };
        if !controller.pairing_enabled() {
            return crate::material::group(cx)
                .debug_selector(|| "remote-pairing-off".into())
                .child(note(
                    crate::tr!("remote.pairing.off").into_owned().into(),
                    cx,
                ))
                .into_any_element();
        }
        let machine_id = controller.endpoint_id().unwrap_or_default();
        let new_invitation = |id: &'static str, cx: &mut Context<Self>| {
            Button::new(id)
                .compact()
                .label(crate::tr!("remote.invite.new"))
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.update_global::<RemoteController, _>(|controller, _| {
                        controller.new_invitation();
                    });
                    this.sync_ticker(cx);
                    cx.notify();
                }))
        };
        let Some((invitation, remaining)) = controller.invitation() else {
            return crate::material::group(cx)
                .debug_selector(|| "remote-invitation".into())
                .child(
                    row(compact)
                        .child(labels(
                            crate::tr!("remote.invite.expired").into_owned().into(),
                            crate::tr!("remote.invite.description").into_owned().into(),
                            cx,
                        ))
                        .child(new_invitation("remote-new-invitation", cx).primary()),
                )
                .into_any_element();
        };
        let link = invitation.url();
        let qr = qr_element(&link);
        crate::material::group(cx)
            .debug_selector(|| "remote-invitation".into())
            .child(
                // Compact stacks the QR under the text rather than putting a
                // fixed-size image beside text that then has nowhere to wrap.
                if compact { v_flex() } else { h_flex() }
                    .w_full()
                    .px_3()
                    .py_3()
                    .gap_4()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_medium()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!("remote.invite.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .child(crate::tr!("remote.invite.how")),
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
                            .child(self.machine_fingerprint(machine_id, cx))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Button::new("remote-copy-invitation")
                                            .ghost()
                                            .outline()
                                            .compact()
                                            .label(crate::tr!("remote.invite.copy"))
                                            .on_click(cx.listener(move |_, _, window, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    link.clone(),
                                                ));
                                                window.push_notification(
                                                    Notification::info(
                                                        crate::tr!("remote.invite.copied")
                                                            .into_owned(),
                                                    ),
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        new_invitation("remote-new-invitation", cx)
                                            .ghost()
                                            .outline(),
                                    ),
                            ),
                    )
                    .children(qr),
            )
            .into_any_element()
    }

    /// The machine id as its fingerprint; the whole id on hover and on the
    /// clipboard when clicked, for checking against a device's Machines page.
    fn machine_fingerprint(&self, machine_id: String, cx: &mut Context<Self>) -> AnyElement {
        let full = machine_id.clone();
        h_flex()
            .id("remote-machine-id")
            .gap_1()
            .items_center()
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .child(crate::tr!(
                "remote.invite.machine",
                fingerprint = super::fingerprint(&machine_id)
            ))
            .child(Icon::new(IconName::Copy).xsmall())
            .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
            .on_click(cx.listener(move |_, _, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(machine_id.clone()));
                window.push_notification(
                    Notification::info(crate::tr!("remote.invite.machine_copied").into_owned()),
                    cx,
                );
            }))
            .into_any_element()
    }

    fn render_devices(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let devices = cx
            .try_global::<RemoteController>()
            .map(RemoteController::devices)
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
            group = group.child(
                row(compact)
                    .child(labels(
                        super::device_label(&device.name, device.platform.as_deref()).into(),
                        crate::tr!(
                            "remote.devices.paired_on",
                            date = crate::time::humanize_ago(
                                crate::time::now_secs().saturating_sub(device.created_unix)
                            )
                        )
                        .into_owned()
                        .into(),
                        cx,
                    ))
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
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let id = id.clone();
                                        cx.update_global::<RemoteController, _>(|controller, _| {
                                            controller.revoke_device(&id);
                                        });
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

impl Render for HostingPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        div()
            .w_full()
            .min_w_0()
            .debug_selector(|| "hosting-settings".into())
            .on_action(cx.listener(Self::on_select_traverse))
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

    /// The pairing switch exists only while hosting; flipping it reaches the
    /// transport, which drops or mints the invitation, and the card follows.
    #[gpui::test]
    fn the_pairing_switch_follows_hosting_and_drives_the_transport(cx: &mut TestAppContext) {
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
                HostLink::new(to_host, from_host),
                Settings::default(),
            ))
        });
        let window = cx.open_window(gpui::size(px(900.), px(700.)), |window, cx| {
            Probe(cx.new(|cx| HostingPanel::new(window, cx)))
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

        // A random port: the desktop's fixed one may be taken on this machine.
        let host = TraverseHost::start(
            mux,
            HostConfig {
                host_name: "Test Host".into(),
                data_dir: root.clone(),
                traverse: TraverseMode::Off,
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
        assert!(cx.debug_bounds("remote-invitation").is_some());
        assert!(cx.debug_bounds("remote-pairing-off").is_none());
        let panel = window.read_with(cx, |probe, _| probe.0.clone()).unwrap();

        panel.update(cx, |panel, cx| panel.set_pairing_enabled(false, cx));
        cx.read(|cx| {
            let controller = cx.global::<RemoteController>();
            assert!(!controller.pairing_enabled());
            assert!(controller.invitation().is_none());
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing-off").is_some());
        assert!(
            cx.debug_bounds("remote-invitation").is_none(),
            "no invitation is offered while pairing is off"
        );

        panel.update(cx, |panel, cx| panel.set_pairing_enabled(true, cx));
        cx.read(|cx| {
            let controller = cx.global::<RemoteController>();
            assert!(controller.pairing_enabled());
            assert!(
                controller.invitation().is_some(),
                "turning pairing on mints an invitation at once"
            );
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing-off").is_none());
        assert!(cx.debug_bounds("remote-invitation").is_some());

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A self-hosted instance is named by the base URL its manifest is
    /// served from; anything a browser would not fetch is refused before it
    /// can be saved.
    #[test]
    fn a_self_hosted_traverse_needs_a_fetchable_base_url() {
        assert_eq!(
            traverse_mode(&TraverseSetting::Custom {
                url: " https://traverse.example/ ".into()
            }),
            Ok(TraverseMode::Custom(
                url::Url::parse("https://traverse.example/").unwrap()
            ))
        );
        for rejected in [
            "",
            "traverse.example",
            "ftp://traverse.example/",
            "https://",
        ] {
            assert!(
                traverse_mode(&TraverseSetting::Custom {
                    url: rejected.into()
                })
                .is_err(),
                "{rejected:?}"
            );
        }
        assert_eq!(
            traverse_mode(&TraverseSetting::Official),
            Ok(TraverseMode::Official)
        );
        assert_eq!(traverse_mode(&TraverseSetting::Off), Ok(TraverseMode::Off));
    }
}
