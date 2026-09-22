//! Hosts: which host this window talks to.
//!
//! This is a product surface, not a settings page. It answers one question —
//! *which host am I talking to* — with saved hosts, pairing by invitation and
//! authentication repair, and it is reached from the sidebar's feature area at
//! every width. It needs `tcode_client` and the attachment owner's switch
//! action, and nothing else: it compiles on every client, including
//! `--no-default-features`.
//!
//! **Hosting** — the endpoint, minted invitations and paired devices — is a genuine
//! setting of *this machine* and lives in
//! `hosting`, behind `remote-hosting`, inside Settings → Remote. The browser
//! uses `hosted` to control its headless listener over the authenticated pipe.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, Context, Entity, Global, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, StyledExt as _, h_flex, v_flex};
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
        self.host.remember_host(host);
    }

    pub fn remove_host(&self, host_id: &str) {
        self.host.remove_host(host_id);
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
    /// [`ClientHost::fixed_machine`]: a browser lists its one machine and
    /// no way to add or re-pair one.
    fixed_machine: bool,
    page_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl RemotePanel {
    pub fn new(
        store: Option<Entity<WorkspaceStore>>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fixed_machine = cx
            .try_global::<ClientAttachment>()
            .is_some_and(|attachment| attachment.host.fixed_machine());
        let form = PairForm::new(window, cx);
        let subscriptions = vec![cx.subscribe_in(
            &form.invitation,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.form.error = None;
                    cx.notify();
                }
                InputEvent::PressEnter {
                    shift: false,
                    secondary: false,
                } => this.submit(window, cx),
                _ => {}
            },
        )];
        Self {
            store,
            window_state,
            form,
            fixed_machine,
            page_scroll: ScrollHandle::new(),
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

    /// Read an invite off the camera. The scanned link goes through the same
    /// parser a pasted one does, pin included.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.client(cx) else {
            return;
        };
        if self.window_state.read(cx).destination() != Destination::Pair {
            self.open_pair(cx);
        }
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
        let address = fingerprint(&request.host_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.pair(request).await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_pair(generation, result, &address, cx);
            });
        })
        .detach();
    }

    fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, String>,
        address: &str,
        cx: &mut Context<Self>,
    ) {
        if self.form.finish_pair(generation, result.clone(), address) {
            if let Ok(host) = result {
                cx.global::<ClientAttachment>().save_host(host);
            }
            cx.notify();
        }
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
        let color = self
            .store
            .as_ref()
            .map(|store| {
                cx.theme()
                    .connection_color(&store.read(cx).connection_state())
            })
            .unwrap_or(cx.theme().success);
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
                // Same anatomy as a saved machine's row: no leading icon, the
                // status glyph in the same slot.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(15.))
                        .font_medium()
                        .truncate()
                        .child(crate::tr!("hosts.this_computer")),
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
                    tcode_client::ConnectionState::Offline { reason } => Some(reason),
                    tcode_client::ConnectionState::Reconnecting { reason, .. } => reason,
                    _ => None,
                })
        } else {
            None
        };
        let needs_pairing = reason == Some(tcode_client::ConnectionFailure::AuthenticationRejected)
            && !self.fixed_machine;
        let subtitle = format!(
            "{} · {}",
            fingerprint(&host.host_id),
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
        let row = list_row(
            SharedString::from(format!("host-{}", host.host_id)),
            name.clone(),
            cx,
        )
        .debug_selector(|| format!("host-{}", host.host_id))
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
                    // A new pairing needs a new invitation from the machine;
                    // nothing from the stale record carries over.
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.form.restart();
                        panel.form.clear(window, cx);
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

    /// The whole Hosts surface: this computer, the saved hosts and, where
    /// the client can pair, the ways to add another.
    pub(crate) fn render_hosts(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hosts = self
            .client(cx)
            .map(|client| client.load_hosts())
            .unwrap_or_default();
        let current_id = self.attached_host_id(cx);
        let mut column = v_flex().w_full().gap_4().pt(px(8.)).pb(px(24.));
        if let Some(local) = self.local_row(cx) {
            column = column.child(plain_list(vec![local.into_any_element()], cx));
        }
        if hosts.is_empty() {
            column = column.child(
                div()
                    .w_full()
                    .px(px(PAGE_PADDING))
                    .text_size(px(15.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("hosts.empty")),
            );
        } else {
            let rows = hosts
                .iter()
                .map(|host| {
                    let current = current_id.as_deref() == Some(host.host_id.as_str());
                    self.host_row(host, current, cx)
                })
                .collect();
            column = column.child(
                v_flex()
                    .child(list_caption(
                        crate::tr!("hosts.saved").into_owned().into(),
                        cx,
                    ))
                    .child(plain_list(rows, cx)),
            );
        }
        if !self.fixed_machine {
            column = column.child(self.add_machine(cx));
        }
        self.page(column.into_any_element(), cx)
    }

    /// The ways in: the camera where there is one, and the invitation field
    /// everywhere. Both lead to the same form; a first pairing is always by
    /// invitation, never by finding a machine on the network.
    fn add_machine(&self, cx: &mut Context<Self>) -> AnyElement {
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        let entry = |id: &'static str,
                     title: SharedString,
                     subtitle: SharedString,
                     cx: &mut Context<Self>| {
            list_row(id, title.clone(), cx)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(div().text_size(px(15.)).truncate().child(title))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(subtitle),
                        ),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                )
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if scannable {
            rows.push(
                entry(
                    "hosts-scan",
                    crate::tr!("hosts.pair.scan").into_owned().into(),
                    crate::tr!("hosts.pair.scan_description")
                        .into_owned()
                        .into(),
                    cx,
                )
                .on_click(cx.listener(|panel, _, _, cx| panel.scan(cx)))
                .into_any_element(),
            );
        }
        rows.push(
            entry(
                "hosts-pair",
                crate::tr!("hosts.pair.paste").into_owned().into(),
                crate::tr!("hosts.pair.paste_description")
                    .into_owned()
                    .into(),
                cx,
            )
            // An invitation is single use, so whatever the field held last
            // time is spent; start empty.
            .on_click(cx.listener(|panel, _, window, cx| {
                panel.form.clear(window, cx);
                panel.open_pair(cx);
            }))
            .into_any_element(),
        );
        v_flex()
            .w_full()
            .debug_selector(|| "hosts-add-machine".into())
            .child(list_caption(
                crate::tr!("hosts.pair.title").into_owned().into(),
                cx,
            ))
            .child(plain_list(rows, cx))
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
        let busy = self.form.busy;
        let request = self.form.request(cx);
        let ready = !busy && request.is_some();
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        // A pairing failure outranks the field's own complaint.
        let error = self.form.error.clone().or_else(|| {
            self.form
                .invalid(cx)
                .then(|| crate::tr!("hosts.pair.bad_invite").into_owned())
        });
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
                    .child(crate::tr!("hosts.pair.description")),
            )
            .child(self.field(
                crate::tr!("hosts.pair.invitation").into_owned().into(),
                &self.form.invitation,
            ))
            .when_some(request, |column, invite| {
                column.child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!(
                            "hosts.pair.filled",
                            name = invite.name.clone(),
                            fingerprint = fingerprint(&invite.host_id)
                        )),
                )
            })
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
            .when_some(error, |column, error| {
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
        crate::scroll::page_viewport(
            "hosts-scroll-bounce",
            crate::wheel_easing::Handle::Scroll(self.page_scroll.clone()),
            div()
                .id("hosts-scroll")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.page_scroll)
                .on_action(cx.listener(Self::on_disconnect))
                .on_action(cx.listener(Self::on_forget))
                .child(
                    h_flex().w_full().justify_center().child(
                        div()
                            .w_full()
                            .when(!compact, |column| column.max_w(px(CONTENT_MAX_WIDTH)))
                            .child(body),
                    ),
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

/// A connected device is listed by name, then its operating system when the
/// device reported one — the same ` · ` separator used elsewhere.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) fn device_label(name: &str, platform: Option<&str>) -> String {
    match platform {
        Some(platform) => format!("{name} · {platform}"),
        None => name.to_owned(),
    }
}

/// The leading characters of a machine id: enough to tell machines apart by
/// eye and to check against the hosting page, never the id itself.
pub(crate) fn fingerprint(host_id: &str) -> String {
    host_id.chars().take(8).collect()
}

/// A device's status column: how it is connected, or that it is not.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) fn path_label(path: Option<&tcode_protocol::PathInfo>) -> String {
    match path {
        None => crate::tr!("remote.path.offline").into_owned(),
        Some(path) if path.direct => crate::tr!("remote.path.direct").into_owned(),
        Some(path) => match path.relay.as_deref().and_then(relay_host) {
            Some(host) => crate::tr!("remote.path.relay_via", host = host).into_owned(),
            None => crate::tr!("remote.path.relay").into_owned(),
        },
    }
}

/// The host of a relay URL, which is all a person needs to recognise it.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
fn relay_host(relay: &str) -> Option<String> {
    url::Url::parse(relay).ok()?.host_str().map(str::to_owned)
}

/// How this window's link to its machine is doing, one line under the
/// machine's name: the path while the link is up, the failure while it is
/// not. A transport that cannot tell how it is carried (a browser) says only
/// that it is connected.
pub(crate) fn connection_label(state: &tcode_client::ConnectionState) -> String {
    use tcode_client::ConnectionState;
    match state {
        ConnectionState::Connected { path } => {
            let connected = crate::tr!("remote.state.connected");
            match path {
                Some(path) if path.direct => {
                    format!("{} · {connected}", crate::tr!("remote.path.direct"))
                }
                Some(_) => format!("{} · {connected}", crate::tr!("remote.path.relay")),
                None => connected.into_owned(),
            }
        }
        ConnectionState::Syncing => crate::tr!("remote.state.syncing").into_owned(),
        ConnectionState::Reconnecting { attempt, reason } => {
            let text = crate::tr!("remote.state.reconnecting", attempt = attempt);
            match reason {
                Some(reason) => format!("{text} · {}", failure_label(*reason)),
                None => text.into_owned(),
            }
        }
        ConnectionState::Offline { reason } => format!(
            "{} · {}",
            crate::tr!("remote.path.offline"),
            failure_label(*reason)
        ),
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
    use gpui::{AppContext as _, Render, TestAppContext};

    /// The line names the path while the link is up, the attempt while it
    /// is being restored and the failure once it is lost; a transport that
    /// cannot tell how it is carried names no path.
    #[test]
    fn the_connection_label_names_the_path_or_the_failure() {
        use tcode_client::{ConnectionFailure, ConnectionState};
        use tcode_protocol::PathInfo;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let direct = PathInfo {
            direct: true,
            relay: None,
        };
        let relayed = PathInfo {
            direct: false,
            relay: Some("https://relay.example/".into()),
        };
        assert_eq!(
            connection_label(&ConnectionState::Connected { path: Some(direct) }),
            "Direct · Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Connected {
                path: Some(relayed)
            }),
            "Relay · Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Connected { path: None }),
            "Connected"
        );
        assert_eq!(connection_label(&ConnectionState::Syncing), "Syncing");
        assert_eq!(
            connection_label(&ConnectionState::Reconnecting {
                attempt: 3,
                reason: None
            }),
            "Reconnecting (attempt 3)"
        );
        assert_eq!(
            connection_label(&ConnectionState::Reconnecting {
                attempt: 3,
                reason: Some(ConnectionFailure::Timeout)
            }),
            "Reconnecting (attempt 3) · Connection timed out"
        );
        assert_eq!(
            connection_label(&ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected
            }),
            "Offline · Access rejected · Pair again"
        );
    }

    struct PairingProbe(Entity<RemotePanel>);

    impl Render for PairingProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The browser is signed in with the machine that served it: its Hosts
    /// page lists that machine and nothing that would add or re-pair one.
    #[gpui::test]
    fn a_fixed_machine_client_has_no_way_to_add_one(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct Browser;
        impl ClientHost for Browser {
            fn device_name(&self) -> String {
                "Safari".into()
            }
            fn device_id(&self) -> String {
                "browser".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Vec<PairedHost> {
                vec![PairedHost {
                    host_id: "served-by".into(),
                    name: "Build server".into(),
                    traverse: None,
                    relay: None,
                    addrs: Vec::new(),
                    last_connected_unix: None,
                }]
            }
            fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
                Default::default()
            }
            fn save_preferences(&self, _: &tcode_client::host::ClientPreferences) {}
            fn save_hosts(&self, _: &[PairedHost]) {}
            fn last_host_id(&self) -> Option<String> {
                Some("served-by".into())
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn fixed_machine(&self) -> bool {
                true
            }
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("the page only lists the machine")
            }
        }
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_global(ClientAttachment::new(Rc::new(Browser), false, |_, _, _| {})));
        let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            HostsProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("host-served-by").is_some(),
            "the serving machine is listed"
        );
        assert!(
            cx.debug_bounds("hosts-add-machine").is_none(),
            "a browser has no way to add a machine"
        );
    }

    #[cfg(feature = "remote-hosting")]
    #[gpui::test]
    fn superseded_pairing_does_not_overwrite_the_saved_machine(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-stale-pairing-{}",
            tcode_services::store::now_millis()
        ));
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        cx.update(|cx| cx.set_global(ClientAttachment::new(client.clone(), false, |_, _, _| {})));
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            PairingProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        let host = |name: &str| PairedHost {
            host_id: "machine".into(),
            name: name.into(),
            traverse: None,
            relay: None,
            addrs: vec!["192.168.1.10:47420".into()],
            last_connected_unix: None,
        };
        probe.update_in(cx, |probe, _, cx| {
            probe.0.update(cx, |panel, cx| {
                let old = panel.form.restart();
                let current = panel.form.restart();
                panel.finish_pair(current, Ok(host("current pairing")), "machine", cx);
                panel.finish_pair(old, Ok(host("superseded pairing")), "machine", cx);
            });
        });
        assert_eq!(client.load_hosts(), vec![host("current pairing")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
