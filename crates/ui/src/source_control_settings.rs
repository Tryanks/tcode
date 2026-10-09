//! Settings → Source Control: every host Tcode reads pull requests from, whatever its kind. The
//! host decides which hosts exist, where each credential comes from and what is wrong; this
//! panel shows it and sends the user's choices.

use crate::{
    icon::Icon,
    overlay::{DialogActions, OverlayExt as _},
    sizing::Sizable as _,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::ActiveTheme as _,
    widgets::{
        Tooltip,
        button::{Button, ButtonVariant, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        menu::DropdownMenu as _,
        switch::Switch,
    },
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::collections::{BTreeMap, BTreeSet};
use tcode_core::{
    pull_request::{HostKind, HostRefusal},
    settings::{CredentialSource, HostOrigin, HostProblem, HostSettings, HostStatus},
};

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = source_control_settings, no_json)]
struct ChooseAccount {
    host: String,
    kind: HostKind,
    account: Option<String>,
}

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = source_control_settings, no_json)]
struct ChooseKind {
    kind: HostKind,
}

/// A host kind's name in keys: `github`, `forgejo`, `gitea`.
fn slug(kind: HostKind) -> String {
    kind.terms().name.to_ascii_lowercase()
}

fn mark(kind: HostKind, size: f32) -> Icon {
    Icon::empty().path(kind.terms().mark).size(px(size))
}

/// The hosts to list, each with its kind: configured, reported by the host, and github.com;
/// by kind, then the kind's public host, then by name.
fn listed(
    hosts: &BTreeMap<String, HostSettings>,
    status: &BTreeMap<String, HostStatus>,
) -> Vec<(String, HostKind)> {
    let names: BTreeSet<_> = hosts
        .keys()
        .chain(status.keys())
        .cloned()
        .chain(std::iter::once("github.com".to_owned()))
        .collect();
    let mut listed: Vec<_> = names
        .into_iter()
        .map(|host| {
            let kind = hosts
                .get(&host)
                .map(|choice| choice.kind)
                .or_else(|| status.get(&host).map(|status| status.kind))
                .or_else(|| HostKind::detect(&host))
                .unwrap_or(HostKind::Github);
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

fn source_slot(source: &CredentialSource) -> String {
    match source {
        CredentialSource::Saved => crate::tr!("source_control.order_saved").into_owned(),
        CredentialSource::Env { name } => name.clone(),
        CredentialSource::Cli { tool } => {
            crate::tr!("source_control.order_cli", tool = tool).into_owned()
        }
    }
}

fn status_line(status: Option<&HostStatus>) -> String {
    match status.and_then(|status| status.source.as_ref()) {
        Some(CredentialSource::Saved) => crate::tr!("source_control.using_saved").into_owned(),
        Some(CredentialSource::Env { name }) => {
            crate::tr!("source_control.using_env", name = name).into_owned()
        }
        Some(CredentialSource::Cli { tool }) => {
            crate::tr!("source_control.using_cli", tool = tool).into_owned()
        }
        None => crate::tr!("source_control.not_connected").into_owned(),
    }
}

pub struct SourceControlPanel {
    store: Entity<WorkspaceStore>,
    window_state: Entity<crate::window_state::WindowState>,
    tokens: BTreeMap<String, Entity<InputState>>,
    visible: bool,
    _subscription: Subscription,
}

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
            tokens: BTreeMap::new(),
            visible: false,
            _subscription: subscription,
        }
    }
    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            self.store
                .update(cx, |store, _| store.refresh_host_credentials());
        }
    }
    pub fn hide(&mut self) {
        self.visible = false;
        self.tokens.clear();
    }

    fn notice(&self, host: &str, problem: &HostProblem, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let text = match problem {
            HostProblem::NotSignedIn { tool, .. } => {
                crate::tr!(
                    "source_control.notice_not_signed_in",
                    tool = tool,
                    host = host
                )
            }
            HostProblem::NoCredential { tools_missing } => crate::tr!(
                "source_control.notice_no_credential",
                tools = tools_missing.join(" / ")
            ),
        };
        let command = match problem {
            HostProblem::NotSignedIn {
                command: Some(command),
                ..
            } => Some(command.clone()),
            _ => None,
        };
        let compact = self.window_state.read(cx).compact;
        v_flex()
            .gap_1()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.muted)
            .text_size(px(12.))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(crate::icon::IconName::TriangleAlert)
                            .size(px(12.))
                            .text_color(theme.warning),
                    )
                    .child(div().flex_1().min_w_0().child(text.into_owned())),
            )
            .when_some(command, |notice, command| {
                let copied = command.clone();
                notice.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .min_w_0()
                                .px_1()
                                .rounded_sm()
                                .bg(theme.secondary)
                                .font_family(theme.mono_font_family.clone())
                                .child(command),
                        )
                        .child(crate::widgets::copy::copy_button(
                            &format!("source-control-command-{host}"),
                            false,
                            compact,
                            move |_, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    copied.clone(),
                                ))
                            },
                            cx,
                        )),
                )
            })
            .into_any_element()
    }

    fn row(
        &mut self,
        host: &str,
        kind: HostKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = self.store.read(cx).settings().source_control.clone();
        let choice = settings
            .hosts
            .get(host)
            .cloned()
            .unwrap_or_else(|| HostSettings::new(kind));
        let status = settings.status.get(host).cloned();
        let theme = cx.theme().clone();
        let store = self.store.clone();
        let toggle_host = host.to_owned();
        let switch = Switch::new(SharedString::from(format!("source-control-enabled-{host}")))
            .checked(choice.enabled)
            .tooltip(crate::tr!("source_control.use_host", host = host).into_owned())
            .on_click(move |enabled, _, cx| {
                store.update(cx, |store, cx| {
                    store.patch_source_control_host(
                        toggle_host.clone(),
                        kind,
                        Some(*enabled),
                        None,
                    );
                    cx.notify();
                });
            });
        let added = status
            .as_ref()
            .map_or(settings.hosts.contains_key(host), |status| {
                status.origin == HostOrigin::Added
            })
            && host != kind.public_host();
        let mut kind_line = kind.terms().name.to_owned();
        if added {
            kind_line.push_str(" · ");
            kind_line.push_str(&crate::tr!("source_control.origin_added"));
        }
        let header_color = if choice.enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let header = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(mark(kind, 16.).text_color(header_color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(header_color)
                            .truncate()
                            .child(host.to_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(kind_line),
                    ),
            )
            .child(switch);
        let mut row = v_flex().gap_2().p_4().child(header);
        if !choice.enabled {
            return row
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(crate::tr!("source_control.off", host = host).into_owned()),
                )
                .into_any_element();
        }
        let order = status
            .as_ref()
            .map(|status| {
                status
                    .order
                    .iter()
                    .map(source_slot)
                    .collect::<Vec<_>>()
                    .join(" → ")
            })
            .unwrap_or_default();
        let tooltip = crate::tr!("source_control.order", order = order).into_owned();
        row = row.child(
            div()
                .id(SharedString::from(format!("source-control-source-{host}")))
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(status_line(status.as_ref()))
                .when(!order.is_empty(), |line| {
                    line.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                }),
        );
        if let Some(problem) = status.as_ref().and_then(|status| status.problem.as_ref()) {
            row = row.child(self.notice(host, problem, cx));
        }
        let accounts = status
            .as_ref()
            .map(|status| status.accounts.clone())
            .unwrap_or_default();
        if accounts.len() > 1 {
            let selected = choice
                .account
                .clone()
                .unwrap_or_else(|| crate::tr!("source_control.active_account").into_owned());
            let selected_account = choice.account.clone();
            let menu_host = host.to_owned();
            row = row.child(
                Button::new(SharedString::from(format!("source-control-account-{host}")))
                    .ghost()
                    .outline()
                    .compact()
                    .label(selected)
                    .dropdown_menu(move |mut menu, _, _| {
                        menu = menu.menu_with_check(
                            crate::tr!("source_control.active_account").into_owned(),
                            selected_account.is_none(),
                            Box::new(ChooseAccount {
                                host: menu_host.clone(),
                                kind,
                                account: None,
                            }),
                        );
                        for account in &accounts {
                            menu = menu.menu_with_check(
                                account.clone(),
                                selected_account.as_ref() == Some(account),
                                Box::new(ChooseAccount {
                                    host: menu_host.clone(),
                                    kind,
                                    account: Some(account.clone()),
                                }),
                            );
                        }
                        menu
                    }),
            );
        }
        if status
            .as_ref()
            .is_some_and(|status| status.env_overrides_account)
        {
            row = row.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(crate::tr!("source_control.env_override").into_owned()),
            );
        }
        let token_set = status.as_ref().is_some_and(|status| status.token_set);
        let placeholder = if token_set {
            crate::tr!("source_control.token_placeholder_saved")
        } else {
            crate::tr!("source_control.token_placeholder")
        };
        let token = self
            .tokens
            .entry(host.to_owned())
            .or_insert_with(|| cx.new(|cx| InputState::new(window, cx).masked(true)))
            .clone();
        token.update(cx, |input, cx| {
            input.set_placeholder(placeholder.into_owned(), window, cx)
        });
        let set_host = host.to_owned();
        let clear_host = host.to_owned();
        let input = token.clone();
        let store = self.store.clone();
        let clear_store = self.store.clone();
        let compact = self.window_state.read(cx).compact;
        let field = div()
            .min_w(px(120.))
            .map(|field| {
                if compact {
                    field.w_full()
                } else {
                    field.flex_1()
                }
            })
            .child(Input::new(&token).small());
        let mut buttons = h_flex()
            .gap_2()
            .child(
                Button::new(SharedString::from(format!(
                    "source-control-token-set-{host}"
                )))
                .ghost()
                .outline()
                .compact()
                .when(compact, |button| button.min_h(px(44.)))
                .label(crate::tr!("source_control.set").into_owned())
                .on_click(move |_, window, cx| {
                    let value = input.read(cx).value().to_string();
                    if !value.trim().is_empty() {
                        store.update(cx, |store, _| {
                            store.set_host_token(set_host.clone(), Some(value))
                        });
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                    }
                }),
            )
            .child(
                Button::new(SharedString::from(format!(
                    "source-control-token-clear-{host}"
                )))
                .ghost()
                .compact()
                .when(compact, |button| button.min_h(px(44.)))
                .disabled(!token_set)
                .label(crate::tr!("source_control.clear").into_owned())
                .on_click(move |_, _, cx| {
                    clear_store.update(cx, |store, _| {
                        store.set_host_token(clear_host.clone(), None)
                    })
                }),
            );
        if added {
            let remove_host = host.to_owned();
            let store = self.store.clone();
            buttons = buttons.child(
                Button::new(SharedString::from(format!("source-control-remove-{host}")))
                    .ghost()
                    .compact()
                    .when(compact, |button| button.min_h(px(44.)))
                    .label(crate::tr!("source_control.remove_host").into_owned())
                    .on_click(move |_, window, cx| {
                        confirm_remove(store.clone(), remove_host.clone(), window, cx)
                    }),
            );
        }
        // A phone gives the field the row and puts the buttons under it, at the end.
        let editor = if compact {
            v_flex()
                .gap_2()
                .child(field)
                .child(h_flex().w_full().justify_end().child(buttons))
        } else {
            h_flex().w_full().gap_2().child(field).child(buttons)
        };
        row.child(editor).into_any_element()
    }
}

fn confirm_remove(store: Entity<WorkspaceStore>, host: String, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let store = store.clone();
        let host = host.clone();
        alert
            .bg(cx.theme().popover)
            .title(crate::tr!("source_control.remove_title", host = &host).into_owned())
            .description(crate::tr!("source_control.remove_desc").into_owned())
            .button_props(
                crate::overlay::DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(crate::tr!("source_control.remove_host"))
                    .cancel_text(crate::tr!("settings.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                store.update(cx, |store, _| {
                    store.remove_source_control_host(host.clone())
                });
                true
            })
    });
}

impl Render for SourceControlPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings().source_control.clone();
        let hosts = listed(&settings.hosts, &settings.status);
        let mut rows = Vec::new();
        for (index, (host, kind)) in hosts.iter().enumerate() {
            let row = self.row(host, *kind, window, cx);
            rows.push(
                div()
                    .when(index > 0, |row| {
                        row.border_t_1().border_color(cx.theme().border)
                    })
                    .child(row),
            );
        }
        let compact = self.window_state.read(cx).compact;
        let store = self.store.clone();
        let listed_hosts: Vec<String> = hosts.into_iter().map(|(host, _)| host).collect();
        let add = Button::new("source-control-add-host")
            .outline()
            .small()
            .icon(crate::icon::IconName::Plus)
            .when(compact, |button| button.w_full())
            .label(crate::tr!("source_control.add_host_menu").into_owned())
            .on_click(move |_, window, cx| {
                AddHostDialog::open(store.clone(), listed_hosts.clone(), window, cx)
            });
        let note = div()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(crate::tr!("source_control.detection_note").into_owned());
        v_flex()
            .gap(px(24.))
            .on_action(cx.listener(|this, action: &ChooseAccount, _, cx| {
                this.store.update(cx, |store, _| {
                    store.patch_source_control_host(
                        action.host.clone(),
                        action.kind,
                        None,
                        Some(action.account.clone()),
                    )
                });
            }))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("source_control.description").into_owned()),
            )
            .child(crate::material::group(cx).children(rows))
            .child(if compact {
                v_flex().gap_2().child(add).child(note)
            } else {
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(note.flex_1())
                    .child(add)
            })
    }
}

/// The Add host dialog: a kind and an authority, checked by the kind's own rule before the host
/// is asked to add it.
struct AddHostDialog {
    store: Entity<WorkspaceStore>,
    listed: Vec<String>,
    kind: HostKind,
    input: Entity<InputState>,
    error: Option<String>,
    _subscription: Subscription,
}

impl AddHostDialog {
    fn open(store: Entity<WorkspaceStore>, listed: Vec<String>, window: &mut Window, cx: &mut App) {
        let dialog = cx.new(|cx| {
            let kind = HostKind::Github;
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::tr!(&format!(
                    "source_control.host_placeholder_{}",
                    slug(kind)
                )))
            });
            input.update(cx, |input, cx| {
                input.set_value(kind.public_host(), window, cx);
                input.focus(window, cx);
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                |dialog: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        dialog.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => dialog.add(window, cx),
                    _ => {}
                },
            );
            Self {
                store,
                listed,
                kind,
                input,
                error: None,
                _subscription: subscription,
            }
        });
        let content = dialog.clone();
        let footer = dialog.clone();
        window.open_dialog(cx, move |builder, _, cx| {
            let content = content.clone();
            let cancel = footer.clone();
            let add = footer.clone();
            builder
                .w(px(460.))
                .rounded(crate::material::radius_overlay(cx))
                .bg(cx.theme().popover)
                .border_1()
                .border_color(cx.theme().border)
                .shadow_xl()
                .title(crate::tr!("source_control.add_host").into_owned())
                .content(move |el, _, _| el.child(content.clone()))
                .footer(
                    DialogActions::new()
                        .child(
                            Button::new("source-control-add-cancel")
                                .rounded(crate::material::radius_button(cx))
                                .outline()
                                .small()
                                .label(crate::tr!("settings.cancel"))
                                .on_click(move |_, window, cx| {
                                    let _ = &cancel;
                                    window.close_dialog(cx)
                                }),
                        )
                        .child(
                            Button::new("source-control-add-confirm")
                                .rounded(crate::material::radius_button(cx))
                                .primary()
                                .small()
                                .label(crate::tr!("source_control.add_host"))
                                .on_click(move |_, window, cx| {
                                    add.update(cx, |dialog, cx| dialog.add(window, cx))
                                }),
                        )
                        .into_any_element(),
                )
        });
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().to_string();
        let name = self.kind.terms().name;
        let host = match self.kind.authority(&typed) {
            Ok(host) => host,
            Err(HostRefusal::Blank) => {
                self.error = Some(crate::tr!("source_control.error_blank").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::Invalid) => {
                self.error = Some(crate::tr!("source_control.error_invalid").into_owned());
                return cx.notify();
            }
            Err(HostRefusal::PortOrPath) => {
                self.error =
                    Some(crate::tr!("source_control.error_port_path", kind = name).into_owned());
                return cx.notify();
            }
        };
        if self.listed.contains(&host) {
            self.error = Some(crate::tr!("source_control.error_exists", host = &host).into_owned());
            return cx.notify();
        }
        let kind = self.kind;
        self.store.update(cx, |store, _| {
            store.patch_source_control_host(host, kind, Some(true), None)
        });
        window.close_dialog(cx);
    }

    fn choose(&mut self, kind: HostKind, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().trim().to_ascii_lowercase();
        let previous = self.kind;
        self.kind = kind;
        self.error = None;
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                crate::tr!(&format!("source_control.host_placeholder_{}", slug(kind))).into_owned(),
                window,
                cx,
            );
            if typed.is_empty() || typed == previous.public_host() {
                input.set_value(kind.public_host(), window, cx);
            }
        });
        cx.notify();
    }
}

impl Render for AddHostDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let kind = self.kind;
        let label = |text: SharedString| {
            div()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(text)
        };
        v_flex()
            .gap_2()
            .on_action(cx.listener(|this, action: &ChooseKind, window, cx| {
                this.choose(action.kind, window, cx)
            }))
            .child(label(
                crate::tr!("source_control.kind_label").into_owned().into(),
            ))
            .child(
                Button::new("source-control-add-kind")
                    .outline()
                    .small()
                    .w_full()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .child(mark(kind, 16.))
                            .child(kind.terms().name)
                            .child(div().ml_auto().child(
                                Icon::new(crate::icon::IconName::ChevronDown).size(px(14.)),
                            )),
                    )
                    // gpui-base has no Select; a button with a menu stands in for one.
                    .dropdown_menu(move |mut menu, _, _| {
                        for choice in HostKind::ALL {
                            menu = menu.menu_with_check(
                                choice.terms().name,
                                choice == kind,
                                Box::new(ChooseKind { kind: choice }),
                            );
                        }
                        menu
                    }),
            )
            .child(label(
                crate::tr!("source_control.host_label").into_owned().into(),
            ))
            .child(Input::new(&self.input).small())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(crate::tr!(&format!("source_control.note_{}", slug(kind))).into_owned()),
            )
            .child(
                div()
                    .min_h(px(16.))
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .children(self.error.clone()),
            )
    }
}
