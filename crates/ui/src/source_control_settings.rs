//! Settings → Git Platforms: every platform Tcode reads pull requests from, whatever its kind.
//! The host decides which platforms exist, where each token comes from, whose it is and what is
//! wrong; this panel lists them, one row each, and connects or manages one in a dialog (a
//! pushed page on a phone).

mod add;
mod host;

pub(crate) use host::HostEditor;

use crate::{
    icon::{Icon, IconName},
    overlay::OverlayExt as _,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::button::{Button, ButtonVariants as _},
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::collections::BTreeSet;
use tcode_core::{
    pull_request::HostKind,
    settings::{CredentialSource, HostProblem, HostSettings, HostStatus, SourceControlSettings},
};

/// A host kind's name in keys: `github`, `forgejo`, `gitea`, `azure_devops`.
fn slug(kind: HostKind) -> String {
    crate::pull_requests::host_key(kind.terms())
}

/// What a pull request is called on a kind's platforms, in running text.
fn noun(kind: HostKind) -> String {
    crate::pull_requests::Noun::of(kind.terms()).pr
}

fn mark(kind: HostKind) -> Icon {
    Icon::empty().path(kind.terms().mark)
}

/// The platforms to list, each with its kind: configured, reported by the host, and github.com;
/// by kind, then the kind's public platform, then by name.
fn listed(settings: &SourceControlSettings) -> Vec<(String, HostKind)> {
    let names: BTreeSet<_> = settings
        .hosts
        .keys()
        .chain(settings.status.keys())
        .cloned()
        .chain(std::iter::once("github.com".to_owned()))
        .collect();
    let mut listed: Vec<_> = names
        .into_iter()
        .map(|host| {
            let kind = settings.kind(&host);
            (host, kind)
        })
        .collect();
    listed.sort_by(|(left, left_kind), (right, right_kind)| {
        (left_kind, left != left_kind.public_host(), left).cmp(&(
            right_kind,
            right != right_kind.public_host(),
            right,
        ))
    });
    listed
}

/// One platform as the host reports it, with the user's choices for it.
#[derive(Clone)]
pub(crate) struct Platform {
    pub host: String,
    pub kind: HostKind,
    pub choice: HostSettings,
    pub status: Option<HostStatus>,
    /// Only the user's settings name it, so only they may remove it.
    pub added: bool,
}

impl Platform {
    pub(crate) fn read(store: &WorkspaceStore, host: &str) -> Self {
        let settings = &store.settings().source_control;
        let kind = settings.kind(host);
        let choice = settings
            .hosts
            .get(host)
            .cloned()
            .unwrap_or_else(|| HostSettings::new(kind));
        let status = settings.status.get(host).cloned();
        let added = status
            .as_ref()
            .map_or(settings.hosts.contains_key(host), |status| status.added)
            && host != kind.public_host();
        Self {
            host: host.to_owned(),
            kind,
            choice,
            status,
            added,
        }
    }

    pub(crate) fn source(&self) -> Option<&CredentialSource> {
        self.status.as_ref()?.source.as_ref()
    }

    pub(crate) fn standing(&self) -> Standing {
        if !self.choice.enabled {
            return Standing::Paused;
        }
        let status = self.status.as_ref();
        match status.and_then(|status| status.problem.as_ref()) {
            Some(HostProblem::Rejected { status, at }) => Standing::Rejected {
                status: *status,
                at: *at,
            },
            Some(HostProblem::NotSignedIn { tool, .. }) => {
                Standing::NotSignedIn { tool: tool.clone() }
            }
            Some(HostProblem::Unverified) => Standing::Unverified,
            Some(HostProblem::CredentialsUnreadable { error }) => Standing::CredentialsUnreadable {
                error: error.clone(),
            },
            _ if self.source().is_some() => Standing::Connected {
                login: status.and_then(|status| status.login.clone()),
            },
            _ => Standing::NotConnected,
        }
    }

    /// The kind's name, and that the user added it.
    pub(crate) fn kind_line(&self) -> String {
        let mut line = self.kind.terms().name.to_owned();
        if self.added {
            line.push_str(" · ");
            line.push_str(&crate::tr!("source_control.origin_added"));
        }
        line
    }

    /// The command that signs the platform's CLI in, when the host names one.
    pub(crate) fn sign_in(&self) -> Option<(String, String)> {
        match self.status.as_ref()?.problem.as_ref()? {
            HostProblem::NotSignedIn {
                tool,
                command: Some(command),
            } => Some((tool.clone(), command.clone())),
            _ => None,
        }
    }
}

/// Where a platform stands, as its row and its dialogs say it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Standing {
    /// The user turned it off.
    Paused,
    Connected {
        login: Option<String>,
    },
    /// The platform refused the token in use.
    Rejected {
        status: u16,
        at: u64,
    },
    /// A saved token the platform has not answered yet.
    Unverified,
    /// The platform's CLI is there, without a login for it.
    NotSignedIn {
        tool: String,
    },
    /// The machine's saved tokens could not be read.
    CredentialsUnreadable {
        error: String,
    },
    NotConnected,
}

impl Standing {
    /// Whether the row offers to connect rather than to manage.
    pub(crate) fn connects(&self) -> bool {
        matches!(self, Self::NotConnected | Self::NotSignedIn { .. })
    }

    /// A name for tests and debug selectors.
    fn key(&self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Connected { .. } => "connected",
            Self::Rejected { .. } => "rejected",
            Self::Unverified => "unverified",
            Self::NotSignedIn { .. } => "not-signed-in",
            Self::CredentialsUnreadable { .. } => "credentials-unreadable",
            Self::NotConnected => "not-connected",
        }
    }

    /// The status dot's color; none for a paused platform, a ring for one not connected.
    fn dot(&self, cx: &App) -> Option<(Hsla, bool)> {
        let theme = cx.theme();
        match self {
            Self::Paused => None,
            Self::Connected { .. } => Some((theme.success, true)),
            Self::Rejected { .. } | Self::CredentialsUnreadable { .. } => {
                Some((theme.danger, true))
            }
            Self::Unverified | Self::NotSignedIn { .. } => Some((theme.warning, true)),
            Self::NotConnected => Some((theme.muted_foreground, false)),
        }
    }

    fn color(&self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Self::Rejected { .. } | Self::CredentialsUnreadable { .. } => theme.danger,
            Self::Unverified | Self::NotSignedIn { .. } => theme.warning,
            _ => theme.muted_foreground,
        }
    }
}

/// The status in words: who it is connected as, or what is wrong.
pub(crate) fn status_text(standing: &Standing, cx: &App) -> AnyElement {
    let text = |text: String| div().child(text).into_any_element();
    match standing {
        Standing::Connected { login: Some(login) } => connected_as(login, None, cx),
        Standing::Connected { login: None } => text(crate::tr!("source_control.connected").into()),
        Standing::Rejected { .. } => text(crate::tr!("source_control.rejected").into()),
        Standing::Unverified => text(crate::tr!("source_control.unverified").into()),
        Standing::NotSignedIn { tool } => {
            text(crate::tr!("source_control.not_signed_in", tool = tool).into())
        }
        Standing::CredentialsUnreadable { error } => {
            text(crate::tr!("source_control.credentials_unreadable", error = error).into())
        }
        Standing::NotConnected => text(crate::tr!("source_control.not_connected").into()),
        Standing::Paused => text(crate::tr!("source_control.paused").into()),
    }
}

/// "Connected as @login", with the login set apart, and the account's name after it when
/// known.
pub(crate) fn connected_as(login: &str, name: Option<&str>, cx: &App) -> AnyElement {
    const AT: &str = "\u{1}";
    let sentence = match name {
        Some(name) => crate::tr!("source_control.connected_as_named", login = AT, name = name),
        None => crate::tr!("source_control.connected_as", login = AT),
    };
    let (before, after) = sentence.split_once(AT).unwrap_or((&sentence, ""));
    h_flex()
        .flex_wrap()
        .child(before.to_owned())
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(format!("@{login}")),
        )
        .child(after.to_owned())
        .into_any_element()
}

/// Where the token in use comes from, as a row names it.
pub(crate) fn source_label(source: &CredentialSource) -> String {
    match source {
        CredentialSource::Saved | CredentialSource::SavedBearer => {
            crate::tr!("source_control.order_saved").into_owned()
        }
        CredentialSource::SavedBasic { email } => {
            crate::tr!("source_control.source_api_token", email = email).into_owned()
        }
        CredentialSource::Env { name } => {
            crate::tr!("source_control.source_env", name = name).into_owned()
        }
        CredentialSource::Cli { tool } => {
            crate::tr!("source_control.order_cli", tool = tool).into_owned()
        }
    }
}

/// The platform's resolution order, the first that works being used.
pub(crate) fn order(status: Option<&HostStatus>) -> String {
    status
        .map(|status| {
            status
                .order
                .iter()
                .map(|source| match source {
                    CredentialSource::Env { name } => name.clone(),
                    source => source_label(source),
                })
                .collect::<Vec<_>>()
                .join(" → ")
        })
        .unwrap_or_default()
}

/// The status dot before a status, in its color; a ring for a platform not connected.
pub(crate) fn dot(standing: &Standing, cx: &App) -> Option<AnyElement> {
    let (color, filled) = standing.dot(cx)?;
    Some(if filled {
        crate::material::status_dot(color).into_any_element()
    } else {
        crate::material::status_dot(gpui::transparent_black())
            .border_1()
            .border_color(color)
            .into_any_element()
    })
}

/// The second line of a platform's row: its status, then where its token comes from, or its
/// kind when nothing is in use.
pub(crate) fn status_line(platform: &Platform, compact: bool, cx: &App) -> gpui::Div {
    let standing = platform.standing();
    let detail = match (&standing, platform.source()) {
        (Standing::Connected { .. } | Standing::Unverified, Some(source)) => source_label(source),
        _ => platform.kind_line(),
    };
    let selector = format!(
        "source-control-standing-{}-{}",
        platform.host,
        standing.key()
    );
    h_flex()
        .min_w_0()
        .gap_1p5()
        .items_center()
        .text_secondary(compact, cx)
        .debug_selector(move || selector)
        .children(dot(&standing, cx))
        .child(
            div()
                .flex_none()
                .text_color(standing.color(cx))
                .child(status_text(&standing, cx)),
        )
        .child(div().flex_none().child("·"))
        .child(div().min_w_0().truncate().child(detail))
}

/// Emitted to manage a platform on a phone, which pushes a page for it.
pub(crate) struct ManageHost(pub String);

pub struct SourceControlPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<crate::window_state::WindowState>,
    visible: bool,
    _subscription: Subscription,
}

impl EventEmitter<ManageHost> for SourceControlPanel {}

impl SourceControlPanel {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<crate::window_state::WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
            if change.topic == TopicKind::Settings {
                cx.notify();
            }
        });
        Self {
            store,
            window_state,
            visible: false,
            _subscription: subscription,
        }
    }

    /// Opening the page asks the host to report each platform afresh, and to learn whose the
    /// tokens in use are.
    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            self.store
                .update(cx, |store, _| store.refresh_host_credentials());
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Connects a platform in a dialog, or manages it: in a dialog, or on a phone a page.
    fn open(&self, host: &str, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !connect && self.window_state.read(cx).compact {
            cx.emit(ManageHost(host.to_owned()));
            return;
        }
        host::open(self.store.clone(), host.to_owned(), connect, window, cx);
    }

    fn row(&self, platform: &Platform, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let host = platform.host.clone();
        let standing = platform.standing();
        let connect = standing.connects();
        let paused = standing == Standing::Paused;
        let panel = cx.entity().downgrade();
        let open = move |host: &str, window: &mut Window, cx: &mut App| {
            let _ = panel.update(cx, |panel, cx| panel.open(host, connect, window, cx));
        };
        let trailing = if compact && !connect {
            Icon::new(IconName::ChevronRight)
                .text_color(cx.theme().muted_foreground)
                .into_any_element()
        } else {
            let button_host = host.clone();
            let open = open.clone();
            let label = if connect {
                crate::tr!("source_control.connect")
            } else {
                crate::tr!("source_control.manage")
            };
            let id = format!(
                "source-control-{}-{host}",
                if connect { "connect" } else { "manage" }
            );
            let button = Button::new(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .compact()
                .touch(compact)
                .label(label.into_owned())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    open(&button_host, window, cx)
                });
            if connect {
                button.primary().into_any_element()
            } else {
                button.outline().into_any_element()
            }
        };
        let row_host = host.clone();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            SharedString::from(format!("source-control-row-{host}")),
            Role::Button,
            host.clone(),
            cx,
        )
        .debug_selector({
            let selector = format!("source-control-row-{host}");
            move || selector
        })
        .relative()
        .cursor_pointer()
        .child(crate::material::row_hover(px(0.), cx))
        .on_click(move |_, window, cx| open(&row_host, window, cx))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_3()
                .items_center()
                .when(paused, |row| row.opacity(0.6))
                .child(
                    mark(platform.kind)
                        .flex_none()
                        .text_color(cx.theme().foreground),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_body(compact, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(host.clone())),
                        )
                        .child(status_line(platform, compact, cx)),
                ),
        )
        .child(div().flex_none().child(trailing))
        .into_any_element()
    }

    fn add_row(&self, listed: Vec<String>, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let store = self.store.clone();
        let label = crate::tr!("source_control.add_host_menu").into_owned();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            "source-control-add-host",
            Role::Button,
            label.clone(),
            cx,
        )
        .debug_selector(|| "source-control-add-host".into())
        .relative()
        .cursor_pointer()
        .child(crate::material::row_hover(px(0.), cx))
        .on_click(move |_, window, cx| add::open(store.clone(), listed.clone(), window, cx))
        .text_body(compact, cx)
        .text_color(cx.theme().link)
        .child(Icon::new(IconName::Plus).flex_none())
        .child(label)
        .into_any_element()
    }
}

impl Render for SourceControlPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let settings = store.settings().source_control.clone();
        let platforms: Vec<Platform> = listed(&settings)
            .into_iter()
            .map(|(host, _)| Platform::read(store, &host))
            .collect();
        let compact = self.window_state.read(cx).compact;
        let names: Vec<String> = platforms
            .iter()
            .map(|platform| platform.host.clone())
            .collect();
        let rows = platforms
            .iter()
            .map(|platform| (SharedString::from(platform.host.clone()), platform.clone()))
            .collect::<Vec<_>>();
        let rows = crate::settings_rows::grouped_keyed(
            "source-control-hosts",
            rows,
            window,
            cx,
            |_, platform, _, cx| self.row(platform, cx),
        );
        let add = self.add_row(names, cx);
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.description").into_owned()),
            )
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("settings.hosts_section"),
                        cx,
                    ))
                    .child(
                        crate::material::group(cx)
                            .child(rows)
                            .child(crate::material::row_hairline(
                                px(crate::material::CARD_INSET),
                                cx,
                            ))
                            .child(add),
                    )
                    .child(crate::settings_rows::footnote(
                        crate::tr!("source_control.detection_note").into_owned(),
                        compact,
                        cx,
                    )),
            )
    }
}

/// Asks before removing a platform the user added, with its saved token.
pub(crate) fn confirm_remove(
    store: Entity<WorkspaceStore>,
    host: String,
    kind: HostKind,
    then: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let then = std::rc::Rc::new(then);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let store = store.clone();
        let host = host.clone();
        let then = then.clone();
        alert
            .title(crate::tr!("source_control.remove_title", host = &host).into_owned())
            .description(crate::tr!("source_control.remove_desc", pr = noun(kind)).into_owned())
            .button_props(
                crate::overlay::DialogButtons::default()
                    .ok_variant(crate::widgets::button::ButtonVariant::Danger)
                    .ok_text(crate::tr!("source_control.remove"))
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                store.update(cx, |store, _| {
                    store.remove_source_control_host(host.clone())
                });
                then(window, cx);
                true
            })
    });
}

#[cfg(test)]
mod tests;
