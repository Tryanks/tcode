//! Hosts: the machines this window can show and which of them it does, and
//! which devices may talk to this one.
//!
//! This is a product surface, not a settings page. It is where connections
//! are made, in both directions: the invitation this machine shows other
//! devices, the saved machines this window can open, pairing by a scanned or
//! pasted invitation and authentication repair. It is reached from the
//! sidebar's feature area at every width. It needs `tcode_client` and the
//! window's host requests, and nothing else: it compiles on every
//! client, including `--no-default-features`.
//!
//! **Hosting** — whether the endpoint runs, its name, Traverse, whether new
//! devices are accepted, and the devices that have paired — is a genuine
//! setting of *this machine* and lives in `hosting`, behind `remote-hosting`,
//! inside Settings → Remote. The browser uses `hosted` to control its headless
//! listener over the authenticated pipe.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, Context, Entity, Global, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_client::host::{ClientHost, FileFailure};
use tcode_client::pairing::PairedHost;

use crate::icon::{Icon, IconName};
use crate::material::{
    CARD_INSET, COMPACT_PAGE_INSET, group_row, group_row_stacked, panel_row, section_gap,
};
use crate::overlay::{Notification, OverlayExt as _};
use crate::pairing::PairForm;
use crate::settings_rows::{CONTENT_MAX_WIDTH, settings_group};
use crate::store::{HostKey, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_state::{Destination, WindowState};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(feature = "remote-hosting")]
mod hosting;
#[cfg(feature = "remote-hosting")]
mod traverse;

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod access;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod group;
mod qr;
pub(crate) mod spaces;

#[cfg(target_family = "wasm")]
pub(crate) use access::AccessSection;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use group::GroupSection;
#[cfg(feature = "remote-hosting")]
pub use hosting::{HostingPanel, RemoteController, machine_name};
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use spaces::SpacesSection;
#[cfg(feature = "remote-hosting")]
pub use traverse::TraversePanel;

/// Where this window's workspace comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTarget {
    Local,
    Remote(Box<PairedHost>),
}

impl AttachmentTarget {
    pub fn key(&self) -> HostKey {
        match self {
            Self::Local => HostKey::Local,
            Self::Remote(host) => HostKey::Remote(host.host_id.clone()),
        }
    }
}

/// What the window is asked to do with a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRequest {
    /// Show `target` in the workspace beside the hosts already there; a
    /// host that is reached only alone replaces them.
    Open(AttachmentTarget),
    /// Take the host out of the workspace, keeping its saved record.
    Leave(HostKey),
    /// Move the host one place earlier, or later, among the workspace's.
    Move { host: HostKey, earlier: bool },
}

pub type RequestHost = Rc<dyn Fn(HostRequest, &mut Window, &mut App)>;

/// Leave the workspace of the host this row names, keeping the saved record.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct DisconnectHost(String);

/// Move the host this row names one place earlier, or later, among the
/// workspace's hosts.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct MoveHost {
    host_id: String,
    earlier: bool,
}

/// Forget the saved record. A live attachment to it keeps running: the record
/// is a credential, not the connection.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct ForgetHost(String);

/// The client's own identity and the way to ask this window for a host.
///
/// It is deliberately separate from hosting: a client that can never host still
/// needs saved hosts, a device name and a way to open one.
pub struct ClientAttachment {
    host: Rc<dyn ClientHost>,
    local: bool,
    request: RequestHost,
}

impl Global for ClientAttachment {}

impl ClientAttachment {
    /// `local` is whether bootstrap gave this window a host inside its own
    /// process. A phone or browser has none, so "back to local" is not a thing
    /// it can be offered.
    pub fn new(
        host: Rc<dyn ClientHost>,
        local: bool,
        request: impl Fn(HostRequest, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            host,
            local,
            request: Rc::new(request),
        }
    }

    /// Whether [`AttachmentTarget::Local`] is reachable from this window.
    pub fn can_attach_local(&self) -> bool {
        self.local
    }

    pub fn host(&self) -> Rc<dyn ClientHost> {
        self.host.clone()
    }

    pub fn requester(&self) -> RequestHost {
        self.request.clone()
    }

    pub fn save_host(&self, host: PairedHost) -> Result<(), FileFailure> {
        self.host.remember_host(host)
    }

    pub fn remove_host(&self, host_id: &str) -> Result<(), FileFailure> {
        self.host.remove_host(host_id)
    }
}

/// The name of the machine this process hosts, where it hosts one.
pub(crate) fn local_machine_name(cx: &App) -> Option<String> {
    #[cfg(feature = "remote-hosting")]
    {
        cx.try_global::<RemoteController>()
            .map(RemoteController::machine_name)
    }
    #[cfg(not(feature = "remote-hosting"))]
    {
        let _ = cx;
        None
    }
}

/// Open a *client-local* path in the user's editor through whatever integration
/// this client was given. `None` when it has none.
pub(crate) fn open_in_editor(path: &std::path::Path, cx: &App) -> Option<Result<(), String>> {
    cx.try_global::<ClientAttachment>()?
        .host
        .open_in_editor(path)
}

/// The Open in Zed entry a host's paths get.
pub(crate) struct ZedEntry {
    pub(crate) host: HostKey,
    /// Its own wording for a machine reached over Tcode.
    pub(crate) label: &'static str,
}

impl ZedEntry {
    /// `None` where the space's policy does not allow the editor, or the
    /// host is a machine and this client cannot reach one from Zed.
    pub(crate) fn of(store: &WorkspaceStore, cx: &App) -> Option<Self> {
        if !store.space_allows(|policy| policy.editor) {
            return None;
        }
        let label = if !store.is_remote() {
            "chat.open_zed"
        } else if cx
            .try_global::<ClientAttachment>()
            .is_some_and(|attachment| attachment.host.machine_editor().is_some())
        {
            "chat.open_zed_remote"
        } else {
            return None;
        };
        Some(Self {
            host: store.host_key(),
            label,
        })
    }

    /// The entry of the current pane's host; this client's own Zed where no
    /// pane is over a host.
    pub(crate) fn current(cx: &App) -> Option<Self> {
        match crate::shell::current_store(cx) {
            Some(store) => Self::of(store.read(cx), cx),
            None => Some(Self {
                host: HostKey::Local,
                label: "chat.open_zed",
            }),
        }
    }
}

/// Open `path` of `host` in Zed; `None` is the current pane's host.
pub(crate) fn open_path_in_zed(
    host: Option<&HostKey>,
    path: &std::path::Path,
    window: &mut Window,
    cx: &mut App,
) {
    let store = match host {
        Some(host) => crate::shell::store_of_host(host, cx),
        None => crate::shell::current_store(cx),
    };
    match store {
        Some(store) => open_host_path_in_zed(&store, path, window, cx),
        None if matches!(host, None | Some(HostKey::Local)) => {
            crate::widgets::menu::open_in_zed(path, window, cx)
        }
        None => {}
    }
}

/// Open `path`, a directory of `store`'s host, in Zed: on this client
/// directly when the host is the one in this process, otherwise on its
/// machine over Tcode's connection to it, after a note on what that does
/// which is also the user's agreement to Tcode's entry in Zed's settings.
pub(crate) fn open_host_path_in_zed(
    store: &Entity<WorkspaceStore>,
    path: &std::path::Path,
    window: &mut Window,
    cx: &mut App,
) {
    if ZedEntry::of(store.read(cx), cx).is_none() {
        return;
    }
    if !store.read(cx).is_remote() {
        crate::widgets::menu::open_in_zed(path, window, cx);
        return;
    }
    let Some(editor) = cx
        .try_global::<ClientAttachment>()
        .and_then(|attachment| attachment.host.machine_editor())
    else {
        return;
    };
    let Some((machine, tunnels)) = store.read(cx).machine_tunnels() else {
        window.push_notification(
            Notification::error(crate::tr!(
                "editor.zed.pipe_not_connected",
                machine = store.read(cx).host_title()
            )),
            cx,
        );
        return;
    };
    let path = path.to_string_lossy().into_owned();
    if !windows_drive_path(&path) {
        let target = tcode_client::host::MachinePath {
            host_id: machine.host_id.clone(),
            path,
            tunnels,
        };
        confirm_and_open(editor, target, machine.name, window, cx);
        return;
    }
    // Zed names the folder of an `ssh://` link by the link's path, which
    // starts with `/`; a Windows machine's server cannot resolve
    // `/C:/...`, but expands a leading `~` to its home.
    let home = store.update(cx, |store, cx| store.browse_directories(String::new(), cx));
    window
        .spawn(cx, async move |cx| {
            let relative = home.await.map(|listing| {
                listing
                    .home
                    .and_then(|home| home_relative(&path, &home.to_string_lossy()))
            });
            let _ = cx.update(|window, cx| match relative {
                Ok(Some(path)) => {
                    let target = tcode_client::host::MachinePath {
                        host_id: machine.host_id.clone(),
                        path,
                        tunnels,
                    };
                    confirm_and_open(editor, target, machine.name, window, cx);
                }
                Ok(None) => window.push_notification(
                    Notification::error(crate::tr!(
                        "editor.zed.open_failed",
                        error = crate::tr!("editor.zed.windows_outside_home")
                    )),
                    cx,
                ),
                Err(error) => window.push_notification(
                    Notification::error(crate::tr!("editor.zed.open_failed", error = error)),
                    cx,
                ),
            });
        })
        .detach();
}

/// Whether `path` is a Windows drive path, `C:\...` or `C:/...`.
fn windows_drive_path(path: &str) -> bool {
    matches!(path.as_bytes(), [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
}

/// `path` under `home` as `~/...`, both Windows paths; `None` outside it.
fn home_relative(path: &str, home: &str) -> Option<String> {
    let normal = |path: &str| path.replace('/', "\\").trim_end_matches('\\').to_owned();
    let (path, home) = (normal(path), normal(home));
    let rest = path
        .get(..home.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(&home))?;
    let rest = &path[rest.len()..];
    match rest.strip_prefix('\\') {
        Some(rest) => Some(format!("~/{}", rest.replace('\\', "/"))),
        None if rest.is_empty() => Some("~".to_owned()),
        None => None,
    }
}

/// Open `target` after the note on what reaching a machine from Zed does,
/// unless this device asked not to see it again.
fn confirm_and_open(
    editor: Rc<dyn tcode_client::host::MachineEditor>,
    target: tcode_client::host::MachinePath,
    machine_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    if crate::device::DevicePreferences::remote_zed_note_dismissed(cx) {
        open_on_machine(editor.as_ref(), target, window, cx);
        return;
    }
    let settings_file = editor
        .settings_file()
        .map(|path| path.display().to_string());
    let dismiss = Rc::new(std::cell::Cell::new(false));
    window.open_dialog(cx, move |dialog, _, _| {
        let checked = dismiss.get();
        let toggle = dismiss.clone();
        let confirm = dismiss.clone();
        let editor = editor.clone();
        let target = target.clone();
        let machine_name = machine_name.clone();
        let settings_file = settings_file.clone();
        dialog
            .close_button(false)
            .title(crate::tr!("editor.zed.remote_title").into_owned())
            .content(move |content, window, cx| {
                let compact = crate::window_seam::window_is_compact(window, cx);
                let line = |text: gpui::SharedString| div().text_secondary(compact, cx).child(text);
                content.child(
                    v_flex()
                        .gap_3()
                        .debug_selector(|| "remote-zed-note".into())
                        .child(line(
                            crate::tr!("editor.zed.remote_channel", machine = machine_name.clone())
                                .into_owned()
                                .into(),
                        ))
                        .child(line(
                            crate::tr!("editor.zed.remote_server", machine = machine_name.clone())
                                .into_owned()
                                .into(),
                        ))
                        .child(line(
                            crate::tr!("editor.zed.remote_only_zed").into_owned().into(),
                        ))
                        .children(settings_file.clone().map(|file| {
                            line(
                                crate::tr!("editor.zed.remote_settings", file = file)
                                    .into_owned()
                                    .into(),
                            )
                        }))
                        .child(
                            crate::widgets::checkbox::Checkbox::new("remote-zed-note-dismiss")
                                .label(crate::tr!("editor.zed.remote_dont_show"))
                                .checked(checked)
                                .on_click({
                                    let toggle = toggle.clone();
                                    move |checked: &bool, window, _| {
                                        toggle.set(*checked);
                                        window.refresh();
                                    }
                                }),
                        ),
                )
            })
            .footer(
                crate::overlay::DialogActions::new()
                    .child(
                        Button::new("remote-zed-note-cancel")
                            .label(crate::tr!("settings.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("remote-zed-note-open")
                            .debug_selector(|| "remote-zed-note-open".into())
                            .primary()
                            .label(crate::tr!("editor.zed.remote_open"))
                            .on_click(move |_, window, cx| {
                                if confirm.get() {
                                    crate::device::DevicePreferences::dismiss_remote_zed_note(cx);
                                }
                                window.close_dialog(cx);
                                open_on_machine(editor.as_ref(), target.clone(), window, cx);
                            }),
                    ),
            )
    });
}

fn open_on_machine(
    editor: &dyn tcode_client::host::MachineEditor,
    target: tcode_client::host::MachinePath,
    window: &mut Window,
    cx: &mut App,
) {
    let opening = cx.foreground_executor().spawn(editor.open(target));
    report("editor.zed.open_failed", opening, window, cx);
}

/// Tell the user why a change did not happen; a change that did shows itself.
/// `failed` is the message key, given the error as `error`.
fn report<T: 'static>(
    failed: &'static str,
    task: gpui::Task<Result<T, String>>,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                let _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::error(crate::tr!(failed, error = error).into_owned()),
                        cx,
                    );
                });
            }
        })
        .detach();
}

/// A page's [`footnote`](crate::settings_rows::footnote) and the link to
/// the `anchor` section of the remote guide, which says the rest.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
fn footnote_with_docs(
    text: impl Into<SharedString>,
    anchor: &str,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    crate::settings_rows::footnote(text, compact, cx)
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(crate::settings_rows::external_link(
            format!("remote-docs-{anchor}"),
            crate::tr!("remote.learn_more").into_owned().into(),
            format!("https://github.com/Tryanks/tcode/blob/main/docs/remote.md#{anchor}"),
        ))
}

/// Ask before a change that cannot be undone from here. `title` names what
/// goes, `description` says in one sentence what follows, and `action` labels
/// the danger button that runs `on_ok`.
fn confirm_destructive(
    title: String,
    description: String,
    action: String,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .button_props(
                crate::overlay::DialogButtons::default()
                    .ok_variant(crate::widgets::ButtonVariant::Danger)
                    .ok_text(action.clone())
                    .cancel_text(crate::tr!("spaces.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

/// A dialog's confirmation; `false` keeps the dialog open.
type Submit = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// A form dialog's buttons: Cancel, and `confirm`, which runs `submit` as
/// Enter does and closes the dialog when it succeeds. `id` prefixes the
/// buttons' ids.
fn submit_actions(
    dialog: crate::overlay::Dialog,
    id: &'static str,
    confirm: SharedString,
    submit: Submit,
) -> crate::overlay::Dialog {
    let ok = submit.clone();
    dialog.on_ok(move |_, window, cx| ok(window, cx)).footer(
        crate::overlay::DialogActions::new()
            .child(
                Button::new(SharedString::from(format!("{id}-cancel")))
                    .label(crate::tr!("spaces.cancel"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-ok")))
                    .debug_selector(move || format!("{id}-ok"))
                    .primary()
                    .label(confirm)
                    .on_click(move |_, window, cx| {
                        if submit(window, cx) {
                            window.close_dialog(cx);
                        }
                    }),
            ),
    )
}

type Row = crate::material::Clickable<gpui::Div>;

/// A [`panel_row`] inside one of the page's groups. Compact, a panel row is a
/// page-inset list row; inside a group it takes the group's inset, so its text
/// lines up with the caption and the group's other rows.
fn group_entry(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    compact: bool,
    cx: &App,
) -> Row {
    panel_row(id, label, compact, cx).px(px(CARD_INSET))
}

pub struct RemotePanel {
    /// The stores of the hosts the window is linked to, in the workspace's
    /// order.
    stores: Vec<Entity<WorkspaceStore>>,
    /// Why the host in this process did not start the last time this window
    /// tried to attach to it.
    local_failure: Option<String>,
    /// Navigation: "Paste an invitation link" pushes [`Destination::Pair`],
    /// which Back pops back to whatever asked for it.
    window_state: Entity<WindowState>,
    form: PairForm,
    /// Whether the window was on the paste page when it last changed pages.
    pairing: bool,
    /// [`ClientHost::fixed_machine`]: a browser lists its one machine and
    /// no way to add or re-pair one.
    fixed_machine: bool,
    hosts_scroll: ScrollHandle,
    pair_scroll: ScrollHandle,
    /// Repaints the invitation's countdown while this machine offers one.
    #[cfg(feature = "remote-hosting")]
    invitation_ticker: Option<gpui::Task<()>>,
    /// The space whose QR is open under this machine's invitation.
    #[cfg(feature = "remote-hosting")]
    space_qr: Option<String>,
    #[cfg(feature = "remote-hosting")]
    spaces_observer: spaces::SpacesObserver,
    /// The panel's size-class motion: the window draws its pages under
    /// another parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
    _subscriptions: Vec<Subscription>,
}

impl RemotePanel {
    pub fn new(
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fixed_machine = cx
            .try_global::<ClientAttachment>()
            .is_some_and(|attachment| attachment.host.fixed_machine());
        let form = PairForm::new(window, cx);
        // A link that parses is the whole request, so it is sent as soon as
        // it lands in the field; Enter and the button only cover a retry.
        let subscriptions = vec![
            cx.observe_in(
                &window_state,
                window,
                |this: &mut Self, state, window, cx| {
                    // Every way onto the paste page opens it with an empty
                    // field: an invitation is single use, so whatever the
                    // field held last time is spent.
                    let pairing = state.read(cx).destination() == Destination::Pair;
                    match (std::mem::replace(&mut this.pairing, pairing), pairing) {
                        (true, false) => this.form.leave(window, cx),
                        (false, true) => {
                            this.form.restart();
                            this.form.clear(window, cx);
                        }
                        _ => {}
                    }
                },
            ),
            cx.subscribe_in(
                &form.invitation,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.form.error = None;
                        if this.form.should_submit(cx) {
                            this.submit(window, cx);
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter {
                        shift: false,
                        secondary: false,
                    } => this.submit(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &form.address,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter {
                        shift: false,
                        secondary: false,
                    } => this.submit(window, cx),
                    _ => {}
                },
            ),
        ];
        Self {
            stores: Vec::new(),
            local_failure: None,
            window_state,
            form,
            pairing: false,
            fixed_machine,
            hosts_scroll: ScrollHandle::new(),
            pair_scroll: ScrollHandle::new(),
            #[cfg(feature = "remote-hosting")]
            invitation_ticker: None,
            #[cfg(feature = "remote-hosting")]
            space_qr: None,
            #[cfg(feature = "remote-hosting")]
            spaces_observer: spaces::SpacesObserver::default(),
            motion: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Follow the window onto the hosts it is linked to now.
    pub fn set_stores(&mut self, stores: Vec<Entity<WorkspaceStore>>, cx: &mut Context<Self>) {
        self.stores = stores;
        cx.notify();
    }

    pub(crate) fn set_local_failure(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.local_failure = reason;
        cx.notify();
    }

    pub(crate) fn set_pairing_error(&mut self, error: Option<String>) {
        self.form.error = error;
    }

    fn client(&self, cx: &App) -> Option<Rc<dyn ClientHost>> {
        cx.try_global::<ClientAttachment>()
            .map(ClientAttachment::host)
    }

    /// Read an invite off the camera and pair with it at once. The scanned
    /// link goes through the same parser a pasted one does; the page the
    /// user is on stays, and shows the attempt and its outcome.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.client(cx) else {
            return;
        };
        self.form.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let scanned = host.scan_qr().await;
            let _ = this.update_in(cx, |panel, window, cx| {
                match scanned {
                    Ok(Some(value)) => {
                        if panel.form.fill_invite(&value, window, cx) {
                            panel.submit(window, cx);
                        } else {
                            panel.form.error =
                                Some(crate::tr!("hosts.pair.bad_invite").into_owned());
                        }
                    }
                    Ok(None) => {}
                    Err(error) => panel.form.error = Some(crate::i18n::device_failure(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pair with the invitation in the field and, once the machine has
    /// admitted this device, connect to it. One step: there is nothing to
    /// confirm that the link did not already say.
    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some((request, address, generation)) = self.form.begin_pair(cx) else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.pair(request, address).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_pair(generation, result, window, cx);
            });
        })
        .detach();
    }

    fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, tcode_client::pairing::PairError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(host) = self.form.finish_pair(generation, result) {
            if let Some(message) = crate::pairing::joined_message(&host) {
                window.push_notification(Notification::success(message), cx);
            }
            let attachment = cx.global::<ClientAttachment>();
            if let Err(error) = attachment.save_host(host.clone()) {
                window.push_notification(
                    Notification::error(
                        crate::tr!(
                            "hosts.pair.unsaved",
                            name = host.name.as_str(),
                            error = error.to_string()
                        )
                        .into_owned(),
                    ),
                    cx,
                );
            }
            let attachment = cx.global::<ClientAttachment>();
            let request = attachment.requester();
            request(
                HostRequest::Open(AttachmentTarget::Remote(Box::new(host))),
                window,
                cx,
            );
        }
        cx.notify();
    }

    /// The store of `host`, while the window is linked to it.
    fn store_of(&self, host: &HostKey, cx: &App) -> Option<&Entity<WorkspaceStore>> {
        self.stores
            .iter()
            .find(|store| store.read(cx).host_key() == *host)
    }

    /// The dot that says how this window's link to the attached machine is
    /// doing. Only a remote link has a state to show.
    fn status_glyph(&self, host_id: &str, window: &mut Window, cx: &mut App) -> AnyElement {
        let color = self
            .store_of(&HostKey::Remote(host_id.to_owned()), cx)
            .map(|store| {
                cx.theme()
                    .connection_color(&store.read(cx).connection_state())
            })
            .unwrap_or(cx.theme().success);
        let color = gpui_base::motion::transition(
            (
                SharedString::from(format!("host-status-{host_id}")),
                "color",
            ),
            color,
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        crate::material::status_dot(color).into_any_element()
    }

    /// The mark on the row this window is on when there is no link to
    /// report on: this machine itself.
    fn current_glyph(cx: &App) -> AnyElement {
        Icon::new(IconName::Check)
            .flex_none()
            .text_color(cx.theme().muted_foreground)
            .into_any_element()
    }

    /// This machine: one target among the saved machines, offered only where
    /// bootstrap actually gave this window a local host to attach to. Being
    /// on it is not a connection, so the row carries a check, not a dot. A
    /// host that did not start says why, and offers starting it again or
    /// quitting.
    fn local_row(&self, compact: bool, window: &mut Window, cx: &mut Context<Self>) -> Option<Row> {
        cx.try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local)
            .then(|| {
                let current = self.store_of(&HostKey::Local, cx).is_some();
                let failure = self.local_failure.clone();
                let failed = failure.is_some();
                let failure =
                    crate::motion::shown("hosts-local-failure", failure, SHOW, window, cx).map(
                        |shown| {
                            shown.fold(
                                div()
                                    .debug_selector(|| "hosts-local-failure".into())
                                    .text_secondary(compact, cx)
                                    .text_color(cx.theme().danger_foreground)
                                    .child(
                                        crate::tr!(
                                            "hosts.local.unavailable",
                                            reason = shown.value.clone()
                                        )
                                        .into_owned(),
                                    ),
                            )
                        },
                    );
                let current = crate::motion::shown(
                    "hosts-local-current",
                    current.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| shown.fade(Self::current_glyph(cx)));
                let actions = crate::motion::shown(
                    "hosts-local-actions",
                    failed.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| {
                    shown.fade(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("hosts-local-retry")
                                    .debug_selector(|| "hosts-local-retry".into())
                                    .primary()
                                    .compact()
                                    .label(crate::tr!("hosts.local.retry"))
                                    .on_click(|_, window, cx| {
                                        let request = cx.global::<ClientAttachment>().requester();
                                        request(
                                            HostRequest::Open(AttachmentTarget::Local),
                                            window,
                                            cx,
                                        );
                                    }),
                            )
                            .child(
                                Button::new("hosts-local-quit")
                                    .compact()
                                    .label(crate::tr!("quit.confirm"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(crate::shell::Quit), cx);
                                    }),
                            ),
                    )
                });
                group_entry(
                    "hosts-local",
                    crate::tr!("hosts.this_computer").into_owned().into(),
                    compact,
                    cx,
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.xxs)
                        .child(
                            div().text_title(cx).overflow_hidden().child(
                                crate::widgets::fade_text(crate::tr!("hosts.this_computer")),
                            ),
                        )
                        .children(failure),
                )
                .children(current)
                .children(actions)
                .when(!failed, |row| {
                    row.on_click(|_, window, cx| {
                        let request = cx.global::<ClientAttachment>().requester();
                        request(HostRequest::Open(AttachmentTarget::Local), window, cx);
                    })
                })
            })
    }

    /// Saved machines offer pairing again when authorization is rejected.
    fn host_row(
        &self,
        host: &PairedHost,
        current: bool,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reason = if current {
            self.store_of(&HostKey::Remote(host.host_id.clone()), cx)
                .and_then(|store| connection_failure(store.read(cx).connection_state()))
        } else {
            None
        };
        let needs_pairing = reason == Some(tcode_client::ConnectionFailure::AuthenticationRejected)
            && !self.fixed_machine;
        let name = SharedString::from(host.name.clone());
        let connect_host = host.clone();
        let part = |part: &str| SharedString::from(format!("host-{part}-{}", host.host_id));
        let reason = crate::motion::shown(
            part("reason"),
            reason
                .as_ref()
                .map(|reason| failure_description(reason, needs_pairing)),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold(
                div()
                    .text_secondary(compact, cx)
                    .text_color(cx.theme().danger_foreground)
                    .child(shown.value.clone()),
            )
        });
        let status = crate::motion::shown(part("current"), current.then_some(()), SHOW, window, cx)
            .map(|shown| shown.fade(self.status_glyph(&host.host_id, window, cx)));
        let repair = crate::motion::shown(
            part("repair"),
            needs_pairing.then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fade(
                Button::new(SharedString::from(format!("repair-{}", host.host_id)))
                    .primary()
                    .compact()
                    .label(crate::tr!("hosts.pair_again"))
                    // A new pairing needs a new invitation from the
                    // machine; nothing from the stale record carries over.
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel
                            .window_state
                            .update(cx, |state, cx| state.go(Destination::Pair, cx));
                    })),
            )
        });
        let row = group_entry(
            SharedString::from(format!("host-{}", host.host_id)),
            name.clone(),
            compact,
            cx,
        )
        .debug_selector(|| format!("host-{}", host.host_id))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(cx.theme().tokens.spacing.xxs)
                .child(
                    div()
                        .text_title(cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(name.clone())),
                )
                .when_some(host.space_name.as_deref(), |column, space| {
                    column.child(div().text_secondary(compact, cx).overflow_hidden().child(
                        crate::widgets::fade_text(
                            crate::tr!("member.space", space = space).into_owned(),
                        ),
                    ))
                })
                .when(
                    host.space_name.is_none() && host.group_id.is_some(),
                    |column| {
                        column.child(
                            div()
                                .text_secondary(compact, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(crate::tr!("hosts.in_group"))),
                        )
                    },
                )
                .children(reason),
        )
        .children(status)
        .children(repair)
        .when(!needs_pairing, |row| {
            row.on_click(move |_, window, cx| {
                let request = cx.global::<ClientAttachment>().requester();
                request(
                    HostRequest::Open(AttachmentTarget::Remote(Box::new(connect_host.clone()))),
                    window,
                    cx,
                );
            })
        })
        // The row must keep its hover fill while the child menu is hovered.
        .child(self.host_menu(host, current, cx));
        row.into_any_element()
    }

    fn host_menu(&self, host: &PairedHost, current: bool, cx: &App) -> AnyElement {
        let host_id = host.host_id.clone();
        // Where the host stands among the workspace's, which a space's
        // host, reached alone, is not one of.
        let key = HostKey::Remote(host.host_id.clone());
        let place = self
            .stores
            .iter()
            .position(|store| store.read(cx).host_key() == key)
            .filter(|_| self.stores.len() > 1);
        let last = self.stores.len().saturating_sub(1);
        let label = crate::tr!("hosts.actions", name = host.name.clone()).into_owned();
        div()
            .flex_none()
            .size(px(crate::material::TOUCH_TARGET))
            .flex()
            .items_center()
            .justify_center()
            // The row must not arm a connection press before the menu opens.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("host-menu-{host_id}")))
                    .ghost()
                    .icon(IconName::Ellipsis)
                    .aria_label(label)
                    .dropdown_menu(move |menu, _window, _cx| {
                        let menu = menu
                            .when(place.is_some_and(|place| place > 0), |menu| {
                                menu.menu(
                                    crate::tr!("hosts.move_up").into_owned(),
                                    Box::new(MoveHost {
                                        host_id: host_id.clone(),
                                        earlier: true,
                                    }),
                                )
                            })
                            .when(place.is_some_and(|place| place < last), |menu| {
                                menu.menu(
                                    crate::tr!("hosts.move_down").into_owned(),
                                    Box::new(MoveHost {
                                        host_id: host_id.clone(),
                                        earlier: false,
                                    }),
                                )
                            })
                            .when(current, |menu| {
                                menu.menu(
                                    crate::tr!("hosts.disconnect").into_owned(),
                                    Box::new(DisconnectHost(host_id.clone())),
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

    /// The whole Hosts surface, one captioned group each: this machine and
    /// the invitation it offers other devices, its shared spaces, the saved
    /// machines and, where the client can pair, the ways to add another.
    pub(crate) fn render_hosts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let (hosts, unreadable) = match self.client(cx).map(|client| client.load_hosts()) {
            Some(Ok(hosts)) => (hosts, None),
            Some(Err(error)) => (Vec::new(), Some(error)),
            None => (Vec::new(), None),
        };
        let mut column = v_flex().gap(section_gap(cx));

        let mut this_machine: Vec<AnyElement> = Vec::new();
        if let Some(local) = self.local_row(compact, window, cx) {
            this_machine.push(local.into_any_element());
        }
        // Only a host running in this process can offer an invitation.
        #[cfg(feature = "remote-hosting")]
        if cx.has_global::<RemoteController>() {
            this_machine.push(self.invitation_row(compact, window, cx));
        }
        if !this_machine.is_empty() {
            column = column.child(settings_group(
                crate::tr!("hosts.this_machine_section"),
                this_machine,
                cx,
            ));
        }
        #[cfg(feature = "remote-hosting")]
        if cx.has_global::<RemoteController>() {
            column = column.children(self.space_links(compact, window, cx));
        }

        // The empty row is one more key, which leaves as the first machine
        // arrives and comes back after the last one goes.
        // The workspace's hosts first, in its order, then the others.
        let mut hosts = hosts;
        hosts.sort_by_key(|host| {
            let key = HostKey::Remote(host.host_id.clone());
            self.stores
                .iter()
                .position(|store| store.read(cx).host_key() == key)
                .unwrap_or(self.stores.len())
        });
        let saved: Vec<(SharedString, Option<(PairedHost, bool)>)> = if hosts.is_empty() {
            let key = if unreadable.is_some() {
                "hosts-unreadable"
            } else {
                "hosts-empty"
            };
            vec![(key.into(), None)]
        } else {
            hosts
                .into_iter()
                .map(|host| {
                    let current = self
                        .store_of(&HostKey::Remote(host.host_id.clone()), cx)
                        .is_some();
                    (
                        format!("host-{}", host.host_id).into(),
                        Some((host, current)),
                    )
                })
                .collect()
        };
        let saved = crate::motion::keyed(
            "hosts-saved",
            saved,
            ROWS,
            window,
            cx,
            |_, row, window, cx| match row {
                Some((host, current)) => self.host_row(host, *current, compact, window, cx),
                None => match &unreadable {
                    Some(error) => crate::material::empty_row(
                        crate::tr!("errors.file_unreadable", error = error.to_string()),
                        cx,
                    )
                    .debug_selector(|| "hosts-unreadable".into()),
                    None => crate::material::empty_row(crate::tr!("hosts.empty"), cx),
                }
                .text_secondary(compact, cx)
                .into_any_element(),
            },
        );
        column = column.child(settings_group(
            crate::tr!("hosts.saved"),
            vec![saved.render(v_flex().w_full())],
            cx,
        ));
        if !self.fixed_machine {
            column = column.child(self.add_machine(compact, window, cx));
        }
        self.page(column, &self.hosts_scroll, window, cx)
    }

    /// What this machine offers other devices: the live invitation while
    /// hosting accepts new devices, otherwise the way to the setting that
    /// turns it on. Hosting itself — the endpoint, its name, Traverse — is
    /// configured in Settings → Remote; this is where the connection is made.
    #[cfg(feature = "remote-hosting")]
    fn invitation_row(
        &mut self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use hosting::{InvitationOffer, RemoteController};
        let offer = cx
            .try_global::<RemoteController>()
            .map(RemoteController::invitation_offer)
            .unwrap_or(InvitationOffer::NotHosting);
        self.sync_invitation_ticker(matches!(offer, InvitationOffer::Live { .. }), cx);
        // Each state of the offer folds into the next, so an expiring
        // invitation shrinks its card down to the row that replaces it.
        let live = match &offer {
            InvitationOffer::Live {
                invitation,
                remaining,
            } => Some((
                invitation.url(),
                *remaining,
                spaces::local(cx)
                    .and_then(|spaces| spaces.read(cx).group().map(|group| group.name.clone())),
            )),
            _ => None,
        };
        let live = crate::motion::shown("hosts-invite-live", live, SHOW, window, cx).map(|shown| {
            let (link, remaining, group) = &shown.value;
            shown.fold(access::invitation_card(
                link,
                *remaining,
                group.as_deref(),
                self.motion
                    .reflow_in("hosts-invite-qr", u64::from(compact), window, cx),
                compact,
                hosting::new_invitation(),
                window,
                cx,
            ))
        });
        let expired = crate::motion::shown(
            "hosts-invite-expired",
            (offer == InvitationOffer::Expired).then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold(access::expired_invitation_row(
                compact,
                hosting::new_invitation(),
                cx,
            ))
        });
        let note = matches!(
            offer,
            InvitationOffer::NotHosting | InvitationOffer::PairingOff
        )
        .then_some(offer == InvitationOffer::PairingOff);
        let note = crate::motion::shown("hosts-invite-note", note, SHOW, window, cx).map(|shown| {
            let note = if shown.value {
                crate::tr!("hosts.invite.pairing_off")
            } else {
                crate::tr!("hosts.invite.disabled")
            };
            shown.fold(
                crate::settings_rows::row_frame(compact, cx)
                    .child(crate::settings_rows::row_labels(
                        note, None, None, compact, cx,
                    ))
                    .child(
                        Button::new("hosts-remote-settings")
                            .debug_selector(|| "hosts-remote-settings".into())
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("hosts.invite.open_settings"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.window_state.update(cx, |state, cx| {
                                    state.pending_settings_section = Some("remote".into());
                                    state.open_settings(cx);
                                });
                            })),
                    ),
            )
        });
        v_flex()
            .w_full()
            .children(live)
            .children(expired)
            .children(note)
            .into_any_element()
    }

    /// This machine's shared spaces, each with its link and QR on demand.
    #[cfg(feature = "remote-hosting")]
    fn space_links(
        &mut self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let spaces = spaces::local(cx);
        self.spaces_observer.watch(spaces.as_ref(), cx);
        spaces.and_then(|spaces| {
            spaces::machine_links(
                &spaces,
                &self.motion,
                self.space_qr.as_deref(),
                |panel: &mut Self, id, cx| {
                    panel.space_qr = (panel.space_qr.as_deref() != Some(id.as_str())).then_some(id);
                    cx.notify();
                },
                compact,
                window,
                cx,
            )
        })
    }

    /// Run the one-second repaint exactly while an invitation counts down.
    /// Its last tick paints the expiry; a fresh invitation is painted by the
    /// visit that shows it.
    #[cfg(feature = "remote-hosting")]
    fn sync_invitation_ticker(&mut self, live: bool, cx: &mut Context<Self>) {
        match (live, self.invitation_ticker.is_some()) {
            (true, false) => {
                self.invitation_ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(1))
                            .await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                }));
            }
            (false, true) => self.invitation_ticker = None,
            _ => {}
        }
    }

    /// The ways in: the camera where there is one, and the invitation field
    /// everywhere. Scanning pairs on the spot; pasting opens a page for the
    /// field. A first pairing is always by invitation, never by finding a
    /// machine on the network. The attempt in flight and its failure are
    /// shown here, under the rows that started them.
    fn add_machine(
        &self,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        let entry = |id: &'static str, title: SharedString, cx: &mut Context<Self>| {
            group_entry(id, title.clone(), compact, cx)
                .debug_selector(move || id.into())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(compact, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(title)),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
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
                cx,
            )
            .on_click(cx.listener(|panel, _, _, cx| {
                panel
                    .window_state
                    .update(cx, |state, cx| state.go(Destination::Pair, cx));
            }))
            .into_any_element(),
        );
        let in_row = |line: AnyElement, cx: &App| group_row(cx).child(line);
        rows.extend(self.attempt_status(compact, in_row, window, cx));
        rows.extend(self.address_prompt(true, compact, in_row, window, cx));
        settings_group(crate::tr!("hosts.pair.title"), rows, cx)
            .debug_selector(|| "hosts-add-machine".into())
            .into_any_element()
    }

    /// The pairing attempt in flight, or the failure of the last one; one
    /// line at both the Hosts page and the paste page, drawn in `frame`.
    /// Each folds in and out, so a failed attempt's line folds into its
    /// error.
    fn attempt_status(
        &self,
        compact: bool,
        frame: impl Fn(AnyElement, &App) -> gpui::Div,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let busy = crate::motion::shown(
            "hosts-pairing",
            self.form.busy.then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold(frame(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .text_secondary(compact, cx)
                    .debug_selector(|| "hosts-pairing".into())
                    .child(crate::widgets::spinner::Spinner::new())
                    .child(div().min_w_0().child(crate::tr!("hosts.pair.connecting")))
                    .into_any_element(),
                cx,
            ))
        });
        let error = (!self.form.busy)
            .then(|| {
                self.form.error.clone().or_else(|| {
                    self.form
                        .invalid(cx)
                        .then(|| crate::tr!("hosts.pair.bad_invite").into_owned())
                })
            })
            .flatten();
        let error =
            crate::motion::shown("hosts-pair-error", error, SHOW, window, cx).map(|shown| {
                shown.fold(frame(
                    div()
                        .w_full()
                        .text_secondary(compact, cx)
                        .min_w_0()
                        .text_color(cx.theme().danger_foreground)
                        .debug_selector(|| "hosts-pair-error".into())
                        .child(shown.value.clone())
                        .into_any_element(),
                    cx,
                ))
            });
        busy.into_iter().chain(error).collect()
    }

    /// The machine's IP address, asked for once an attempt found no path to
    /// it; the prompt stands in for that failure and folds in and out with
    /// it, drawn in `frame`. `retry` adds the button that sends it, where no
    /// Connect button is beside the field.
    fn address_prompt(
        &self,
        retry: bool,
        compact: bool,
        frame: impl Fn(AnyElement, &App) -> gpui::Div,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let shown = crate::motion::shown(
            "hosts-pair-address",
            self.form.needs_address(cx).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let invalid = self.form.typed_address(cx).is_err();
        let error = if invalid {
            Some(crate::tr!("hosts.pair.bad_address").into_owned())
        } else {
            self.form.address_error(cx).map(str::to_owned)
        };
        let error = crate::motion::shown("hosts-pair-address-error", error, SHOW, window, cx).map(
            |shown| {
                shown.fold(
                    div()
                        .pt_1p5()
                        .text_secondary(compact, cx)
                        .min_w_0()
                        .text_color(cx.theme().danger_foreground)
                        .debug_selector(|| "hosts-pair-address-error".into())
                        .child(shown.value.clone()),
                )
            },
        );
        let busy = self.form.busy;
        Some(
            shown.fold(frame(
                v_flex()
                    .w_full()
                    .debug_selector(|| "hosts-pair-address".into())
                    .child(
                        v_flex()
                            .w_full()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_body(compact, cx)
                                    .child(crate::tr!("hosts.pair.address")),
                            )
                            .child(
                                div()
                                    .text_secondary(compact, cx)
                                    .min_w_0()
                                    .child(crate::tr!("hosts.pair.address_description")),
                            )
                            .child(
                                Input::new(&self.form.address)
                                    .placeholder(crate::tr!("hosts.pair.address_placeholder")),
                            ),
                    )
                    // The error carries its own spacing, so it folds away
                    // with it.
                    .children(error)
                    .when(retry, |column| {
                        column.child(
                            div().pt_1p5().child(
                                Button::new("hosts-pair-retry")
                                    .primary()
                                    .when(compact, |button| button.w_full())
                                    .loading(busy)
                                    .disabled(busy || invalid)
                                    .label(crate::tr!("hosts.pair.retry"))
                                    .on_click(
                                        cx.listener(|panel, _, window, cx| {
                                            panel.submit(window, cx)
                                        }),
                                    ),
                            ),
                        )
                    })
                    .into_any_element(),
                cx,
            )),
        )
    }

    /// The paste page. A link that parses is submitted as it lands; Connect
    /// is never the only way, only the retry. Wide, the form is one card with
    /// Connect beside its field. Compact, Connect is pinned to the foot of
    /// the page, above the software keyboard, which the window seam already
    /// accounts for.
    pub(crate) fn render_pair(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let busy = self.form.busy;
        let ready = !busy && self.form.request(cx).is_some() && self.form.typed_address(cx).is_ok();
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        let action = Button::new("hosts-pair-submit")
            .debug_selector(|| "hosts-pair-submit".into())
            .primary()
            .loading(busy)
            .disabled(!ready)
            .label(crate::tr!("hosts.pair.action"))
            .on_click(cx.listener(|panel, _, window, cx| panel.submit(window, cx)));
        let scan = scannable.then(|| {
            Button::new("hosts-scan")
                .ghost()
                .outline()
                .disabled(busy)
                .label(crate::tr!("hosts.pair.scan"))
                .on_click(cx.listener(|panel, _, _, cx| panel.scan(cx)))
        });
        let field = Input::new(&self.form.invitation)
            .placeholder(crate::tr!("hosts.pair.invitation_placeholder"));
        let as_is = |line: AnyElement, _: &App| div().w_full().child(line);
        // A window turning compact moves the field, the scan button and the
        // action into a page with the action pinned at its foot; each glides
        // there from where it was, and back.
        let reflow = self
            .motion
            .reflow_in("hosts-pair", u64::from(compact), window, cx);
        if !compact {
            let form = group_row_stacked(cx)
                .child(crate::settings_rows::row_labels(
                    crate::tr!("hosts.pair.invitation"),
                    Some(crate::tr!("hosts.pair.description").into_owned().into()),
                    None,
                    false,
                    cx,
                ))
                .child(
                    h_flex()
                        .w_full()
                        .gap(cx.theme().tokens.spacing.sm)
                        .items_center()
                        .child(reflow.part("field", div().flex_1().min_w_0().child(field)))
                        .children(scan.map(|scan| reflow.part("scan", scan)))
                        .child(reflow.part("action", action)),
                )
                .children(self.attempt_status(compact, as_is, window, cx))
                .children(self.address_prompt(false, compact, as_is, window, cx));
            let page = v_flex().child(
                crate::material::group(cx)
                    .debug_selector(|| "hosts-pair-form".into())
                    .child(form),
            );
            return reflow.render(self.page(page, &self.pair_scroll, window, cx));
        }
        let body = v_flex()
            .gap(section_gap(cx))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .min_w_0()
                    .child(crate::tr!("hosts.pair.description")),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("hosts.pair.invitation")),
                    )
                    .child(reflow.part("field", field)),
            )
            .children(self.attempt_status(compact, as_is, window, cx))
            .children(self.address_prompt(false, compact, as_is, window, cx))
            .children(scan.map(|scan| reflow.part("scan", scan.w_full())));
        let action = reflow.part("action", action.w_full());
        reflow.render(self.page_with_footer(body, action, window, cx))
    }

    /// The scrolling page body: the settings content column, so the page's
    /// groups line up with Settings' at every width.
    fn page(
        &self,
        body: gpui::Div,
        scroll: &ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::scroll::page_viewport(
            "hosts-scroll-bounce",
            scroll,
            div()
                .id("hosts-scroll")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(scroll)
                .on_action(cx.listener(Self::on_disconnect))
                .on_action(cx.listener(Self::on_move))
                .on_action(cx.listener(Self::on_forget))
                .child(crate::settings_rows::content_column(
                    body,
                    compact,
                    &self.motion,
                    window,
                    cx,
                )),
        )
        .into_any_element()
    }

    fn page_with_footer(
        &self,
        body: gpui::Div,
        action: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .child(self.page(body, &self.pair_scroll, window, cx))
            .child(
                h_flex().flex_none().w_full().justify_center().child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .px(px(COMPACT_PAGE_INSET))
                        .pb(px(COMPACT_PAGE_INSET))
                        .pt(cx.theme().tokens.spacing.sm)
                        .child(action),
                ),
            )
            .into_any_element()
    }

    fn on_disconnect(
        &mut self,
        action: &DisconnectHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = cx.global::<ClientAttachment>().requester();
        request(
            HostRequest::Leave(HostKey::Remote(action.0.clone())),
            window,
            cx,
        );
    }

    fn on_move(&mut self, action: &MoveHost, window: &mut Window, cx: &mut Context<Self>) {
        let request = cx.global::<ClientAttachment>().requester();
        request(
            HostRequest::Move {
                host: HostKey::Remote(action.host_id.clone()),
                earlier: action.earlier,
            },
            window,
            cx,
        );
    }

    fn on_forget(&mut self, action: &ForgetHost, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.0.clone();
        let Some(name) = self
            .client(cx)
            .and_then(|client| {
                client
                    .load_hosts()
                    .ok()?
                    .into_iter()
                    .find(|host| host.host_id == id)
            })
            .map(|host| host.name)
        else {
            return;
        };
        let panel = cx.entity().downgrade();
        let title = crate::tr!("hosts.forget_title", name = name.as_str()).into_owned();
        confirm_destructive(
            title,
            crate::tr!("hosts.forget_description").into_owned(),
            crate::tr!("hosts.forget").into_owned(),
            move |window, cx| {
                if let Err(error) = cx.global::<ClientAttachment>().remove_host(&id) {
                    window.push_notification(
                        Notification::error(
                            crate::tr!(
                                "hosts.forget_failed",
                                name = name.as_str(),
                                error = error.to_string()
                            )
                            .into_owned(),
                        ),
                        cx,
                    );
                }
                let _ = panel.update(cx, |_, cx| cx.notify());
            },
            window,
            cx,
        );
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

/// A path's name, in a device's status column and under a machine's name:
/// on the LAN, punched across the internet, or through a relay named by
/// its region; `None` is a device that is not connected.
pub(crate) fn path_label(path: Option<&tcode_protocol::PathInfo>) -> String {
    use tcode_protocol::PathKind;
    match path.map(tcode_protocol::PathInfo::kind) {
        None => crate::tr!("remote.path.offline"),
        Some(PathKind::Lan) => crate::tr!("remote.path.lan"),
        Some(PathKind::Tunnel) => crate::tr!("remote.path.tunnel"),
        Some(PathKind::Relay { url }) => match url.and_then(relay_region) {
            Some(region) => crate::tr!("remote.path.relay_region", region = region),
            None => crate::tr!("remote.path.relay"),
        },
    }
    .into_owned()
}

/// What tells one relay from another: the region label of an n0 relay
/// (`aps1-1.relay.n0.iroh.link` is `aps1-1`), the whole host of any other.
fn relay_region(relay: &str) -> Option<String> {
    let host = url::Url::parse(relay).ok()?.host_str()?.to_owned();
    Some(match host.strip_suffix(".relay.n0.iroh.link") {
        Some(region) => region.to_owned(),
        None => host,
    })
}

/// How this window's link to its machine is doing, one line under the
/// machine's name: the path while the link is up, and once it is lost the
/// failure, which says what to do about it. A transport that cannot tell
/// how it is carried (a browser) says only that it is connected. Loading
/// the baseline over an up link is not a link state: the content area's
/// skeletons show it, the line says the link is connected.
pub(crate) fn connection_label(state: &tcode_client::ConnectionState) -> String {
    connection_line(state, false)
}

/// [`connection_label`] beside a Pair again button, which the line leaves
/// the remedy to.
pub(crate) fn connection_label_beside_pair_again(state: &tcode_client::ConnectionState) -> String {
    connection_line(state, true)
}

fn connection_line(state: &tcode_client::ConnectionState, pair_again_shown: bool) -> String {
    use tcode_client::ConnectionState;
    match state {
        ConnectionState::Connected { path } | ConnectionState::Syncing { path } => match path {
            Some(path) if path.probing_direct => {
                crate::tr!("remote.path.probing_direct").into_owned()
            }
            Some(path) => format!(
                "{} · {}",
                path_label(Some(path)),
                crate::tr!("remote.state.connected")
            ),
            None => crate::tr!("remote.state.connected").into_owned(),
        },
        ConnectionState::Reconnecting { .. } => {
            crate::tr!("remote.state.reconnecting").into_owned()
        }
        ConnectionState::Offline { reason } => format!(
            "{} · {}",
            crate::tr!("remote.path.offline"),
            failure_label(reason, pair_again_shown)
        ),
    }
}

/// The same recovery wording is used in the shell and the machine row: a
/// refusal says to pair again unless a Pair again button is beside it.
fn failure_label(reason: &tcode_client::ConnectionFailure, pair_again_shown: bool) -> String {
    use tcode_client::ConnectionFailure::*;
    match reason {
        Unreachable(_) => crate::tr!("remote.failure.unreachable"),
        Timeout => crate::tr!("remote.failure.timeout"),
        AuthenticationRejected if pair_again_shown => {
            crate::tr!("remote.failure.authentication_rejected")
        }
        AuthenticationRejected => format!(
            "{} · {}",
            crate::tr!("remote.failure.authentication_rejected"),
            crate::tr!("hosts.pair_again")
        )
        .into(),
        ProtocolMismatch => crate::tr!("remote.failure.protocol_mismatch"),
        HostClosed => crate::tr!("remote.failure.host_closed"),
    }
    .into_owned()
}

/// Why a link is down, while it is: it stopped, or it failed and dials again.
pub(crate) fn connection_failure(
    state: tcode_client::ConnectionState,
) -> Option<tcode_client::ConnectionFailure> {
    match state {
        tcode_client::ConnectionState::Offline { reason } => Some(reason),
        tcode_client::ConnectionState::Reconnecting { reason, .. } => reason,
        _ => None,
    }
}

/// The machine row has room for the transport's cause; the shell's status
/// line does not.
fn failure_description(reason: &tcode_client::ConnectionFailure, pair_again_shown: bool) -> String {
    match reason {
        tcode_client::ConnectionFailure::Unreachable(Some(cause)) => {
            crate::tr!("remote.failure.unreachable_cause", reason = cause).into_owned()
        }
        reason => failure_label(reason, pair_again_shown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Render, TestAppContext};

    /// The line names the path while the link is up and the failure once
    /// it is lost; a relay is told apart by region, a self-hosted one by
    /// host; a transport that cannot tell how it is carried names no path.
    #[test]
    fn the_connection_label_names_the_path_or_the_failure() {
        use tcode_client::{ConnectionFailure, ConnectionState};
        use tcode_protocol::PathInfo;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let lan = PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        };
        let tunnel = PathInfo {
            lan: false,
            ..lan.clone()
        };
        let relayed = PathInfo {
            direct: false,
            relay: Some("https://aps1-1.relay.n0.iroh.link/".into()),
            lan: false,
            probing_direct: false,
        };
        let self_hosted = PathInfo {
            relay: Some("https://traverse.example:8443/".into()),
            ..relayed.clone()
        };
        let probing = PathInfo {
            probing_direct: true,
            ..relayed.clone()
        };
        let connected = |path: &PathInfo| {
            connection_label(&ConnectionState::Connected {
                path: Some(path.clone()),
            })
        };
        assert_eq!(connected(&lan), "LAN · Connected");
        assert_eq!(connected(&tunnel), "Tunnel · Connected");
        assert_eq!(connected(&relayed), "Relay (aps1-1) · Connected");
        assert_eq!(
            connected(&self_hosted),
            "Relay (traverse.example) · Connected"
        );
        assert_eq!(connected(&probing), "Relay · trying direct");
        assert_eq!(path_label(None), "Offline");
        assert_eq!(path_label(Some(&probing)), "Relay (aps1-1)");
        assert_eq!(
            connection_label(&ConnectionState::Connected { path: None }),
            "Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Syncing {
                path: Some(lan.clone())
            }),
            "LAN · Connected",
            "loading the baseline is not a link state"
        );
        assert_eq!(
            connection_label(&ConnectionState::Syncing { path: None }),
            "Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Reconnecting {
                attempt: 3,
                reason: Some(ConnectionFailure::Timeout)
            }),
            "Reconnecting"
        );
        assert_eq!(
            connection_label(&ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected
            }),
            "Offline · Access rejected · Pair again"
        );
        // Beside a Pair again button, the line leaves the remedy to it.
        assert_eq!(
            connection_label_beside_pair_again(&ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected
            }),
            "Offline · Access rejected"
        );
        assert_eq!(
            failure_description(&ConnectionFailure::AuthenticationRejected, true),
            "Access rejected"
        );
        let unreachable = ConnectionFailure::Unreachable(Some(
            "No addressing information available: Address lookup failed".into(),
        ));
        assert_eq!(
            connection_label(&ConnectionState::Offline {
                reason: unreachable.clone()
            }),
            "Offline · Machine unreachable",
            "the status line has no room for the cause"
        );
        assert_eq!(
            failure_description(&unreachable, false),
            "Machine unreachable: No addressing information available: Address lookup failed",
            "the machine row says why"
        );
        assert_eq!(
            failure_description(&ConnectionFailure::Unreachable(None), false),
            "Machine unreachable"
        );
    }

    #[cfg(feature = "remote-hosting")]
    struct PairingProbe(Entity<RemotePanel>);

    #[cfg(feature = "remote-hosting")]
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
            fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
                Ok(vec![PairedHost {
                    host_id: "served-by".into(),
                    name: "Build server".into(),
                    traverse: Vec::new(),
                    relay: None,
                    addrs: Vec::new(),
                    last_connected_unix: None,
                    group_id: None,
                    space_id: None,
                    space_name: None,
                }])
            }
            fn save_hosts(&self, _: &[PairedHost]) -> Result<(), FileFailure> {
                Ok(())
            }
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
            let state = cx.new(|_| WindowState::default());
            HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
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

    /// Saved machines that cannot be read are not "no machines": the list
    /// says the file could not be read, and why.
    #[gpui::test]
    fn unreadable_saved_machines_say_so_on_the_hosts_page(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("hosts-page-unreadable");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hosts.json"), br#"[{"host_id": 7}]"#).unwrap();
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
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
        cx.update(|cx| cx.set_global(ClientAttachment::new(client, false, |_, _, _| {})));
        let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
            let state = cx.new(|_| WindowState::default());
            HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        crate::test_frames::draw_settled(cx);
        assert!(cx.debug_bounds("hosts-unreadable").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Forgetting a saved machine: its row folds away where it was, taking no
    /// press, and the machine below it slides up as it goes rather than
    /// jumping into its place; a machine paired afterwards folds in.
    #[gpui::test]
    fn a_forgotten_machine_folds_away_and_the_next_moves_up(cx: &mut TestAppContext) {
        use std::cell::{Cell, RefCell};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct Phone(RefCell<Vec<PairedHost>>);
        impl ClientHost for Phone {
            fn device_name(&self) -> String {
                "Phone".into()
            }
            fn device_id(&self) -> String {
                "phone".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
                Ok(self.0.borrow().clone())
            }
            fn save_hosts(&self, hosts: &[PairedHost]) -> Result<(), FileFailure> {
                *self.0.borrow_mut() = hosts.to_vec();
                Ok(())
            }
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("nothing connects")
            }
        }
        let host = |id: &str, name: &str| PairedHost {
            host_id: id.into(),
            name: name.into(),
            traverse: Vec::new(),
            relay: None,
            addrs: Vec::new(),
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        let presses = Rc::new(Cell::new(0));
        cx.update(crate::theme::init);
        cx.update(|cx| {
            let presses = presses.clone();
            let phone = Phone(RefCell::new(vec![
                host("studio", "Studio"),
                host("laptop", "Laptop"),
            ]));
            cx.set_global(ClientAttachment::new(
                Rc::new(phone),
                false,
                move |_, _, _| presses.set(presses.get() + 1),
            ))
        });
        let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
            let state = cx.new(|_| WindowState::default());
            HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
        });
        let probe = window.root(cx).unwrap();
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let settled = |cx: &mut gpui::VisualTestContext| crate::test_frames::draw_settled(cx);
        settled(cx);
        let studio = cx.debug_bounds("host-studio").expect("Studio is listed");
        let laptop = cx.debug_bounds("host-laptop").expect("Laptop is listed");
        assert!(laptop.top() > studio.top());

        let change = |cx: &mut gpui::VisualTestContext, change: &dyn Fn(&ClientAttachment)| {
            cx.update(|_, cx| change(cx.global::<ClientAttachment>()));
            probe.update(cx, |probe, cx| probe.0.update(cx, |_, cx| cx.notify()));
            crate::test_frames::draw(cx);
        };
        change(cx, &|hosts| hosts.remove_host("studio").unwrap());
        let leaving = cx
            .debug_bounds("host-studio")
            .expect("a forgotten machine is drawn while it leaves");
        cx.simulate_click(leaving.center(), Default::default());
        assert_eq!(presses.get(), 0, "a leaving machine takes no press");

        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        crate::test_frames::draw(cx);
        let moving = cx.debug_bounds("host-laptop").unwrap();
        assert!(
            moving.top() > studio.top() && moving.top() < laptop.top(),
            "Laptop is on its way up, at {:?}",
            moving.top()
        );

        settled(cx);
        assert!(cx.debug_bounds("host-studio").is_none(), "gone once folded");
        assert_eq!(cx.debug_bounds("host-laptop").unwrap().top(), studio.top());

        let below = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("hosts-add-machine")
                .expect("the way to add a machine")
                .top()
        };
        let before = below(cx);
        change(cx, &|hosts| {
            hosts.save_host(host("studio", "Studio")).unwrap()
        });
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        crate::test_frames::draw(cx);
        assert!(
            cx.debug_bounds("host-studio").is_some(),
            "a paired machine arrives"
        );
        let opening = below(cx);
        settled(cx);
        assert!(
            opening > before && opening < below(cx),
            "it folds in, so what is below moves down as it opens"
        );
    }

    /// A phone turned to a width whose layout is compact moves the pairing
    /// form's action from beside the field to the page's foot: it is drawn
    /// where it was, glides there, and with motion reduced is there at once.
    #[gpui::test]
    fn the_pair_action_glides_to_the_foot_when_the_window_turns_compact(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct Phone;
        impl ClientHost for Phone {
            fn device_name(&self) -> String {
                "Phone".into()
            }
            fn device_id(&self) -> String {
                "phone".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
                Ok(Vec::new())
            }
            fn save_hosts(&self, _: &[PairedHost]) -> Result<(), FileFailure> {
                Ok(())
            }
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("nothing connects")
            }
        }
        /// Sets the layout for the window's width as it draws, as the shell
        /// does, so a resize and the layout it calls for land in one frame.
        struct PairProbe(Entity<RemotePanel>, Entity<WindowState>);
        impl Render for PairProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let compact = window.viewport_size().width < px(900.);
                self.1
                    .update(cx, |state, cx| state.set_compact(compact, cx));
                v_flex()
                    .size_full()
                    .child(self.0.update(cx, |panel, cx| panel.render_pair(window, cx)))
            }
        }
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_global(ClientAttachment::new(Rc::new(Phone), false, |_, _, _| {})));
        let state = cx.new(|_| WindowState::default());
        let window = cx.open_window(gpui::size(px(1024.), px(852.)), |window, cx| {
            PairProbe(
                cx.new(|cx| RemotePanel::new(state.clone(), window, cx)),
                state.clone(),
            )
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let turn = |width: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            crate::test_frames::draw(cx);
        };
        let action = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("hosts-pair-submit")
                .expect("the action is drawn")
                .origin
        };
        crate::test_frames::draw_settled(cx);
        let beside = action(cx);

        turn(393., cx);
        assert_eq!(action(cx), beside, "the action starts where it was");
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        crate::test_frames::draw(cx);
        let gliding = action(cx);
        crate::test_frames::draw_settled(cx);
        let foot = action(cx);
        assert!(foot.y > px(852. / 2.), "settled, the action is at the foot");
        assert!(
            gliding.y > beside.y && gliding.y < foot.y,
            "the action is on its way down, at {gliding:?}"
        );

        cx.update(|_, cx| cx.set_reduce_motion(true));
        turn(1024., cx);
        assert_eq!(action(cx), beside, "with motion reduced it is back at once");
    }

    /// A scanned link that opens no path to the machine: the Hosts page asks
    /// for the machine's address under the failure, and the retry sends the
    /// same invitation with it.
    #[gpui::test]
    fn a_link_that_found_no_path_asks_for_the_address_and_retries_with_it(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::net::IpAddr;
        use tcode_client::pairing::PairInvite;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        #[derive(Default)]
        struct Phone {
            attempts: RefCell<Vec<(PairInvite, Option<IpAddr>)>>,
            hosts: RefCell<Vec<PairedHost>>,
        }
        impl ClientHost for Phone {
            fn device_name(&self) -> String {
                "phone".into()
            }
            fn device_id(&self) -> String {
                "phone".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
                Ok(self.hosts.borrow().clone())
            }
            fn save_hosts(&self, hosts: &[PairedHost]) -> Result<(), FileFailure> {
                *self.hosts.borrow_mut() = hosts.to_vec();
                Ok(())
            }
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn supports_qr(&self) -> bool {
                true
            }
            fn pair(
                &self,
                invite: PairInvite,
                address: Option<IpAddr>,
            ) -> tcode_client::host::HostFuture<
                '_,
                Result<PairedHost, tcode_client::pairing::PairError>,
            > {
                self.attempts.borrow_mut().push((invite.clone(), address));
                Box::pin(async move {
                    let reached: Option<IpAddr> = "192.168.1.20".parse().ok();
                    if address == reached {
                        Ok(invite.paired("Studio".into()))
                    } else {
                        Err(tcode_client::pairing::PairError::Unreachable(
                            "No addressing information available".into(),
                        ))
                    }
                })
            }
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("the switch is the attachment owner's")
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
        let phone = Rc::new(Phone::default());
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_global(ClientAttachment::new(phone.clone(), false, |_, _, _| {})));
        let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
            let state = cx.new(|_| WindowState::default());
            HostsProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
        });
        let panel = window.update(cx, |probe, _, _| probe.0.clone()).unwrap();
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
        };
        let invite = PairInvite {
            host_id: "ab".repeat(32),
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            space: None,
            group: None,
            traverse: Vec::new(),
            relay: None,
            port: 47420,
        };
        let link = tcode_client::pairing::pair_url(&invite);
        // What a scan does with the link it read.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                assert!(panel.form.fill_invite(&link, window, cx));
                panel.submit(window, cx);
            });
        });
        draw(cx);
        assert_eq!(*phone.attempts.borrow(), [(invite.clone(), None)]);
        assert!(
            cx.debug_bounds("hosts-pair-address").is_some(),
            "no path: the page asks for the address"
        );
        assert!(
            cx.debug_bounds("hosts-pair-error").is_none()
                && cx.debug_bounds("hosts-pair-address-error").is_none(),
            "the prompt stands in for the transport's cause"
        );
        let retry = |address: &'static str, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel
                        .form
                        .address
                        .update(cx, |state, cx| state.set_value(address, window, cx));
                    panel.submit(window, cx);
                });
            });
            draw(cx);
        };
        retry("192.168.1.99", cx);
        assert!(
            cx.debug_bounds("hosts-pair-address-error").is_some(),
            "a typed address that found no path says so in the prompt"
        );
        assert!(cx.debug_bounds("hosts-pair-error").is_none());
        retry("192.168.1.20", cx);
        assert_eq!(
            phone.attempts.borrow()[2],
            (invite.clone(), Some("192.168.1.20".parse().unwrap())),
            "the same invitation, with the typed address"
        );
        assert_eq!(
            phone.load_hosts().unwrap(),
            [invite.paired("Studio".into())]
        );
        assert!(cx.debug_bounds("hosts-pair-address").is_none());
    }

    /// The Hosts page is where this machine offers a connection: with hosting
    /// off it points at the setting; hosting and accepting devices, it shows
    /// the invitation itself, and the expiry when that runs out.
    #[cfg(feature = "remote-hosting")]
    #[gpui::test]
    fn the_hosts_page_offers_this_machines_invitation(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        use tcode_client::HostLink;
        use tcode_traverse::{HostConfig, HostMux, TraverseHost};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // The machine's host runs on its own threads and tells the window
        // of hosting changes from there, as it does in the app.
        cx.executor().allow_parking();
        let root = crate::store::tests::scratch_root("hosts-invitation");
        std::fs::create_dir_all(&root).unwrap();
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        let mux = HostMux::new(attachments, from_host.clone());
        cx.update(crate::theme::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                mux.clone(),
                root.clone(),
                HostLink::new(to_host, from_host),
                tcode_core::settings::Settings::default(),
            ));
            let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "desk"));
            cx.set_global(ClientAttachment::new(client, true, |_, _, _| {}));
        });
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        let state = cx.new(|_| WindowState::default());
        let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
            HostsProbe(cx.new(|cx| RemotePanel::new(state.clone(), window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        // One frame starts whatever the change moves, the next draws it
        // settled.
        let draw = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..2 {
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.refresh();
                    _ = window.draw(cx);
                });
                cx.executor().advance_clock(crate::edge_back::SETTLE);
            }
        };
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds("hosts-remote-settings").is_some(),
                "with hosting off, the page leads to the setting"
            );
            assert!(cx.debug_bounds("remote-invitation").is_none());
            assert!(cx.debug_bounds("hosting-settings").is_none());
        }

        let host = TraverseHost::start(
            mux,
            HostConfig {
                host_name: "Studio".into(),
                data_dir: root.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        host.new_invitation();
        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.adopt_host(host));
        });
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds("remote-invitation").is_some(),
                "hosting and accepting devices, the invitation is on the page"
            );
            assert!(cx.debug_bounds("hosts-remote-settings").is_none());
            assert!(
                cx.debug_bounds("hosting-settings").is_none(),
                "the hosting controls stay in Settings"
            );
        }

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller
                    .hosting(tcode_protocol::HostingAction::SetEnabled(false))
                    .unwrap();
            });
        });
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(cx.debug_bounds("remote-invitation").is_none());
            assert!(
                cx.debug_bounds("hosts-remote-settings").is_some(),
                "with pairing off, the page leads to the setting"
            );
            assert!(cx.debug_bounds("hosting-settings").is_none());
        }

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "remote-hosting")]
    #[gpui::test]
    fn superseded_pairing_does_not_overwrite_the_saved_machine(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("stale-pairing");
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        cx.update(|cx| cx.set_global(ClientAttachment::new(client.clone(), false, |_, _, _| {})));
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::default());
            PairingProbe(cx.new(|cx| RemotePanel::new(state, window, cx)))
        });
        let host = |name: &str| PairedHost {
            host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
            name: name.into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.1.10:47420".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        probe.update_in(cx, |probe, window, cx| {
            probe.0.update(cx, |panel, cx| {
                let old = panel.form.restart();
                let current = panel.form.restart();
                panel.finish_pair(
                    old,
                    Err(tcode_client::pairing::PairError::Invalid),
                    window,
                    cx,
                );
                assert!(panel.form.error.is_none());
                assert!(client.load_hosts().unwrap().is_empty());
                panel.finish_pair(
                    current,
                    Err(tcode_client::pairing::PairError::Invalid),
                    window,
                    cx,
                );
                assert_eq!(
                    panel.form.error.as_deref(),
                    Some(crate::tr!("hosts.pair.rejected").as_ref())
                );
                assert!(client.load_hosts().unwrap().is_empty());
                let current = panel.form.restart();
                panel.finish_pair(current, Ok(host("current pairing")), window, cx);
                panel.finish_pair(old, Ok(host("superseded pairing")), window, cx);
            });
        });
        assert_eq!(client.load_hosts().unwrap(), vec![host("current pairing")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
