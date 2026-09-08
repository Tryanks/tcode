//! Hosts: which host this window talks to.
//!
//! This is a product surface, not a settings page. It answers one question —
//! *which host am I talking to* — with saved hosts, discovery, pairing and
//! authentication repair, and it is reached from the sidebar's feature area at
//! every width. It needs `tcode_client` and the attachment owner's switch
//! action, and nothing else: it compiles on every client, including
//! `--no-default-features`.
//!
//! **Hosting** — the listener, discovery beacon, minted codes and paired
//! devices — is a genuine setting of *this machine* and lives in
//! `hosting`, behind `remote-hosting`, inside Settings → Remote. The browser
//! uses `hosted` to control its headless listener over the authenticated pipe.

use crate::touch_scroll::TouchScrollExt as _;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, Context, Entity, Global, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_client::host::ClientHost;
use tcode_client::pairing::PairedHost;

use crate::icon::{Icon, IconName};
// Machines are a navigable content list, so their rows, captions and hairlines
// are the shared plain-list vocabulary — the same the thread list uses.
use crate::material::{list_caption, list_row, plain_list};
use crate::pairing::PairForm;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_state::{Destination, WindowState};

#[cfg(feature = "remote-hosting")]
mod hosting;

#[cfg(target_family = "wasm")]
pub(crate) mod hosted;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod qr;

#[cfg(feature = "remote-hosting")]
pub use hosting::{HostingPanel, RemoteController, machine_name};

pub use crate::pairing::DEFAULT_REMOTE_PORT;

/// Where this window's workspace comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTarget {
    Local,
    Remote(PairedHost),
}

pub type SwitchAttachment = Rc<dyn Fn(AttachmentTarget, &mut Window, &mut App)>;

/// Leave the host this row names, keeping the saved record.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct DisconnectHost;

/// Forget the saved record. A live attachment to it keeps running: the record
/// is a credential, not the connection.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct ForgetHost(String);

/// The client's own identity and the action that re-points this window at a
/// different host.
///
/// It is deliberately separate from hosting: a client that can never host still
/// needs saved hosts, a device name and a way to switch attachment.
pub struct ClientAttachment {
    host: Rc<dyn ClientHost>,
    local: bool,
    switch: SwitchAttachment,
}

impl Global for ClientAttachment {}

impl ClientAttachment {
    /// `local` is whether bootstrap gave this window a host inside its own
    /// process. A phone or browser has none, so "back to local" is not a thing
    /// it can be offered.
    pub fn new(
        host: Rc<dyn ClientHost>,
        local: bool,
        switch: impl Fn(AttachmentTarget, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            host,
            local,
            switch: Rc::new(switch),
        }
    }

    /// Whether [`AttachmentTarget::Local`] is reachable from this window.
    pub fn can_attach_local(&self) -> bool {
        self.local
    }

    pub fn host(&self) -> Rc<dyn ClientHost> {
        self.host.clone()
    }

    pub fn switcher(&self) -> SwitchAttachment {
        self.switch.clone()
    }

    pub fn hosts(&self) -> Vec<PairedHost> {
        self.host.load_hosts()
    }

    pub fn save_host(&self, host: PairedHost) {
        let mut hosts = self.hosts();
        hosts.retain(|existing| existing.host_id != host.host_id);
        hosts.push(host);
        self.host.save_hosts(&hosts);
    }

    pub fn remove_host(&self, host_id: &str) {
        let mut hosts = self.hosts();
        hosts.retain(|existing| existing.host_id != host_id);
        self.host.save_hosts(&hosts);
    }
}

/// Open a *client-local* path in the user's editor through whatever integration
/// this client was given. `None` when it has none.
pub(crate) fn open_in_editor(path: &std::path::Path, cx: &App) -> Option<Result<(), String>> {
    cx.try_global::<ClientAttachment>()?
        .host
        .open_in_editor(path)
}

/// Page inset: the compact 16pt margin, a little more room when the same list
/// runs inside the wide content column.
const PAGE_PADDING: f32 = 16.;
/// The Hosts content column, matching the settings and chat reading measure.
const CONTENT_MAX_WIDTH: f32 = 768.;

type Row = gpui::Stateful<gpui::Div>;

pub struct RemotePanel {
    /// The window's current attachment, when it has one. Hosts is also the
    /// root of a window that has none.
    store: Option<Entity<WorkspaceStore>>,
    /// Navigation: "Pair a host" pushes [`Destination::Pair`], which Back pops
    /// back to whatever asked for it.
    window_state: Entity<WindowState>,
    form: PairForm,
    _subscriptions: Vec<Subscription>,
}

impl RemotePanel {
    pub fn new(
        store: Option<Entity<WorkspaceStore>>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fixed = cx
            .try_global::<ClientAttachment>()
            .and_then(|attachment| attachment.host.fixed_pairing_endpoint());
        let form = PairForm::new(fixed, window, cx);
        let mut subscriptions = Vec::new();
        // An invite pasted into either field fills the whole form.
        for input in [&form.address, &form.code] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this: &mut Self, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        let value = input.read(cx).value().to_string();
                        if value.trim().starts_with("tcode://pair?") {
                            this.form.fill_invite(&value, window, cx);
                        }
                        cx.notify();
                    } else if matches!(
                        event,
                        InputEvent::PressEnter {
                            shift: false,
                            secondary: false
                        }
                    ) {
                        if *input == this.form.address {
                            this.form
                                .code
                                .update(cx, |state, cx| state.focus(window, cx));
                        } else {
                            this.submit(window, cx);
                        }
                    }
                },
            ));
        }
        Self {
            store,
            window_state,
            form,
            _subscriptions: subscriptions,
        }
    }

    /// Follow the window onto another attachment, or off every attachment.
    pub fn set_store(&mut self, store: Option<Entity<WorkspaceStore>>, cx: &mut Context<Self>) {
        self.store = store;
        cx.notify();
    }

    pub(crate) fn set_pairing_error(&mut self, error: Option<String>) {
        self.form.error = error;
    }

    fn client(&self, cx: &App) -> Option<Rc<dyn ClientHost>> {
        cx.try_global::<ClientAttachment>()
            .map(ClientAttachment::host)
    }

    fn discover(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.client(cx) else {
            return;
        };
        let generation = self.form.restart();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let found = host.browse_hosts().await;
            let _ = this.update(cx, |panel, cx| {
                if panel.form.accept_browse(generation, found) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Read an invite off the camera. The scanned link goes through the same
    /// parser a pasted one does, pin included.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.client(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let scanned = host.scan_qr().await;
            let _ = this.update_in(cx, |panel, window, cx| {
                match scanned {
                    Ok(value) => {
                        if !panel.form.fill_invite(&value, window, cx) {
                            panel.form.error =
                                Some(crate::tr!("hosts.pair.bad_invite").into_owned());
                        }
                    }
                    Err(error) => panel.form.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(host) = self.form.take_paired() {
            let switch = cx.global::<ClientAttachment>().switcher();
            cx.global::<ClientAttachment>().save_host(host.clone());
            switch(AttachmentTarget::Remote(host), window, cx);
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some((request, generation)) = self.form.begin_pair(cx) else {
            return;
        };
        let address = request.origin.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.pair(request).await;
            let _ = this.update(cx, |panel, cx| {
                if let Ok(host) = &result {
                    cx.global::<ClientAttachment>().save_host(host.clone());
                }
                if panel.form.finish_pair(generation, result, &address) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Start a fresh pairing attempt on the Pair page.
    fn open_pair(&mut self, cx: &mut Context<Self>) {
        self.form.error = None;
        self.window_state
            .update(cx, |state, cx| state.go(Destination::Pair, cx));
        cx.notify();
    }

    fn attached_host_id(&self, cx: &App) -> Option<String> {
        self.store
            .as_ref()?
            .read(cx)
            .remote_host_id()
            .map(str::to_owned)
    }

    fn attached_locally(&self, cx: &App) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.read(cx).remote_host_id().is_none())
    }

    /// The dot that says how this window's link to the attached host is doing.
    fn status_glyph(&self, cx: &App) -> AnyElement {
        let color = match self
            .store
            .as_ref()
            .map(|store| store.read(cx).connection_state())
        {
            Some(tcode_client::ConnectionState::Connected) | None => cx.theme().success,
            Some(
                tcode_client::ConnectionState::Syncing
                | tcode_client::ConnectionState::Reconnecting { .. },
            ) => cx.theme().warning,
            Some(tcode_client::ConnectionState::Offline { .. }) => cx.theme().danger,
        };
        div()
            .flex_none()
            .size(px(8.))
            .rounded_full()
            .bg(color)
            .into_any_element()
    }

    /// "This computer": one target among the saved hosts, offered only where
    /// bootstrap actually gave this window a local host to attach to.
    fn local_row(&self, cx: &mut Context<Self>) -> Option<Row> {
        cx.try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local)
            .then(|| {
                let current = self.attached_locally(cx);
                list_row(
                    "hosts-local",
                    crate::tr!("hosts.this_computer").into_owned().into(),
                    cx,
                )
                // Same anatomy as a saved machine's row: title over one muted
                // subtitle, no leading icon, the status glyph in the same slot.
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_medium()
                                .truncate()
                                .child(crate::tr!("hosts.this_computer")),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(crate::tr!("hosts.this_computer_description")),
                        ),
                )
                .when(current, |row| row.child(self.status_glyph(cx)))
                .on_click(|_, window, cx| {
                    let switch = cx.global::<ClientAttachment>().switcher();
                    switch(AttachmentTarget::Local, window, cx);
                })
            })
    }

    /// Saved machines offer pairing again when authorization is rejected.
    fn host_row(&self, host: &PairedHost, current: bool, cx: &mut Context<Self>) -> AnyElement {
        let reason = if current {
            self.store
                .as_ref()
                .and_then(|store| match store.read(cx).connection_state() {
                    tcode_client::ConnectionState::Offline { reason } => Some(*reason),
                    tcode_client::ConnectionState::Reconnecting { reason, .. } => *reason,
                    _ => None,
                })
        } else {
            None
        };
        let needs_pairing = reason == Some(tcode_client::ConnectionFailure::AuthenticationRejected);
        let subtitle = format!(
            "{} · {}",
            host.origin,
            match host.last_connected_unix {
                Some(unix) => crate::tr!(
                    "hosts.last_connected",
                    ago = crate::time::humanize_ago(crate::time::now_secs().saturating_sub(unix))
                )
                .into_owned(),
                None => crate::tr!("hosts.never_connected").into_owned(),
            }
        );
        let name = SharedString::from(host.name.clone());
        let connect_host = host.clone();
        let repair_host = host.clone();
        let row = list_row(
            SharedString::from(format!("host-{}", host.host_id)),
            name.clone(),
            cx,
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(15.))
                        .font_medium()
                        .truncate()
                        .child(name.clone()),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(subtitle),
                )
                .when_some(reason, |column, reason| {
                    column.child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().danger_foreground)
                            .child(failure_label(reason)),
                    )
                }),
        )
        .when(current, |row| row.child(self.status_glyph(cx)))
        .when(needs_pairing, |row| {
            row.child(
                Button::new(SharedString::from(format!("repair-{}", host.host_id)))
                    .primary()
                    .compact()
                    .label(crate::tr!("hosts.pair_again"))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.form.restart();
                        panel.form.browsing = false;
                        panel
                            .form
                            .fill_discovered(repair_host.origin.clone(), window, cx);
                        panel.open_pair(cx);
                    })),
            )
        })
        .when(!needs_pairing, |row| {
            row.on_click(move |_, window, cx| {
                let switch = cx.global::<ClientAttachment>().switcher();
                switch(AttachmentTarget::Remote(connect_host.clone()), window, cx);
            })
        })
        // The menu lives *inside* the row rather than beside it, so the row's
        // hover fill covers the whole row instead of stopping short of a seam
        // next to the trigger. The trigger occludes, so it keeps its own hit
        // region and opening it never also connects the row.
        .child(self.host_menu(host, current));
        row.into_any_element()
    }

    /// The row's own overflow menu. It carries its own hit region, so opening
    /// it can never also connect the row underneath.
    fn host_menu(&self, host: &PairedHost, current: bool) -> AnyElement {
        let host_id = host.host_id.clone();
        let label = crate::tr!("hosts.actions", name = host.name.clone()).into_owned();
        div()
            .flex_none()
            .size(px(crate::material::TOUCH_TARGET))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("host-menu-{host_id}")))
                    .ghost()
                    .icon(IconName::Ellipsis)
                    .aria_label(label)
                    .dropdown_menu(move |menu, _window, _cx| {
                        let menu = menu.when(current, |menu| {
                            menu.menu(
                                crate::tr!("hosts.disconnect").into_owned(),
                                Box::new(DisconnectHost),
                            )
                        });
                        menu.menu(
                            crate::tr!("hosts.forget").into_owned(),
                            Box::new(ForgetHost(host_id.clone())),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Hosts advertised on this network. A fixed-origin client (a browser) can
    /// only pair with the origin that served it, so the section is absent there.
    fn nearby(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.form.has_fixed_endpoint() {
            return None;
        }
        let mut rows: Vec<AnyElement> = Vec::new();
        if self.form.discovered.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .px(px(PAGE_PADDING))
                    .py_3()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.form.browsing {
                        crate::tr!("hosts.nearby_searching")
                    } else {
                        crate::tr!("hosts.nearby_empty")
                    })
                    .into_any_element(),
            );
        }
        for beacon in &self.form.discovered {
            let origin = beacon.origin.clone();
            let name = SharedString::from(beacon.name.clone());
            rows.push(
                list_row(
                    SharedString::from(format!("nearby-{}-{}", beacon.host_id, beacon.origin)),
                    name.clone(),
                    cx,
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(div().text_size(px(15.)).truncate().child(name))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(beacon.origin.clone()),
                        ),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                )
                // Discovery carries no code: prefill the origin so only digits remain.
                .on_click(cx.listener(move |panel, _, window, cx| {
                    panel.form.fill_discovered(origin.clone(), window, cx);
                    panel.open_pair(cx);
                }))
                .into_any_element(),
            );
        }
        Some(
            v_flex()
                .w_full()
                .child(
                    h_flex()
                        .w_full()
                        .pr(px(PAGE_PADDING))
                        .items_center()
                        .justify_between()
                        .child(list_caption(
                            crate::tr!("hosts.nearby").into_owned().into(),
                            cx,
                        ))
                        .child(
                            Button::new("hosts-refresh")
                                .ghost()
                                .compact()
                                .loading(self.form.browsing)
                                .label(crate::tr!("hosts.refresh"))
                                .on_click(cx.listener(|panel, _, _, cx| panel.discover(cx))),
                        ),
                )
                .child(plain_list(rows, cx))
                .into_any_element(),
        )
    }

    /// The whole Hosts surface: this computer, the saved hosts, one way to add
    /// another, and whatever is on the network.
    pub(crate) fn render_hosts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A browser that has not paired with its own origin has exactly one
        // thing to do here, so it is the page rather than a hop away from it.
        let hosts = self
            .client(cx)
            .map(|client| client.load_hosts())
            .unwrap_or_default();
        if self.form.has_fixed_endpoint() && hosts.is_empty() {
            return self.render_pair(window, cx);
        }
        let current_id = self.attached_host_id(cx);
        let mut column = v_flex().w_full().gap_4().pt(px(8.)).pb(px(24.));
        if let Some(local) = self.local_row(cx) {
            column = column.child(plain_list(vec![local.into_any_element()], cx));
        }
        if hosts.is_empty() {
            column = column.child(
                v_flex()
                    .w_full()
                    .px(px(PAGE_PADDING))
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(15.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("hosts.empty")),
                    )
                    .child(self.pair_button(cx)),
            );
        } else {
            let rows = hosts
                .iter()
                .map(|host| {
                    let current = current_id.as_deref() == Some(host.host_id.as_str());
                    self.host_row(host, current, cx)
                })
                .collect();
            column = column
                .child(
                    v_flex()
                        .child(list_caption(
                            crate::tr!("hosts.saved").into_owned().into(),
                            cx,
                        ))
                        .child(plain_list(rows, cx)),
                )
                .child(div().px(px(PAGE_PADDING)).child(self.pair_button(cx)));
        }
        column = column.children(self.nearby(cx));
        self.page(column.into_any_element(), cx)
    }

    fn pair_button(&self, cx: &mut Context<Self>) -> AnyElement {
        Button::new("hosts-pair")
            .primary()
            .w_full()
            .label(crate::tr!("hosts.pair.title"))
            .on_click(cx.listener(|panel, _, _, cx| panel.open_pair(cx)))
            .into_any_element()
    }

    /// The pairing form: labels above full-width fields, errors under them, and
    /// the primary action pinned to the foot of the page — above the software
    /// keyboard, which the window seam already accounts for.
    pub(crate) fn render_pair(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(paired) = self.form.paired.clone() {
            return self.render_pair_confirm(&paired.name, cx);
        }
        let fixed = self.form.has_fixed_endpoint();
        let busy = self.form.busy;
        let ready = !busy && self.form.request(cx).is_some();
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr()) && !fixed;
        let body = v_flex()
            .w_full()
            .px(px(PAGE_PADDING))
            .py(px(16.))
            .gap_4()
            .child(
                div()
                    .text_size(px(15.))
                    .line_height(px(20.))
                    .min_w_0()
                    .text_color(cx.theme().muted_foreground)
                    .child(if fixed {
                        crate::tr!("hosts.pair.fixed_description")
                    } else {
                        crate::tr!("hosts.pair.description")
                    }),
            )
            .when(!fixed, |column| {
                column.child(self.field(
                    crate::tr!("hosts.pair.address").into_owned().into(),
                    &self.form.address,
                ))
            })
            .child(self.field(
                crate::tr!("hosts.pair.code").into_owned().into(),
                &self.form.code,
            ))
            .when(scannable, |column| {
                column.child(
                    Button::new("hosts-scan")
                        .ghost()
                        .outline()
                        .w_full()
                        .label(crate::tr!("hosts.pair.scan"))
                        .on_click(cx.listener(|panel, _, _, cx| panel.scan(cx))),
                )
            })
            .when(self.form.filled, |column| {
                column.child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("hosts.pair.filled")),
                )
            })
            .when_some(self.form.error.clone(), |column, error| {
                column.child(
                    div()
                        .text_size(px(13.))
                        .min_w_0()
                        .text_color(cx.theme().danger_foreground)
                        .child(error),
                )
            });
        let action = Button::new("hosts-pair-submit")
            .primary()
            .w_full()
            .loading(busy)
            .disabled(!ready)
            .label(crate::tr!("hosts.pair.action"))
            .on_click(cx.listener(|panel, _, window, cx| panel.submit(window, cx)));
        self.page_with_footer(body.into_any_element(), action.into_any_element(), cx)
    }

    /// Confirm the machine name before attaching its workspace.
    fn render_pair_confirm(&self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let body = v_flex()
            .w_full()
            .px(px(PAGE_PADDING))
            .py(px(16.))
            .child(name.to_owned());
        let action = Button::new("hosts-pair-connect")
            .primary()
            .w_full()
            .label(crate::tr!("hosts.pair.connect_host", name = name).into_owned())
            .on_click(cx.listener(|panel, _, window, cx| panel.submit(window, cx)));
        self.page_with_footer(body.into_any_element(), action.into_any_element(), cx)
    }

    /// One labelled field: the label above a full-width control, never a
    /// fixed-width label column beside it.
    fn field(&self, label: SharedString, state: &Entity<InputState>) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_1p5()
            .child(div().text_size(px(13.)).font_medium().child(label))
            .child(
                Input::new(state)
                    .large()
                    .rounded(crate::material::radius_input()),
            )
    }

    /// The scrolling page body, centered in the wide content column and
    /// full-bleed in compact.
    fn page(&self, body: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        div()
            .id("hosts-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .touch_overflow_y_scroll()
            .on_action(cx.listener(Self::on_disconnect))
            .on_action(cx.listener(Self::on_forget))
            .child(
                h_flex().w_full().justify_center().child(
                    div()
                        .w_full()
                        .when(!compact, |column| column.max_w(px(CONTENT_MAX_WIDTH)))
                        .child(body),
                ),
            )
            .into_any_element()
    }

    fn page_with_footer(
        &self,
        body: AnyElement,
        action: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .child(self.page(body, cx))
            .child(
                h_flex().flex_none().w_full().justify_center().child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .px(px(PAGE_PADDING))
                        .pb(px(PAGE_PADDING))
                        .pt(px(8.))
                        .child(action),
                ),
            )
            .into_any_element()
    }

    fn on_disconnect(&mut self, _: &DisconnectHost, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving a host is an explicit act; the saved record stays. A client
        // with a host of its own falls back to it rather than to nothing.
        if cx
            .try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local)
        {
            let switch = cx.global::<ClientAttachment>().switcher();
            switch(AttachmentTarget::Local, window, cx);
        } else {
            crate::shell::detach_current(cx);
        }
        cx.notify();
    }

    fn on_forget(&mut self, action: &ForgetHost, _window: &mut Window, cx: &mut Context<Self>) {
        cx.global::<ClientAttachment>().remove_host(&action.0);
        cx.notify();
    }
}

/// The same recovery wording is used in the shell and the machine row.
pub(crate) fn failure_label(reason: tcode_client::ConnectionFailure) -> String {
    use tcode_client::ConnectionFailure::*;
    match reason {
        Unreachable => crate::tr!("remote.failure.unreachable"),
        Timeout => crate::tr!("remote.failure.timeout"),
        AuthenticationRejected => crate::tr!("remote.failure.authentication_rejected"),
        ProtocolMismatch => crate::tr!("remote.failure.protocol_mismatch"),
        HostClosed => crate::tr!("remote.failure.host_closed"),
    }
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Focusable as _, Render, TestAppContext};

    struct PairingProbe(Entity<RemotePanel>);

    impl Render for PairingProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn address_enter_moves_focus_to_connection_code(cx: &mut TestAppContext) {
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            PairingProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        probe.update_in(cx, |probe, window, cx| {
            let address = probe.0.read(cx).form.address.clone();
            address.update(cx, |input, cx| {
                input.focus(window, cx);
                cx.emit(InputEvent::PressEnter {
                    shift: false,
                    secondary: false,
                });
            });
        });
        cx.run_until_parked();
        probe.update_in(cx, |probe, window, cx| {
            assert!(
                probe
                    .0
                    .read(cx)
                    .form
                    .code
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
    }
}
