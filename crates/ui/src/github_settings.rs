use crate::{
    sizing::Sizable as _,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::ActiveTheme as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{Input, InputState},
        menu::DropdownMenu as _,
        switch::Switch,
    },
};
use gpui::{
    Action, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use std::collections::{BTreeMap, BTreeSet};
use tcode_core::settings::GitHubCredentialSource;

#[derive(Action, Clone, PartialEq, serde::Deserialize)]
#[action(namespace = github_settings, no_json)]
struct ChooseAccount {
    host: String,
    account: Option<String>,
}

pub struct GitHubSettingsPanel {
    store: Entity<WorkspaceStore>,
    tokens: BTreeMap<String, Entity<InputState>>,
    host_input: Entity<InputState>,
    visible: bool,
    _subscription: Subscription,
}
impl GitHubSettingsPanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
            if change.topic == TopicKind::Settings {
                cx.notify();
            }
        });
        let host_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("source_control.host_placeholder"))
        });
        Self {
            store,
            tokens: BTreeMap::new(),
            host_input,
            visible: false,
            _subscription: subscription,
        }
    }
    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            self.store
                .update(cx, |store, _| store.refresh_github_credentials());
        }
    }
    pub fn hide(&mut self) {
        self.visible = false;
        self.tokens.clear();
    }
}
impl Render for GitHubSettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.store.read(cx).settings().github.clone();
        let hosts: BTreeSet<_> = settings
            .hosts
            .keys()
            .chain(settings.status.keys())
            .cloned()
            .chain(std::iter::once("github.com".into()))
            .collect();
        let mut rows = Vec::new();
        for host in hosts {
            let choice = settings.hosts.get(&host).cloned().unwrap_or_default();
            let status = settings.status.get(&host).cloned().unwrap_or_default();
            let token = self
                .tokens
                .entry(host.clone())
                .or_insert_with(|| {
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(true)
                            .placeholder(crate::tr!("source_control.token_placeholder"))
                    })
                })
                .clone();
            let store = self.store.clone();
            let toggle_host = host.clone();
            let switch = Switch::new(SharedString::from(format!("github-enabled-{host}")))
                .checked(choice.enabled)
                .label(crate::tr!("source_control.enabled").into_owned())
                .on_click(move |enabled, _, cx| {
                    store.update(cx, |store, cx| {
                        store.patch_github_host(toggle_host.clone(), Some(*enabled), None);
                        cx.notify();
                    });
                });
            let source = match status.source {
                Some(GitHubCredentialSource::Saved) => crate::tr!("source_control.source_saved"),
                Some(GitHubCredentialSource::Env) => crate::tr!("source_control.source_env"),
                Some(GitHubCredentialSource::Gh) => crate::tr!("source_control.source_gh"),
                None => crate::tr!("source_control.source_none"),
            };
            let mut row = v_flex()
                .gap_3()
                .p_4()
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_3()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(host.clone()),
                        )
                        .child(switch),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(source.into_owned()),
                );
            if status.accounts.len() > 1 {
                let selected = choice
                    .account
                    .clone()
                    .unwrap_or_else(|| crate::tr!("source_control.active_account").into_owned());
                let selected_account = choice.account.clone();
                let menu_host = host.clone();
                let accounts = status.accounts.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("github-account-{host}")))
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
                                    account: None,
                                }),
                            );
                            for account in &accounts {
                                menu = menu.menu_with_check(
                                    account.clone(),
                                    selected_account.as_ref() == Some(account),
                                    Box::new(ChooseAccount {
                                        host: menu_host.clone(),
                                        account: Some(account.clone()),
                                    }),
                                );
                            }
                            menu
                        }),
                );
            }
            if status.env_overrides_account {
                row = row.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("source_control.env_override").into_owned()),
                );
            }
            row = row.child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if status.token_set {
                        crate::tr!("source_control.token_set").into_owned()
                    } else {
                        crate::tr!("source_control.token_unset").into_owned()
                    }),
            );
            let set_host = host.clone();
            let clear_host = host.clone();
            let input = token.clone();
            let store = self.store.clone();
            let clear_store = self.store.clone();
            row = row.child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(120.))
                            .child(Input::new(&token).small()),
                    )
                    .child(
                        Button::new(SharedString::from(format!("github-token-set-{host}")))
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("source_control.set").into_owned())
                            .on_click(move |_, window, cx| {
                                let value = input.read(cx).value().to_string();
                                if !value.trim().is_empty() {
                                    store.update(cx, |store, _| {
                                        store.set_github_token(set_host.clone(), Some(value))
                                    });
                                    input.update(cx, |input, cx| input.set_value("", window, cx));
                                }
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("github-token-clear-{host}")))
                            .ghost()
                            .compact()
                            .disabled(!status.token_set)
                            .label(crate::tr!("source_control.clear").into_owned())
                            .on_click(move |_, _, cx| {
                                clear_store.update(cx, |store, _| {
                                    store.set_github_token(clear_host.clone(), None)
                                })
                            }),
                    ),
            );
            rows.push(row);
        }
        v_flex()
            .gap_4()
            .on_action(cx.listener(|this, action: &ChooseAccount, _, cx| {
                this.store.update(cx, |store, _| {
                    store.patch_github_host(action.host.clone(), None, Some(action.account.clone()))
                });
            }))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("source_control.description").into_owned()),
            )
            .child(
                v_flex()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(crate::material::radius_card(cx))
                    .children(rows),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(120.))
                            .child(Input::new(&self.host_input).small()),
                    )
                    .child(
                        Button::new("github-add-host")
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("source_control.add_host").into_owned())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let host =
                                    this.host_input.read(cx).value().trim().to_ascii_lowercase();
                                if !host.is_empty() {
                                    this.store.update(cx, |store, _| {
                                        store.patch_github_host(host, Some(true), None)
                                    });
                                    this.host_input
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                            })),
                    ),
            )
    }
}
