//! Hosting this machine: the listener, the discovery beacon, minted pairing
//! codes and the devices that have paired with it.
//!
//! [`RemoteController`] is the process-wide handle the composition root installs.
//! It owns the local [`HostMux`], listener and beacon independently of whichever
//! host the window is currently attached to, so **Connect** and **Back to local**
//! never stop it and never disturb another attached client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, Context, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_client::HostLink;
use tcode_client::pairing::{PairInvite, pair_url};
use tcode_core::settings::Settings;
use tcode_protocol::{Command, SettingsPatch};
use tcode_remote::discovery::{BeaconHandle, start_beacon};
use tcode_remote::{DeviceInfo, HostMux, PairingCode, RemoteConfig, RemoteServer, serve};

use crate::overlay::{Notification, OverlayExt as _};
use crate::pairing::DEFAULT_REMOTE_PORT;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::switch::Switch;

/// A caption above one group of this settings-like page. Grouped cards, not
/// the plain content-list rows Machines uses (`docs/DESIGN.md`, list style).
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
    server: Option<RemoteServer>,
    beacon: Option<BeaconHandle>,
    data_dir: PathBuf,
    local_settings_link: HostLink,
    local_settings: Settings,
    /// The last minted code and when it was minted, for the countdown.
    pairing: Option<(PairingCode, Instant)>,
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
            server: None,
            beacon: None,
            data_dir,
            local_settings_link,
            local_settings,
            pairing: None,
        }
    }

    pub fn local_settings(&self) -> &Settings {
        &self.local_settings
    }

    pub fn save_hosting_settings(&mut self, enabled: bool, port: u16, name: Option<String>) {
        self.local_settings.remote_hosting_enabled = enabled;
        self.local_settings.remote_port = Some(port);
        self.local_settings.remote_host_name = name.clone();
        for patch in [
            SettingsPatch::RemoteHostingEnabled(enabled),
            SettingsPatch::RemotePort(Some(port)),
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
        self.server.is_some()
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.server.as_ref().map(RemoteServer::local_addr)
    }

    /// Bind the listener, start the discovery beacon and mint a first code.
    pub fn start_hosting(&mut self, port: u16, host_name: String) -> Result<(), String> {
        if self.server.is_some() {
            return Ok(());
        }
        let listen: SocketAddr = format!("0.0.0.0:{port}")
            .parse()
            .map_err(|error| format!("invalid listen address: {error}"))?;
        let server = serve(
            self.mux.clone(),
            RemoteConfig {
                listen,
                host_name,
                data_dir: self.data_dir.clone(),
                static_bundle: None,
            },
        )
        .map_err(|error| error.to_string())?;
        let pairing = server.new_pairing_code();
        self.beacon = Some(start_beacon(
            pairing.host_id.clone(),
            pairing.host_name.clone(),
            server.local_addr().port(),
        ));
        self.pairing = Some((pairing, Instant::now()));
        self.server = Some(server);
        Ok(())
    }

    pub fn stop_hosting(&mut self) {
        if let Some(beacon) = self.beacon.take() {
            beacon.shutdown();
        }
        if let Some(server) = self.server.take() {
            server.shutdown();
        }
        self.pairing = None;
    }

    pub fn new_pairing_code(&mut self) {
        if let Some(server) = self.server.as_ref() {
            self.pairing = Some((server.new_pairing_code(), Instant::now()));
        }
    }

    /// The active code with its remaining lifetime in seconds, or `None` once
    /// it has expired.
    pub fn pairing(&self) -> Option<(&PairingCode, u64)> {
        let (code, minted) = self.pairing.as_ref()?;
        let remaining = code
            .expires_in_secs
            .saturating_sub(minted.elapsed().as_secs());
        (remaining > 0).then_some((code, remaining))
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.server
            .as_ref()
            .map(RemoteServer::devices)
            .unwrap_or_default()
    }

    pub fn revoke_device(&self, id: &str) {
        if let Some(server) = self.server.as_ref()
            && let Err(error) = server.revoke_device(id)
        {
            log::error!("could not revoke remote device: {error}");
        }
    }
}

/// This machine's default advertised host name.
pub fn machine_name() -> String {
    tcode_remote::client_host::default_device_name()
}

/// A QR code as `(width_in_modules, dark_module_flags)`, row-major.
fn qr_modules(payload: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width();
    let modules = code
        .into_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    Some((width, modules))
}

/// Paint the matrix as one flex row per module row, collapsing consecutive
/// same-colour modules into a single box — a per-module element would be
/// thousands of nodes repainting every countdown tick.
fn qr_element(payload: &str) -> Option<AnyElement> {
    const MODULE: f32 = 4.;
    const QUIET: f32 = 12.;
    let (width, modules) = qr_modules(payload)?;
    // A QR is scanned by a camera, not read by a human: it must stay black on
    // white in both themes, so neither colour comes from the palette.
    let dark = gpui::black();
    let mut grid = v_flex().flex_none();
    for row in modules.chunks(width) {
        let mut line = h_flex().flex_none().h(px(MODULE));
        let mut start = 0;
        while start < row.len() {
            let mut end = start + 1;
            while end < row.len() && row[end] == row[start] {
                end += 1;
            }
            // The row centers its children, so a run without an explicit
            // height would collapse to nothing and paint no modules at all.
            let run = div()
                .flex_none()
                .h(px(MODULE))
                .w(px((end - start) as f32 * MODULE));
            line = line.child(if row[start] { run.bg(dark) } else { run });
            start = end;
        }
        grid = grid.child(line);
    }
    Some(
        div()
            .flex_none()
            .p(px(QUIET))
            .rounded(crate::material::radius_card())
            .bg(gpui::white())
            .child(grid)
            .into_any_element(),
    )
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

/// Settings → Remote: the editable hosting controls for *this machine*. Their
/// live state lives in the process-wide [`RemoteController`]; only the
/// in-progress edits belong here.
pub struct HostingPanel {
    port_input: Entity<InputState>,
    host_name_input: Entity<InputState>,
    /// One-second repaint while a pairing code is counting down.
    ticker: Option<Task<()>>,
}

impl HostingPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = cx
            .try_global::<RemoteController>()
            .map(RemoteController::local_settings)
            .cloned()
            .unwrap_or_default();
        let port_input = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                settings
                    .remote_port
                    .unwrap_or(DEFAULT_REMOTE_PORT)
                    .to_string(),
            )
        });
        let host_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(machine_name())
                .default_value(settings.remote_host_name.clone().unwrap_or_default())
        });
        Self {
            port_input,
            host_name_input,
            ticker: None,
        }
    }
}

impl HostingPanel {
    /// Run a 1 Hz repaint exactly while a code is counting down.
    fn sync_ticker(&mut self, cx: &mut Context<Self>) {
        let counting = cx
            .try_global::<RemoteController>()
            .is_some_and(|controller| controller.pairing().is_some());
        match (counting, self.ticker.is_some()) {
            (true, false) => {
                self.ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
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

    fn port(&self, cx: &App) -> u16 {
        self.port_input
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(DEFAULT_REMOTE_PORT)
    }

    fn typed_host_name(&self, cx: &App) -> String {
        self.host_name_input.read(cx).value().trim().to_owned()
    }

    fn set_hosting(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let port = self.port(cx);
        let typed_name = self.typed_host_name(cx);
        let name = if typed_name.is_empty() {
            machine_name()
        } else {
            typed_name.clone()
        };
        let mut failure = None;
        cx.update_global::<RemoteController, _>(|controller, _| {
            if enabled {
                if let Err(error) = controller.start_hosting(port, name) {
                    failure = Some(error);
                }
            } else {
                controller.stop_hosting();
            }
            if failure.is_none() {
                controller.save_hosting_settings(
                    enabled,
                    port,
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

    /// Re-bind the listener so an edited port or name takes effect at once.
    fn restart_hosting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if cx
            .try_global::<RemoteController>()
            .is_some_and(RemoteController::is_hosting)
        {
            self.set_hosting(false, window, cx);
            self.set_hosting(true, window, cx);
        } else {
            let port = self.port(cx);
            let typed_name = self.typed_host_name(cx);
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller.save_hosting_settings(
                    false,
                    port,
                    (!typed_name.is_empty()).then_some(typed_name),
                );
            });
        }
    }

    fn render_hosting(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        self.sync_ticker(cx);
        let hosting = cx
            .try_global::<RemoteController>()
            .is_some_and(RemoteController::is_hosting);
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
        let port_row = row(compact)
            .child(labels(
                crate::tr!("remote.port.title").into_owned().into(),
                crate::tr!("remote.port.description").into_owned().into(),
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .when(compact, |controls| controls.w_full())
                    .child(
                        div()
                            .when(compact, |field| field.flex_1().min_w_0())
                            .when(!compact, |field| field.w(px(110.)))
                            .child(
                                Input::new(&self.port_input)
                                    .small()
                                    .rounded(crate::material::radius_input()),
                            ),
                    )
                    .child(
                        Button::new("remote-apply-port")
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("remote.apply"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restart_hosting(window, cx);
                            })),
                    ),
            )
            .into_any_element();
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

        let mut column = v_flex().w_full().gap_3().child(
            v_flex()
                .child(section_caption(
                    crate::tr!("remote.host.section").into_owned().into(),
                    cx,
                ))
                .child(
                    crate::material::group(cx)
                        .child(toggle)
                        .child(port_row)
                        .child(name_row),
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
        let listening = controller
            .local_addr()
            .map(|addr| addr.port().to_string())
            .unwrap_or_default();
        let Some((code, remaining)) = controller.pairing() else {
            return crate::material::group(cx)
                .child(
                    row(compact)
                        .child(labels(
                            crate::tr!("remote.code.expired").into_owned().into(),
                            crate::tr!("remote.code.description").into_owned().into(),
                            cx,
                        ))
                        .child(
                            Button::new("remote-new-code")
                                .primary()
                                .compact()
                                .label(crate::tr!("remote.code.new"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.update_global::<RemoteController, _>(|controller, _| {
                                        controller.new_pairing_code();
                                    });
                                    this.sync_ticker(cx);
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element();
        };
        let digits = code.code.clone();
        let url = pair_url(&PairInvite {
            host_id: code.host_id.clone(),
            name: code.host_name.clone(),
            origin: tcode_client::pairing::lan_origin(
                code.addrs
                    .first()
                    .map(String::as_str)
                    .unwrap_or("127.0.0.1"),
                code.port,
            ),
            code: code.code.clone(),
        });
        let addresses = if code.addrs.is_empty() {
            crate::tr!("remote.code.no_addresses").into_owned()
        } else {
            code.addrs.join(", ")
        };
        let qr = qr_element(&url);
        crate::material::group(cx)
            .child(
                // Compact stacks the QR under the code rather than putting a
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
                                    .child(crate::tr!("remote.code.title")),
                            )
                            .child(
                                div()
                                    .font_family("Lilex")
                                    .text_size(px(34.))
                                    .font_semibold()
                                    .child(digits),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!(
                                        "remote.code.expires",
                                        time = countdown(remaining)
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!(
                                        "remote.code.listening",
                                        addrs = addresses,
                                        port = listening
                                    )),
                            )
                            .child(
                                h_flex().child(
                                    Button::new("remote-new-code")
                                        .ghost()
                                        .outline()
                                        .compact()
                                        .label(crate::tr!("remote.code.new"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.update_global::<RemoteController, _>(
                                                |controller, _| controller.new_pairing_code(),
                                            );
                                            this.sync_ticker(cx);
                                            cx.notify();
                                        })),
                                ),
                            ),
                    )
                    .children(qr),
            )
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
            group = group.child(
                row(compact)
                    .child(labels(
                        device.name.clone().into(),
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
            .child(self.render_hosting(compact, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::countdown;

    #[test]
    fn countdown_reads_as_minutes_and_seconds() {
        assert_eq!(countdown(299), "4:59");
        assert_eq!(countdown(60), "1:00");
        assert_eq!(countdown(7), "0:07");
    }
}
