//! Settings → Remote.
//!
//! Two independent halves that used to be one feature:
//!
//! - **Connecting** — saved hosts, discovery, the pair form, certificate repair
//!   and switching this window's attachment. It needs `tcode_client` and the
//!   attachment owner's switch action, and nothing else: it compiles on every
//!   client, including `--no-default-features`.
//! - **Hosting** — the listener, discovery beacon, minted codes and paired
//!   devices, behind `remote-hosting`. A browser cannot listen or advertise, so
//!   that half simply does not exist there.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, Global, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_client::host::ClientHost;
use tcode_client::pairing::PairedHost;

use crate::icon::{Icon, IconName};
use crate::pairing::PairForm;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};

#[cfg(feature = "remote-hosting")]
mod hosting;

#[cfg(feature = "remote-hosting")]
pub use hosting::{RemoteController, machine_name};

pub use crate::pairing::DEFAULT_REMOTE_PORT;

/// Where this window's workspace comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTarget {
    Local,
    Remote(PairedHost),
}

pub type SwitchAttachment = Rc<dyn Fn(AttachmentTarget, &mut Window, &mut App)>;

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

pub(crate) fn section_caption(label: SharedString, cx: &App) -> AnyElement {
    div()
        .pl_3()
        .pb(px(6.))
        .text_size(px(11.))
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(label)
        .into_any_element()
}

pub(crate) fn row() -> gpui::Div {
    h_flex()
        .w_full()
        .min_h(px(44.))
        .px_3()
        .py_2p5()
        .gap_3()
        .items_center()
}

pub(crate) fn labels(title: SharedString, description: SharedString, cx: &App) -> gpui::Div {
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

pub(crate) fn note(text: SharedString, cx: &App) -> AnyElement {
    div()
        .w_full()
        .px_3()
        .py_3()
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

fn field(state: &Entity<InputState>, width: f32) -> impl IntoElement {
    div().w(px(width)).child(
        Input::new(state)
            .small()
            .rounded(crate::material::radius_input()),
    )
}

pub struct RemotePanel {
    /// The window's current attachment, when it has one. The panel is also the
    /// hosts destination of an unattached window, which has none.
    store: Option<Entity<WorkspaceStore>>,
    form: PairForm,
    #[cfg(feature = "remote-hosting")]
    hosting: hosting::HostingSection,
    _subscriptions: Vec<Subscription>,
}

impl RemotePanel {
    pub fn new(
        store: Option<Entity<WorkspaceStore>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fixed = cx
            .try_global::<ClientAttachment>()
            .and_then(|attachment| attachment.host.fixed_pairing_endpoint());
        let form = PairForm::new(fixed, window, cx);
        let mut subscriptions = Vec::new();
        // One parser for all three fields: an invite pasted anywhere fills the
        // whole form, including the fingerprint it pins.
        for input in [&form.address, &form.port, &form.code] {
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
                    }
                },
            ));
        }
        Self {
            store,
            form,
            #[cfg(feature = "remote-hosting")]
            hosting: hosting::HostingSection::new(window, cx),
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
        let address = format!("{}:{}", request.addr, request.port);
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

    fn render_connect(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let attached = self.store.as_ref().map(|store| store.read(cx));
        let current_id = attached
            .as_ref()
            .and_then(|store| store.remote_host_id().map(str::to_owned));
        let attached_locally = attached.is_some() && current_id.is_none();
        let mut column = v_flex().w_full().gap_3().child(section_caption(
            crate::tr!("remote.connect.section").into_owned().into(),
            cx,
        ));
        // "This computer" is one target among the saved hosts, offered only
        // where bootstrap actually gave this window a local host to attach to.
        let can_attach_local = cx
            .try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local);
        if can_attach_local {
            column = column.child(
                crate::material::group(cx).child(
                    row()
                        .child(labels(
                            crate::tr!("remote.connect.local").into_owned().into(),
                            crate::tr!("remote.connect.local_description")
                                .into_owned()
                                .into(),
                            cx,
                        ))
                        .child(
                            Button::new("remote-back-to-local")
                                .primary()
                                .compact()
                                .disabled(attached_locally)
                                .label(if attached_locally {
                                    crate::tr!("remote.hosts.current")
                                } else {
                                    crate::tr!("remote.connect.back_to_local")
                                })
                                .on_click(|_, window, cx| {
                                    let switch = cx.global::<ClientAttachment>().switcher();
                                    switch(AttachmentTarget::Local, window, cx);
                                }),
                        ),
                ),
            );
        }
        column
            .child(self.render_paired_hosts(current_id.as_deref(), cx))
            .children(self.render_discovery(cx))
            .child(self.render_pair_form(cx))
            .into_any_element()
    }

    /// Saved hosts, plus the repair path when a host's certificate no longer
    /// matches the one that was pinned: the row refuses to connect and offers to
    /// pair again instead, which is the only way to accept a new certificate.
    fn render_paired_hosts(&self, current_id: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let client = self.client(cx);
        let hosts = client
            .as_ref()
            .map(|host| host.load_hosts())
            .unwrap_or_default();
        let mut group = crate::material::group(cx);
        if hosts.is_empty() {
            group = group.child(note(
                crate::tr!("remote.hosts.empty").into_owned().into(),
                cx,
            ));
        }
        for host in hosts {
            let current = current_id == Some(host.host_id.as_str());
            let changed = client
                .as_ref()
                .is_some_and(|client| client.certificate_changed(&host.host_id));
            let connect_host = host.clone();
            let repair_host = host.clone();
            let remove_id = host.host_id.clone();
            let address = host
                .addrs
                .first()
                .cloned()
                .unwrap_or_else(|| "?".to_owned());
            let description = if changed {
                crate::tr!("remote.hosts.certificate_changed").into_owned()
            } else {
                format!(
                    "{address}:{} · {}",
                    host.port,
                    crate::tr!(
                        "remote.pair.fingerprint",
                        fingerprint = tcode_client::pairing::display_fingerprint(&host.fingerprint)
                    )
                )
            };
            group = group.child(
                row()
                    .child(labels(host.name.clone().into(), description.into(), cx))
                    .when(changed, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("repair-{}", host.host_id)))
                                .primary()
                                .compact()
                                .label(crate::tr!("remote.hosts.pair_again"))
                                .on_click(cx.listener(move |panel, _, window, cx| {
                                    panel.form.restart();
                                    panel.form.browsing = false;
                                    panel.form.pin_discovered(
                                        repair_host.addrs.first().cloned().unwrap_or_default(),
                                        repair_host.port,
                                        String::new(),
                                        window,
                                        cx,
                                    );
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!changed, |row| {
                        row.child(
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
                                    let switch = cx.global::<ClientAttachment>().switcher();
                                    switch(
                                        AttachmentTarget::Remote(connect_host.clone()),
                                        window,
                                        cx,
                                    );
                                }),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("remove-{}", host.host_id)))
                            .ghost()
                            .compact()
                            .label(crate::tr!("remote.hosts.remove"))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                let id = remove_id.clone();
                                cx.global::<ClientAttachment>().remove_host(&id);
                                cx.notify();
                            })),
                    ),
            );
        }
        v_flex()
            .child(section_caption(
                crate::tr!("remote.hosts.section").into_owned().into(),
                cx,
            ))
            .child(group)
            .into_any_element()
    }

    /// Local-network discovery. A fixed-origin client (a browser) can only pair
    /// with the origin that served it, so the whole section is absent there.
    fn render_discovery(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.form.has_fixed_endpoint() {
            return None;
        }
        let mut group = crate::material::group(cx).child(
            row()
                .child(labels(
                    crate::tr!("remote.discover.title").into_owned().into(),
                    crate::tr!("remote.discover.description")
                        .into_owned()
                        .into(),
                    cx,
                ))
                .child(
                    Button::new("remote-discover")
                        .ghost()
                        .outline()
                        .compact()
                        .loading(self.form.browsing)
                        .label(crate::tr!("remote.discover.search"))
                        .on_click(cx.listener(|this, _, _, cx| this.discover(cx))),
                ),
        );
        if !self.form.browsing && self.form.discovered.is_empty() {
            group = group.child(note(
                crate::tr!("remote.discover.none").into_owned().into(),
                cx,
            ));
        }
        for beacon in &self.form.discovered {
            let (addr, port, fingerprint) = (beacon.addr.clone(), beacon.port, beacon.fp.clone());
            group = group.child(
                row()
                    .child(labels(
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
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                // Discovery carries no code: prefill the endpoint and
                                // pin its fingerprint so the user only types digits.
                                this.form.pin_discovered(
                                    addr.clone(),
                                    port,
                                    fingerprint.clone(),
                                    window,
                                    cx,
                                );
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        Some(
            v_flex()
                .child(section_caption(
                    crate::tr!("remote.discover.section").into_owned().into(),
                    cx,
                ))
                .child(group)
                .into_any_element(),
        )
    }

    fn render_pair_form(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(paired) = &self.form.paired {
            return self.render_pair_confirm(&paired.name.clone(), cx);
        }
        let busy = self.form.busy;
        let ready = !busy && self.form.request(cx).is_some();
        let fixed = self.form.has_fixed_endpoint();
        let body = v_flex()
            .w_full()
            .gap_2()
            .px_3()
            .py_3()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if fixed {
                        crate::tr!("remote.pair.fixed_origin_description")
                    } else {
                        crate::tr!("remote.pair.description")
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .when(!fixed, |row| {
                        row.child(field(&self.form.address, 200.))
                            .child(field(&self.form.port, 84.))
                    })
                    .child(field(&self.form.code, 110.))
                    .child(
                        Button::new("remote-pair")
                            .primary()
                            .compact()
                            .loading(busy)
                            .disabled(!ready)
                            .label(crate::tr!("remote.pair.action"))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
            .when(self.form.filled, |column| {
                column.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("remote.pair.filled")),
                )
            })
            .when(!self.form.fingerprint.is_empty(), |column| {
                column.child(div().text_size(px(12.)).child(crate::tr!(
                    "remote.pair.fingerprint",
                    fingerprint =
                        tcode_client::pairing::display_fingerprint(&self.form.fingerprint)
                )))
            })
            .when_some(self.form.error.clone(), |column, error| {
                column.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger_foreground)
                        .child(error),
                )
            });
        v_flex()
            .child(section_caption(
                crate::tr!("remote.pair.section").into_owned().into(),
                cx,
            ))
            .child(crate::material::group(cx).child(body))
            .into_any_element()
    }

    /// Paired, not yet connected: show the pinned fingerprint next to the one
    /// the host displays, so a swapped certificate is caught before any traffic.
    fn render_pair_confirm(&self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .child(section_caption(
                crate::tr!("remote.pair.section").into_owned().into(),
                cx,
            ))
            .child(
                crate::material::group(cx).child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .py_3()
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(14.))
                                .child(tcode_client::pairing::display_fingerprint(
                                    &self.form.fingerprint,
                                )),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::tr!("remote.pair.fingerprint_compare")),
                        )
                        .child(
                            h_flex().child(
                                Button::new("remote-pair-connect")
                                    .primary()
                                    .compact()
                                    .label(
                                        crate::tr!("remote.pair.connect_host", name = name)
                                            .into_owned(),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                        ),
                ),
            )
            .into_any_element()
    }
}

impl Render for RemotePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let column = v_flex().w_full().gap_6().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::Info).xsmall())
                .child(
                    div()
                        .text_size(px(13.))
                        .child(if cfg!(feature = "remote-hosting") {
                            crate::tr!("remote.intro")
                        } else {
                            // A client that cannot listen has nothing to host with.
                            crate::tr!("remote.intro_client")
                        }),
                ),
        );
        #[cfg(feature = "remote-hosting")]
        let column = column.child(self.render_hosting(cx));
        column.child(self.render_connect(cx))
    }
}
