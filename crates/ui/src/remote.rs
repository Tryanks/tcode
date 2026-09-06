//! Settings → Remote: host this computer, or connect to another tcode host.
//!
//! `RemoteController` is the process-wide handle main.rs installs. It owns the
//! local [`HostMux`], listener and discovery beacon independently of whichever
//! host the desktop window is currently attached to.
//!
//! Everything the panel does off the UI thread (pairing, discovery) is blocking
//! I/O, so it runs on the background executor.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, Context, Entity, Global, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_client::HostLink;
use tcode_client::host::{ClientHost, DiscoveredHost, PairRequest};
use tcode_client::pairing::{PairInvite, PairedHost, pair_url, parse_pair_url};
use tcode_core::settings::Settings;
use tcode_protocol::{Command, SettingsPatch};
use tcode_remote::discovery::{BeaconHandle, start_beacon};
use tcode_remote::{DeviceInfo, HostMux, PairingCode, RemoteConfig, RemoteServer, serve};

use crate::icon::{Icon, IconName};
use crate::overlay::{Notification, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::switch::Switch;

/// Default port advertised by desktop hosting.
pub const DEFAULT_REMOTE_PORT: u16 = 47_420;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTarget {
    Local,
    Remote(PairedHost),
}

type SwitchAttachment = Rc<dyn Fn(AttachmentTarget, &mut Window, &mut App)>;

pub struct RemoteController {
    mux: HostMux,
    server: Option<RemoteServer>,
    beacon: Option<BeaconHandle>,
    data_dir: PathBuf,
    client_host: Rc<dyn ClientHost>,
    local_settings_link: HostLink,
    local_settings: Settings,
    switch_attachment: SwitchAttachment,
    /// The last minted code and when it was minted, for the countdown.
    pairing: Option<(PairingCode, Instant)>,
}

impl Global for RemoteController {}

impl RemoteController {
    pub fn new(
        mux: HostMux,
        data_dir: PathBuf,
        client_host: Rc<dyn ClientHost>,
        local_settings_link: HostLink,
        local_settings: Settings,
        switch_attachment: impl Fn(AttachmentTarget, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            mux,
            server: None,
            beacon: None,
            data_dir,
            client_host,
            local_settings_link,
            local_settings,
            switch_attachment: Rc::new(switch_attachment),
            pairing: None,
        }
    }

    pub fn client_host(&self) -> Rc<dyn ClientHost> {
        self.client_host.clone()
    }

    pub fn switcher(&self) -> SwitchAttachment {
        self.switch_attachment.clone()
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
            pairing.fp.clone(),
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

    pub fn hosts(&self) -> Vec<PairedHost> {
        self.client_host.load_hosts()
    }

    pub fn save_host(&self, host: PairedHost) {
        let mut hosts = self.hosts();
        hosts.retain(|existing| existing.host_id != host.host_id);
        hosts.push(host);
        self.write_hosts(&hosts);
    }

    pub fn remove_host(&self, host_id: &str) {
        let mut hosts = self.hosts();
        hosts.retain(|existing| existing.host_id != host_id);
        self.write_hosts(&hosts);
    }

    fn write_hosts(&self, hosts: &[PairedHost]) {
        self.client_host.save_hosts(hosts);
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

fn countdown(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

enum Discovery {
    Idle,
    Searching,
    Found(Vec<DiscoveredHost>),
}

pub struct RemotePanel {
    store: Entity<WorkspaceStore>,
    port_input: Entity<InputState>,
    host_name_input: Entity<InputState>,
    pair_addr_input: Entity<InputState>,
    pair_port_input: Entity<InputState>,
    pair_code_input: Entity<InputState>,
    discovery: Discovery,
    pairing_busy: bool,
    pair_pin: Option<(String, u16, String)>,
    paired_fingerprint: String,
    /// One-second repaint while a pairing code is counting down.
    ticker: Option<Task<()>>,
}

impl RemotePanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let pair_addr_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("remote.pair.address_placeholder"))
        });
        let pair_port_input =
            cx.new(|cx| InputState::new(window, cx).default_value(DEFAULT_REMOTE_PORT.to_string()));
        let pair_code_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("remote.pair.code_placeholder"))
        });
        let mut panel = Self {
            store,
            port_input,
            host_name_input,
            pair_addr_input,
            pair_port_input,
            pair_code_input,
            discovery: Discovery::Idle,
            pairing_busy: false,
            pair_pin: None,
            paired_fingerprint: String::new(),
            ticker: None,
        };
        panel.sync_ticker(cx);
        panel
    }

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

    fn host_name(&self, cx: &App) -> String {
        let typed = self.host_name_input.read(cx).value().trim().to_owned();
        if typed.is_empty() {
            machine_name()
        } else {
            typed
        }
    }

    fn set_hosting(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        let port = self.port(cx);
        let name = self.host_name(cx);
        let typed_name = self.host_name_input.read(cx).value().trim().to_owned();
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
            let typed_name = self.host_name_input.read(cx).value().trim().to_owned();
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller.save_hosting_settings(
                    false,
                    port,
                    (!typed_name.is_empty()).then_some(typed_name),
                );
            });
        }
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        self.discovery = Discovery::Searching;
        cx.notify();
        let host = cx.global::<RemoteController>().client_host();
        cx.spawn(async move |this, cx| {
            let found = host.browse_hosts().await;
            let _ = this.update(cx, |panel, cx| {
                panel.discovery = Discovery::Found(found);
                cx.notify();
            });
        })
        .detach();
    }

    fn pair_with(
        &mut self,
        addr: String,
        port: u16,
        code: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pairing_busy {
            return;
        }
        let (addr, port, code, fingerprint) =
            if let Some(invite) = parse_pair_url(&addr).or_else(|| parse_pair_url(&code)) {
                (invite.addrs[0].clone(), invite.port, invite.code, invite.fp)
            } else {
                let fp = self
                    .pair_pin
                    .as_ref()
                    .filter(|(a, p, _)| a == &addr && *p == port)
                    .map(|(_, _, fp)| fp.clone())
                    .unwrap_or_default();
                (addr, port, code, fp)
            };
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            window.push_notification(
                Notification::error(crate::tr!("remote.pair.bad_code").into_owned()),
                cx,
            );
            return;
        }
        self.pairing_busy = true;
        cx.notify();
        let host = cx.global::<RemoteController>().client_host();
        cx.spawn_in(window, async move |this, cx| {
            let result = host
                .pair(PairRequest {
                    addr,
                    port,
                    code,
                    fingerprint,
                })
                .await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.pairing_busy = false;
                match result {
                    Ok(host) => {
                        panel.paired_fingerprint = host.fingerprint.clone();
                        let name = host.name.clone();
                        cx.update_global::<RemoteController, _>(|controller, _| {
                            controller.save_host(host);
                        });
                        window.push_notification(
                            Notification::success(
                                crate::tr!("remote.pair.paired", name = name).into_owned(),
                            ),
                            cx,
                        );
                    }
                    Err(error) => {
                        window.push_notification(Notification::error(error), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn section_caption(&self, label: SharedString, cx: &Context<Self>) -> AnyElement {
        div()
            .pl_3()
            .pb(px(6.))
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element()
    }

    fn row(&self) -> gpui::Div {
        h_flex()
            .w_full()
            .min_h(px(44.))
            .px_3()
            .py_2p5()
            .gap_3()
            .items_center()
    }

    fn labels(
        &self,
        title: SharedString,
        description: SharedString,
        cx: &Context<Self>,
    ) -> gpui::Div {
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

    fn note(&self, text: SharedString, cx: &Context<Self>) -> AnyElement {
        div()
            .w_full()
            .px_3()
            .py_3()
            .text_size(px(13.))
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    fn render_hosting(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hosting = cx
            .try_global::<RemoteController>()
            .is_some_and(RemoteController::is_hosting);
        let toggle = self
            .row()
            .child(self.labels(
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
        let port_row = self
            .row()
            .child(self.labels(
                crate::tr!("remote.port.title").into_owned().into(),
                crate::tr!("remote.port.description").into_owned().into(),
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().w(px(110.)).child(
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
        let name_row = self
            .row()
            .child(
                self.labels(
                    crate::tr!("remote.host_name.title").into_owned().into(),
                    crate::tr!("remote.host_name.description")
                        .into_owned()
                        .into(),
                    cx,
                ),
            )
            .child(
                div().w(px(240.)).child(
                    Input::new(&self.host_name_input)
                        .small()
                        .rounded(crate::material::radius_input()),
                ),
            )
            .into_any_element();

        let mut column = v_flex().w_full().gap_3().child(
            v_flex()
                .child(
                    self.section_caption(crate::tr!("remote.host.section").into_owned().into(), cx),
                )
                .child(
                    crate::material::group(cx)
                        .child(toggle)
                        .child(port_row)
                        .child(name_row),
                ),
        );
        if hosting {
            column = column.child(self.render_pairing_card(cx));
            column = column.child(self.render_devices(cx));
        }
        column.into_any_element()
    }

    fn render_pairing_card(&self, cx: &mut Context<Self>) -> AnyElement {
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
                    self.row()
                        .child(self.labels(
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
            addrs: if code.addrs.is_empty() {
                vec!["127.0.0.1".to_owned()]
            } else {
                code.addrs.clone()
            },
            port: code.port,
            code: code.code.clone(),
            fp: code.fp.clone(),
        });
        let addresses = if code.addrs.is_empty() {
            crate::tr!("remote.code.no_addresses").into_owned()
        } else {
            code.addrs.join(", ")
        };
        let qr = qr_element(&url);
        crate::material::group(cx)
            .child(
                h_flex()
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
                            .child(div().text_size(px(11.)).child(crate::tr!(
                                "mobile.fingerprint",
                                fingerprint = tcode_client::pairing::display_fingerprint(&code.fp)
                            )))
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

    fn render_devices(&self, cx: &mut Context<Self>) -> AnyElement {
        let devices = cx
            .try_global::<RemoteController>()
            .map(RemoteController::devices)
            .unwrap_or_default();
        let mut group = crate::material::group(cx);
        if devices.is_empty() {
            group =
                group.child(self.note(crate::tr!("remote.devices.empty").into_owned().into(), cx));
        }
        for device in devices {
            let id = device.id.clone();
            group = group.child(
                self.row()
                    .child(
                        self.labels(
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
                        ),
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
            );
        }
        v_flex()
            .child(
                self.section_caption(crate::tr!("remote.devices.section").into_owned().into(), cx),
            )
            .child(group)
            .into_any_element()
    }

    fn render_connect(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current_id = self.store.read(cx).remote_host_id().map(str::to_owned);
        let current_name = self.store.read(cx).remote_host_name().map(str::to_owned);
        let mut column = v_flex().w_full().gap_3().child(
            self.section_caption(crate::tr!("remote.connect.section").into_owned().into(), cx),
        );
        if current_id.as_deref().is_some_and(|host_id| {
            cx.global::<RemoteController>()
                .client_host()
                .certificate_changed(host_id)
        }) {
            column = column.child(
                div()
                    .text_color(cx.theme().danger_foreground)
                    .child(crate::tr!("mobile.certificate_changed"))
                    .child(crate::tr!("mobile.certificate_changed_help")),
            );
        }
        if let Some(name) = current_name {
            column = column.child(
                crate::material::group(cx).child(
                    self.row()
                        .child(
                            self.labels(
                                crate::tr!("remote.connect.connected_to", name = name)
                                    .into_owned()
                                    .into(),
                                crate::tr!("remote.connect.connected_description")
                                    .into_owned()
                                    .into(),
                                cx,
                            ),
                        )
                        .child(
                            Button::new("remote-back-to-local")
                                .primary()
                                .compact()
                                .label(crate::tr!("remote.connect.back_to_local"))
                                .on_click(|_, window, cx| {
                                    let switch = cx.global::<RemoteController>().switcher();
                                    switch(AttachmentTarget::Local, window, cx);
                                }),
                        ),
                ),
            );
        }
        column
            .child(self.render_paired_hosts(current_id.as_deref(), cx))
            .child(self.render_discovery(cx))
            .child(self.render_pair_form(cx))
            .into_any_element()
    }

    fn render_paired_hosts(&self, current_id: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let hosts = cx
            .try_global::<RemoteController>()
            .map(RemoteController::hosts)
            .unwrap_or_default();
        let mut group = crate::material::group(cx);
        if hosts.is_empty() {
            group =
                group.child(self.note(crate::tr!("remote.hosts.empty").into_owned().into(), cx));
        }
        for host in hosts {
            let current = current_id == Some(host.host_id.as_str());
            let connect_host = host.clone();
            let remove_id = host.host_id.clone();
            let address = host
                .addrs
                .first()
                .cloned()
                .unwrap_or_else(|| "?".to_owned());
            group = group.child(
                self.row()
                    .child(
                        self.labels(
                            host.name.clone().into(),
                            format!(
                                "{address}:{} · {}",
                                host.port,
                                crate::tr!(
                                    "mobile.fingerprint",
                                    fingerprint = tcode_client::pairing::display_fingerprint(
                                        &host.fingerprint
                                    )
                                )
                            )
                            .into(),
                            cx,
                        ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("connect-{}", host.host_id)))
                            .ghost()
                            .outline()
                            .compact()
                            .disabled(current)
                            .label(if current {
                                crate::tr!("remote.hosts.current")
                            } else {
                                crate::tr!("remote.hosts.connect")
                            })
                            .on_click(move |_, window, cx| {
                                let switch = cx.global::<RemoteController>().switcher();
                                switch(AttachmentTarget::Remote(connect_host.clone()), window, cx);
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-{}", host.host_id)))
                            .ghost()
                            .compact()
                            .label(crate::tr!("remote.hosts.remove"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                let id = remove_id.clone();
                                cx.update_global::<RemoteController, _>(|controller, _| {
                                    controller.remove_host(&id);
                                });
                                cx.notify();
                            })),
                    ),
            );
        }
        v_flex()
            .child(self.section_caption(crate::tr!("remote.hosts.section").into_owned().into(), cx))
            .child(group)
            .into_any_element()
    }

    fn render_discovery(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut group = crate::material::group(cx).child(
            self.row()
                .child(
                    self.labels(
                        crate::tr!("remote.discover.title").into_owned().into(),
                        crate::tr!("remote.discover.description")
                            .into_owned()
                            .into(),
                        cx,
                    ),
                )
                .child(
                    Button::new("remote-discover")
                        .ghost()
                        .outline()
                        .compact()
                        .loading(matches!(self.discovery, Discovery::Searching))
                        .label(crate::tr!("remote.discover.search"))
                        .on_click(cx.listener(|this, _, _, cx| this.discover(cx))),
                ),
        );
        match &self.discovery {
            Discovery::Idle | Discovery::Searching => {}
            Discovery::Found(found) if found.is_empty() => {
                group = group
                    .child(self.note(crate::tr!("remote.discover.none").into_owned().into(), cx));
            }
            Discovery::Found(found) => {
                for beacon in found {
                    let addr = beacon.addr.clone();
                    let port = beacon.port;
                    let fingerprint = beacon.fp.clone();
                    group =
                        group.child(
                            self.row()
                                .child(self.labels(
                                    beacon.name.clone().into(),
                                    format!("{}:{}", beacon.addr, beacon.port).into(),
                                    cx,
                                ))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "pair-found-{}-{}",
                                        beacon.host_id, beacon.addr
                                    )))
                                    .ghost()
                                    .outline()
                                    .compact()
                                    .label(crate::tr!("remote.discover.pair"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        // Discovery carries no code: prefill the form
                                        // so the user only types the six digits.
                                        let addr = addr.clone();
                                        this.pair_pin =
                                            Some((addr.clone(), port, fingerprint.clone()));
                                        this.pair_addr_input.update(cx, |input, cx| {
                                            input.set_value(addr, window, cx)
                                        });
                                        this.pair_port_input.update(cx, |input, cx| {
                                            input.set_value(port.to_string(), window, cx)
                                        });
                                        this.pair_code_input
                                            .update(cx, |input, cx| input.focus(window, cx));
                                        cx.notify();
                                    })),
                                ),
                        );
                }
            }
        }
        v_flex()
            .child(self.section_caption(
                crate::tr!("remote.discover.section").into_owned().into(),
                cx,
            ))
            .child(group)
            .into_any_element()
    }

    fn render_pair_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let input = |state: &Entity<InputState>, width: f32| {
            div().w(px(width)).child(
                Input::new(state)
                    .small()
                    .rounded(crate::material::radius_input()),
            )
        };
        v_flex()
            .child(self.section_caption(crate::tr!("remote.pair.section").into_owned().into(), cx))
            .child(div().text_size(px(12.)).child(crate::tr!(
                "mobile.fingerprint",
                fingerprint = tcode_client::pairing::display_fingerprint(&self.paired_fingerprint)
            )))
            .child(
                crate::material::group(cx).child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::tr!("remote.pair.description")),
                        )
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_center()
                                .child(input(&self.pair_addr_input, 200.))
                                .child(input(&self.pair_port_input, 84.))
                                .child(input(&self.pair_code_input, 110.))
                                .child(
                                    Button::new("remote-pair")
                                        .primary()
                                        .compact()
                                        .loading(self.pairing_busy)
                                        .label(crate::tr!("remote.pair.action"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let addr = this
                                                .pair_addr_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_owned();
                                            let port = this
                                                .pair_port_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .parse()
                                                .unwrap_or(DEFAULT_REMOTE_PORT);
                                            let code = this
                                                .pair_code_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_owned();
                                            this.pair_with(addr, port, code, window, cx);
                                        })),
                                ),
                        ),
                ),
            )
            .into_any_element()
    }
}

impl Render for RemotePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_ticker(cx);
        let hosting = self.render_hosting(cx);
        v_flex()
            .w_full()
            .gap_6()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::Info).xsmall())
                    .child(div().text_size(px(13.)).child(crate::tr!("remote.intro"))),
            )
            .child(hosting)
            .child(self.render_connect(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_reads_as_minutes_and_seconds() {
        assert_eq!(countdown(299), "4:59");
        assert_eq!(countdown(60), "1:00");
        assert_eq!(countdown(7), "0:07");
    }
}
