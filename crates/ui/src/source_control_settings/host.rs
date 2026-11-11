//! Connecting one platform, or managing it: the token the platform takes, checked by the host
//! against the platform before it is saved; the account a CLI login uses; the switch that
//! pauses it; and disconnecting or removing it.

use super::{Platform, Standing, mark, noun};
use crate::{
    icon::{Icon, IconName},
    overlay::{DialogActions, OverlayExt as _},
    sizing::Sizable as _,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip, TooltipTrigger as _,
        button::{Button, ButtonVariant, ButtonVariants as _},
        choice::{Choice, ChoiceGroup},
        input::{Input, InputEvent, InputState},
    },
};
use gpui::{
    AnyElement, App, AppContext as _, Axis, Context, DismissEvent, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{Radio, RadioGroup, h_flex, v_flex};
use tcode_core::settings::{CredentialSource, HostAccount, TokenCheck};
use tcode_protocol::CommandResponse;

/// What the host answered the last token sent to it with.
#[derive(Clone, Debug)]
enum Answer {
    Checked(TokenCheck),
    /// The host could not be asked at all.
    Failed(String),
}

pub(crate) struct HostEditor {
    store: Entity<WorkspaceStore>,
    pub(crate) host: String,
    /// Opened to connect a platform with nothing in use: the steps to make a token.
    connect: bool,
    /// Drawn as a phone's pushed page rather than in a dialog.
    page: bool,
    /// Drawn for touch, at the phone's text sizes.
    compact: bool,
    token: Entity<InputState>,
    email: Entity<InputState>,
    /// Whether the token typed is one sent with the account's email, where the kind takes one;
    /// unset until chosen, when the saved one decides.
    with_email: Option<bool>,
    /// A CLI login or an environment token is in use and a token is being typed instead.
    replacing: bool,
    verifying: bool,
    answer: Option<Answer>,
    /// The kind the user just added the host as, which stands until settings name it.
    chosen: Option<tcode_core::pull_request::HostKind>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DismissEvent> for HostEditor {}

impl HostEditor {
    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        host: String,
        connect: bool,
        page: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let token = cx.new(|cx| InputState::new(window, cx).masked(true));
        let email = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("source_control.bitbucket_email"))
        });
        let typed = |editor: &mut Self,
                     _: &Entity<InputState>,
                     event: &InputEvent,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            match event {
                InputEvent::Change => {
                    if !matches!(
                        editor.answer,
                        Some(Answer::Checked(TokenCheck::Accepted { .. }))
                    ) {
                        editor.answer = None;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => editor.verify(window, cx),
                _ => {}
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&token, window, typed),
            cx.subscribe_in(&email, window, typed),
            cx.subscribe(&store, |_, _, change: &StoreChange, cx| {
                if change.topic == TopicKind::Settings {
                    cx.notify();
                }
            }),
        ];
        Self {
            compact: crate::window_seam::window_is_compact(window, cx),
            store,
            host,
            connect,
            page,
            token,
            email,
            with_email: None,
            replacing: false,
            verifying: false,
            answer: None,
            chosen: None,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn focus_token(&self, window: &mut Window, cx: &mut App) {
        self.token.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The editor of a host the user has just added as `kind`.
    pub(crate) fn chosen(mut self, kind: tcode_core::pull_request::HostKind) -> Self {
        self.chosen = Some(kind);
        self
    }

    /// `None` once the host has no kind Tcode knows, as when its settings were removed.
    fn platform(&self, cx: &App) -> Option<Platform> {
        Platform::read(self.store.read(cx), &self.host, self.chosen)
    }

    /// Whether the token typed goes with the account's email.
    fn with_email(&self, platform: &Platform) -> bool {
        platform.kind.takes_email()
            && self
                .with_email
                .unwrap_or(platform.source() != Some(&CredentialSource::SavedBearer))
    }

    /// The platform took the token: whose it is, when it said.
    fn accepted(&self) -> Option<Option<&HostAccount>> {
        match &self.answer {
            Some(Answer::Checked(TokenCheck::Accepted { account })) => Some(account.as_ref()),
            _ => None,
        }
    }

    /// Whether a token is asked for: nothing is in use, a saved one is, or the user chose to
    /// replace a login or an environment token with one.
    fn asks_for_token(&self, platform: &Platform) -> bool {
        self.connect
            || self.replacing
            || !matches!(
                platform.source(),
                Some(CredentialSource::Cli { .. } | CredentialSource::Env { .. })
            )
    }

    fn typed(&self, platform: &Platform, cx: &App) -> Option<(String, Option<String>)> {
        let token = self.token.read(cx).value().trim().to_owned();
        if token.is_empty() {
            return None;
        }
        let email = if self.with_email(platform) {
            let email = self.email.read(cx).value().trim().to_owned();
            if email.is_empty() {
                return None;
            }
            Some(email)
        } else {
            None
        };
        Some((token, email))
    }

    fn verify(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(platform) = self.platform(cx) else {
            return;
        };
        let Some((token, email)) = self.typed(&platform, cx) else {
            return;
        };
        if self.verifying || self.accepted().is_some() {
            return;
        }
        self.verifying = true;
        self.answer = None;
        cx.notify();
        let host = self.host.clone();
        let asked = self.store.update(cx, |store, cx| {
            store.verify_host_token(host, token, email, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let response = asked.await;
            let _ = this.update_in(cx, |editor, window, cx| {
                editor.verifying = false;
                editor.answer = Some(match response {
                    Ok(CommandResponse::TokenCheck(check)) => Answer::Checked(check),
                    Ok(other) => Answer::Failed(format!("{other:?}")),
                    Err(error) => Answer::Failed(error.message),
                });
                if editor.accepted().is_some() {
                    editor
                        .token
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    editor.replacing = false;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Saves the token the host could not check, to be checked once the platform answers.
    fn save_anyway(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(platform) = self.platform(cx) else {
            return;
        };
        let Some((token, email)) = self.typed(&platform, cx) else {
            return;
        };
        let host = self.host.clone();
        self.store.update(cx, |store, _| {
            store.set_host_token(host, Some(token), email)
        });
        self.token
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.finish(window, cx);
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page {
            cx.emit(DismissEvent);
        } else {
            window.close_dialog(cx);
        }
    }
}

fn confirm_disconnect(
    store: Entity<WorkspaceStore>,
    host: String,
    platform: &Platform,
    window: &mut Window,
    cx: &mut App,
) {
    {
        let fallback: Vec<_> = platform
            .status
            .iter()
            .flat_map(|status| &status.order)
            .filter(|source| {
                matches!(
                    source,
                    CredentialSource::Env { .. } | CredentialSource::Cli { .. }
                )
            })
            .map(|source| match source {
                CredentialSource::Env { name } => name.clone(),
                source => super::source_label(source),
            })
            .collect();
        let description = if fallback.is_empty() {
            crate::tr!("source_control.disconnect_desc").into_owned()
        } else {
            crate::tr!(
                "source_control.disconnect_desc_fallback",
                order = fallback.join(" → ")
            )
            .into_owned()
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let host = host.clone();
            alert
                .title(crate::tr!("source_control.disconnect_title", host = &host).into_owned())
                .description(description.clone())
                .button_props(
                    crate::overlay::DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("source_control.disconnect_confirm"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _| {
                        store.set_host_token(host.clone(), None, None)
                    });
                    true
                })
        });
    }
}

impl HostEditor {
    fn confirm_remove(&self, platform: &Platform, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.entity().downgrade();
        let page = self.page;
        super::confirm_remove(
            self.store.clone(),
            self.host.clone(),
            platform.kind,
            move |window, cx| {
                if page {
                    let _ = editor.update(cx, |_, cx| cx.emit(DismissEvent));
                } else {
                    window.close_all_dialogs(cx);
                }
            },
            window,
            cx,
        );
    }

    /// The dialog's title: the platform's mark and address over its kind.
    pub(crate) fn title(&self, cx: &App) -> AnyElement {
        let Some(platform) = self.platform(cx) else {
            return div().into_any_element();
        };
        let compact = self.compact;
        h_flex()
            .gap_3()
            .items_center()
            .child(mark(platform.kind).flex_none())
            .child(
                v_flex()
                    .min_w_0()
                    .child(if self.connect {
                        crate::tr!("source_control.connect_title", host = &self.host).into_owned()
                    } else {
                        self.host.clone()
                    })
                    .child(
                        div()
                            .text_secondary(compact, cx)
                            .font_weight(gpui::FontWeight::NORMAL)
                            .child(platform.kind_line()),
                    ),
            )
            .into_any_element()
    }

    /// The dialog's actions: disconnecting and removing to the left, and to the right the
    /// check, or Done once there is nothing to check.
    pub(crate) fn footer(editor: &Entity<Self>, cx: &App) -> AnyElement {
        let this = editor.read(cx);
        let Some(platform) = this.platform(cx) else {
            return div().into_any_element();
        };
        let mut leading = Vec::new();
        if !this.connect && this.accepted().is_none() {
            leading.extend(this.danger_actions(editor, &platform, cx));
        }
        let mut actions = DialogActions::new().leading(leading);
        if this.accepted().is_some() || !this.asks_for_token(&platform) {
            let done = editor.clone();
            actions = actions.child(
                Button::new("source-control-done")
                    .debug_selector(|| "source-control-done".into())
                    .primary()
                    .small()
                    .label(crate::tr!("source_control.done"))
                    .on_click(move |_, window, cx| {
                        done.update(cx, |editor, cx| editor.finish(window, cx))
                    }),
            );
            return actions.into_any_element();
        }
        let verify = editor.clone();
        actions
            .child(
                Button::new("source-control-cancel")
                    .debug_selector(|| "source-control-cancel".into())
                    .outline()
                    .small()
                    .label(crate::tr!("settings.cancel"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                this.verify_button(&platform, cx)
                    .small()
                    .on_click(move |_, window, cx| {
                        verify.update(cx, |editor, cx| editor.verify(window, cx))
                    }),
            )
            .into_any_element()
    }

    fn verify_button(&self, platform: &Platform, cx: &App) -> Button {
        Button::new("source-control-verify")
            .debug_selector(|| "source-control-verify".into())
            .primary()
            .loading(self.verifying)
            .disabled(self.typed(platform, cx).is_none())
            .label(if self.verifying {
                crate::tr!("source_control.verifying")
            } else if self.connect {
                crate::tr!("source_control.verify_connect")
            } else {
                crate::tr!("source_control.verify_save")
            })
    }

    fn danger_actions(
        &self,
        editor: &Entity<Self>,
        platform: &Platform,
        cx: &App,
    ) -> Vec<AnyElement> {
        let compact = self.compact;
        let mut actions = Vec::new();
        let token_set = platform
            .status
            .as_ref()
            .is_some_and(|status| status.token_set);
        let action = |id: &'static str, label: SharedString| {
            Button::new(id)
                .debug_selector(move || id.into())
                .text()
                .small()
                .touch(compact)
                .label(label)
                .text_color(cx.theme().danger)
        };
        if token_set {
            let disconnect = editor.clone();
            let shown = platform.clone();
            actions.push(
                action(
                    "source-control-disconnect",
                    crate::tr!("source_control.disconnect").into_owned().into(),
                )
                .on_click(move |_, window, cx| {
                    let (store, host) = {
                        let editor = disconnect.read(cx);
                        (editor.store.clone(), editor.host.clone())
                    };
                    confirm_disconnect(store, host, &shown, window, cx)
                })
                .into_any_element(),
            );
        }
        if platform.added {
            let remove = editor.clone();
            let shown = platform.clone();
            actions.push(
                action(
                    "source-control-remove",
                    crate::tr!("source_control.remove_host").into_owned().into(),
                )
                .on_click(move |_, window, cx| {
                    remove.update(cx, |editor, cx| editor.confirm_remove(&shown, window, cx))
                })
                .into_any_element(),
            );
        }
        actions
    }

    /// The card a check's answer leaves under the token, in its tone; "Save anyway" when the
    /// host could not check it.
    fn answer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let host = &self.host;
        let (text, tone, anyway, key) = match self.answer.as_ref()? {
            Answer::Checked(TokenCheck::Accepted { .. }) => return None,
            Answer::Checked(TokenCheck::Rejected { status }) => (
                crate::tr!(
                    "source_control.check_rejected",
                    host = host,
                    status = status
                ),
                theme.danger,
                false,
                "rejected",
            ),
            Answer::Checked(TokenCheck::MissingScopes { scopes }) => (
                crate::tr!(
                    "source_control.check_missing_scopes",
                    host = host,
                    scopes = scopes.join(", ")
                ),
                theme.danger,
                false,
                "missing-scopes",
            ),
            Answer::Checked(TokenCheck::Unreachable { status: None }) => (
                crate::tr!("source_control.check_unreachable", host = host),
                theme.warning,
                true,
                "unreachable",
            ),
            Answer::Checked(TokenCheck::Unreachable {
                status: Some(status),
            }) => (
                crate::tr!(
                    "source_control.check_unexpected",
                    host = host,
                    status = status
                ),
                theme.warning,
                true,
                "unreachable",
            ),
            Answer::Checked(TokenCheck::Unreadable { status, error }) => (
                crate::tr!(
                    "source_control.check_unreadable",
                    host = host,
                    status = status,
                    error = error
                ),
                theme.warning,
                true,
                "unreadable",
            ),
            Answer::Checked(TokenCheck::Unconfirmed) => (
                crate::tr!("source_control.check_unconfirmed", host = host),
                theme.warning,
                true,
                "unconfirmed",
            ),
            Answer::Failed(reason) => (
                crate::tr!("source_control.check_failed", reason = reason),
                theme.danger,
                false,
                "failed",
            ),
        };
        let compact = self.compact;
        let selector = format!("source-control-check-{key}");
        Some(
            h_flex()
                .w_full()
                .gap_2()
                .items_start()
                .debug_selector(move || selector)
                .text_secondary(compact, cx)
                .text_color(tone)
                .child(Icon::new(IconName::TriangleAlert).flex_none().mt(px(2.)))
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .flex_wrap()
                        .gap_1()
                        .child(text.into_owned())
                        .when(anyway, |line| {
                            line.child(
                                Button::new("source-control-save-anyway")
                                    .debug_selector(|| "source-control-save-anyway".into())
                                    .text()
                                    .xsmall()
                                    .label(crate::tr!("source_control.save_anyway"))
                                    .text_color(cx.theme().link)
                                    .on_click(cx.listener(|editor, _, window, cx| {
                                        editor.save_anyway(window, cx)
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// The account the platform says the token is, with what Tcode now does there.
    fn connected(&self, account: Option<&HostAccount>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let compact = self.compact;
        let Some(platform) = self.platform(cx) else {
            return div().into_any_element();
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(
                crate::material::notice(theme.success, cx)
                    .debug_selector(|| "source-control-connected".into())
                    .text_body(compact, cx)
                    .text_color(theme.success)
                    .child(Icon::new(IconName::Check).flex_none())
                    .child(match account {
                        Some(account) => {
                            super::connected_as(&account.login, account.name.as_deref(), cx)
                        }
                        None => div()
                            .child(crate::widgets::fade_text(
                                crate::tr!("source_control.connected").into_owned(),
                            ))
                            .into_any_element(),
                    }),
            )
            .child(
                div().text_secondary(compact, cx).child(
                    crate::tr!(
                        "source_control.connected_body",
                        host = &self.host,
                        pr = noun(platform.kind)
                    )
                    .into_owned(),
                ),
            )
            .into_any_element()
    }

    /// The command that signs the platform's CLI in, with a copy button, under `lead`.
    fn command_card(&self, lead: String, command: String, warning: bool, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let compact = self.compact;
        let copied = command.clone();
        crate::material::card_secondary(cx)
            .w_full()
            .debug_selector(|| "source-control-command".into())
            .gap_2()
            .text_secondary(compact, cx)
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .when(warning, |lead| {
                        lead.child(
                            Icon::new(IconName::TriangleAlert)
                                .flex_none()
                                .mt(px(2.))
                                .text_color(theme.warning),
                        )
                    })
                    .child(div().flex_1().min_w_0().child(lead)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        // Wraps between words only, so a URL in the command stays whole.
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .px_2()
                            .py_1()
                            .rounded(crate::material::radius_chip(cx))
                            .bg(theme.secondary)
                            .text_color(theme.foreground)
                            .font_family(theme.mono_font_family.clone())
                            .children(
                                command
                                    .split_inclusive(' ')
                                    .map(|word| div().whitespace_nowrap().child(word.to_owned())),
                            ),
                    )
                    .child(crate::widgets::copy::copy_button(
                        &format!("source-control-command-{}", self.host),
                        false,
                        compact,
                        move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied.clone()))
                        },
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// The scopes the token needs and where to create it.
    fn token_help(&self, platform: &Platform, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact;
        let status = platform.status.clone();
        let with_email = self.with_email(platform);
        let mut help = v_flex().w_full().gap_1p5().items_start();
        if platform.kind.takes_email() {
            let method = |id: &str, label: SharedString, chosen: bool, cx: &mut Context<Self>| {
                Choice::new(
                    SharedString::from(format!("source-control-method-{id}")),
                    label,
                )
                .selected(with_email == chosen)
                .on_select({
                    let editor = cx.entity().downgrade();
                    move |_, cx| {
                        let _ = editor.update(cx, |editor, cx| {
                            editor.with_email = Some(chosen);
                            editor.answer = None;
                            cx.notify();
                        });
                    }
                })
            };
            let api = method(
                "api",
                crate::tr!("source_control.bitbucket_api_token")
                    .into_owned()
                    .into(),
                true,
                cx,
            );
            let access = method(
                "access",
                crate::tr!("source_control.bitbucket_access_token")
                    .into_owned()
                    .into(),
                false,
                cx,
            );
            help = help.child(
                ChoiceGroup::new("source-control-method")
                    .compact(compact)
                    .choices([api, access]),
            );
            let (description, label, link) = if with_email {
                (
                    crate::tr!("source_control.bitbucket_api_desc"),
                    crate::tr!("source_control.bitbucket_api_link"),
                    status.as_ref().and_then(|status| status.token_url.clone()),
                )
            } else {
                (
                    crate::tr!("source_control.bitbucket_access_desc"),
                    crate::tr!("source_control.bitbucket_access_link"),
                    status
                        .as_ref()
                        .and_then(|status| status.access_token_url.clone()),
                )
            };
            return help
                .child(
                    div()
                        .text_secondary(compact, cx)
                        .child(description.into_owned()),
                )
                .when_some(link, |help, url| {
                    help.child(crate::settings_rows::external_link(
                        "source-control-token-link",
                        label.into_owned().into(),
                        url,
                    ))
                })
                .into_any_element();
        }
        let scopes = status
            .as_ref()
            .map(|status| status.token_scopes.clone())
            .unwrap_or_default();
        help.when(!scopes.is_empty(), |help| {
            help.child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .items_center()
                    .text_secondary(compact, cx)
                    .child(crate::widgets::fade_text(
                        crate::tr!("source_control.scopes_needed").into_owned(),
                    ))
                    .children(scopes.into_iter().map(|scope| {
                        crate::material::chip(scope, cx)
                            .font_family(cx.theme().mono_font_family.clone())
                    })),
            )
        })
        .when_some(status.and_then(|status| status.token_url), |help, url| {
            help.child(crate::settings_rows::external_link(
                "source-control-token-link",
                crate::tr!("source_control.open_token_page", host = &self.host)
                    .into_owned()
                    .into(),
                url,
            ))
        })
        .into_any_element()
    }

    /// The email field, for an API token, and the token field with its answer.
    fn token_fields(&self, platform: &Platform, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.compact;
        let placeholder = if self.connect {
            crate::tr!("source_control.token_placeholder")
        } else {
            crate::tr!("source_control.token_placeholder_new")
        };
        v_flex()
            .w_full()
            .gap_1p5()
            .when(self.with_email(platform), |fields| {
                fields.child(
                    Input::new(&self.email).aria_label(
                        crate::tr!("source_control.bitbucket_email_label", host = &self.host)
                            .into_owned(),
                    ),
                )
            })
            .child(Input::new(&self.token).placeholder(placeholder).aria_label(
                crate::tr!("source_control.token_label", host = &self.host).into_owned(),
            ))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.token_kept").into_owned()),
            )
            .children(self.answer(cx))
            .into_any_element()
    }

    /// A step of connecting: its number beside its title and content.
    fn step(number: usize, title: String, content: impl IntoElement, cx: &App) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .w_full()
            .gap_3()
            .items_start()
            .child(
                div()
                    .flex_none()
                    .size(px(20.))
                    .rounded_full()
                    .bg(theme.secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_token(&theme.tokens.typography.xs)
                    .text_color(theme.muted_foreground)
                    .child(number.to_string()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(div().text_body(false, cx).child(title))
                    .child(content),
            )
            .into_any_element()
    }

    fn render_connect(&mut self, platform: &Platform, cx: &mut Context<Self>) -> gpui::Div {
        if let Some(account) = self.accepted() {
            let account = account.cloned();
            return v_flex()
                .w_full()
                .child(self.connected(account.as_ref(), cx));
        }
        let help = self.token_help(platform, cx);
        let fields = self.token_fields(platform, cx);
        let theme_gap = cx.theme().tokens.spacing.lg;
        v_flex()
            .w_full()
            .gap(theme_gap)
            .child(Self::step(
                1,
                crate::tr!("source_control.connect_step_token", host = &self.host).into_owned(),
                help,
                cx,
            ))
            .child(Self::step(
                2,
                crate::tr!("source_control.token_field").into_owned(),
                fields,
                cx,
            ))
            .children(platform.sign_in().map(|(tool, command)| {
                self.command_card(
                    crate::tr!("source_control.cli_alternative", tool = &tool).into_owned(),
                    command,
                    false,
                    cx,
                )
            }))
    }

    /// A field's name beside its value.
    fn field(label: String, value: impl IntoElement, compact: bool, cx: &App) -> AnyElement {
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .text_secondary(compact, cx)
            .child(
                div()
                    .flex_none()
                    .min_w(px(56.))
                    .child(crate::widgets::fade_text(label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(cx.theme().foreground)
                    .child(value),
            )
            .into_any_element()
    }

    /// The problem the platform has, when one is for the user to solve.
    fn problem(&self, platform: &Platform, cx: &App) -> Option<AnyElement> {
        let compact = self.compact;
        let theme = cx.theme();
        match platform.standing() {
            Standing::Rejected { status, at } => {
                let ago = crate::time::humanize_ago(
                    self.store.read(cx).host_now_secs(cx).saturating_sub(at),
                );
                Some(
                    crate::material::notice(theme.danger, cx)
                        .debug_selector(|| "source-control-problem-rejected".into())
                        .items_start()
                        .text_secondary(compact, cx)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .flex_none()
                                .mt(px(2.))
                                .text_color(theme.danger),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_color(theme.danger)
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(crate::widgets::fade_text(
                                            crate::tr!("source_control.rejected").into_owned(),
                                        )),
                                )
                                .child(
                                    div().text_color(theme.foreground).child(
                                        crate::tr!(
                                            "source_control.problem_rejected",
                                            host = &self.host,
                                            ago = ago,
                                            status = status,
                                            pr = noun(platform.kind)
                                        )
                                        .into_owned(),
                                    ),
                                ),
                        )
                        .into_any_element(),
                )
            }
            Standing::NotSignedIn { .. } => platform.sign_in().map(|(tool, command)| {
                self.command_card(
                    crate::tr!(
                        "source_control.notice_not_signed_in",
                        tool = &tool,
                        host = &self.host
                    )
                    .into_owned(),
                    command,
                    true,
                    cx,
                )
            }),
            _ => None,
        }
    }

    /// A CLI's accounts on the platform, to choose the one Tcode uses.
    fn accounts(&self, platform: &Platform, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = platform.status.as_ref()?;
        let Some(CredentialSource::Cli { tool }) = &status.source else {
            return None;
        };
        if status.accounts.len() < 2 {
            return None;
        }
        let compact = self.compact;
        let chosen = platform
            .choice
            .account
            .clone()
            .filter(|account| status.accounts.contains(account));
        let active = status
            .login
            .clone()
            .filter(|_| chosen.is_none())
            .map(|login| {
                crate::tr!(
                    "source_control.active_account_now",
                    login = format!("@{login}")
                )
                .into_owned()
            });
        let options: Vec<(Option<String>, String, Option<String>)> = std::iter::once((
            None,
            crate::tr!("source_control.active_account").into_owned(),
            active,
        ))
        .chain(
            status
                .accounts
                .iter()
                .map(|account| (Some(account.clone()), format!("@{account}"), None)),
        )
        .collect();
        let count = options.len();
        let theme = cx.theme().clone();
        let rows = options
            .into_iter()
            .enumerate()
            .map(|(index, (account, label, detail))| {
                let checked = account == chosen;
                let store = self.store.clone();
                let host = self.host.clone();
                let kind = platform.kind;
                let indicator = div()
                    .flex_none()
                    .size(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(if checked { theme.primary } else { theme.border })
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(checked, |ring| {
                        ring.child(div().size(px(8.)).rounded_full().bg(theme.primary))
                    });
                let id = SharedString::from(format!(
                    "source-control-account-{}",
                    account.as_deref().unwrap_or("active")
                ));
                let selector = id.to_string();
                Radio::new(id)
                    .debug_selector(move || selector)
                    .checked(checked)
                    .set_position(index + 1, count)
                    .accessibility_label(label.clone())
                    .flex()
                    .items_center()
                    .gap_3()
                    .w_full()
                    .px(px(crate::material::CARD_INSET))
                    .py_2()
                    .cursor_pointer()
                    .text_body(compact, cx)
                    .on_change(move |_, _, _, cx| {
                        store.update(cx, |store, _| {
                            store.patch_source_control_host(
                                host.clone(),
                                kind,
                                None,
                                Some(account.clone()),
                            )
                        })
                    })
                    .child(indicator)
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(crate::widgets::fade_text(label))
                            .children(
                                detail
                                    .map(|detail| div().text_secondary(compact, cx).child(detail)),
                            ),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .w_full()
                .gap_1p5()
                .child(div().text_caption(cx).child(crate::widgets::fade_text(
                    crate::tr!("source_control.cli_accounts", tool = tool).into_owned(),
                )))
                .child(
                    crate::material::grouped(
                        vec![
                            RadioGroup::new("source-control-accounts")
                                .axis(Axis::Vertical)
                                .aria_label(
                                    crate::tr!("source_control.cli_accounts", tool = tool)
                                        .into_owned(),
                                )
                                .children(rows)
                                .into_any_element(),
                        ],
                        cx,
                    )
                    .shadow_none(),
                )
                .into_any_element(),
        )
    }

    fn render_manage(&mut self, platform: &Platform, cx: &mut Context<Self>) -> gpui::Div {
        let compact = self.compact;
        let standing = platform.standing();
        let gap = cx.theme().tokens.spacing.lg;
        let mut body = v_flex()
            .w_full()
            .gap(gap)
            .children(self.problem(platform, cx));
        let order = super::order(platform.status.as_ref());
        let source = platform.source().cloned();
        let mut fields = v_flex().w_full().gap_1().child(Self::field(
            crate::tr!("source_control.field_status").into_owned(),
            h_flex()
                .gap_1p5()
                .items_center()
                .children(super::dot(&standing, cx))
                .child(super::status_text(&standing, cx)),
            compact,
            cx,
        ));
        if let Some(source) = &source {
            let tooltip = crate::tr!("source_control.order", order = order).into_owned();
            fields = fields.child(Self::field(
                crate::tr!("source_control.field_source").into_owned(),
                div()
                    .id("source-control-source")
                    .child(crate::widgets::fade_text(super::source_label(source)))
                    .with_tooltip(move |_, _| Tooltip::new(tooltip.clone())),
                compact,
                cx,
            ));
        }
        if let Some(login) = platform
            .status
            .as_ref()
            .and_then(|status| status.login.clone())
        {
            fields = fields.child(Self::field(
                crate::tr!("source_control.field_account").into_owned(),
                crate::widgets::fade_text(format!("@{login}")),
                compact,
                cx,
            ));
        }
        body = body.child(fields);
        if let Some(account) = self.accepted() {
            let account = account.cloned();
            return body.child(self.connected(account.as_ref(), cx));
        }
        body = body.children(self.accounts(platform, cx));
        if let Some(CredentialSource::Env { name }) = &source {
            body = body.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.env_note", name = name).into_owned()),
            );
        }
        if platform
            .status
            .as_ref()
            .is_some_and(|status| status.env_overrides_account)
        {
            body = body.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.env_override").into_owned()),
            );
        }
        if self.asks_for_token(platform) {
            let help = self.token_help(platform, cx);
            let fields = self.token_fields(platform, cx);
            body = body.child(
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(div().text_caption(cx).child(crate::widgets::fade_text(
                        crate::tr!("source_control.new_token").into_owned(),
                    )))
                    .child(fields)
                    .child(help),
            );
        } else {
            body = body.child(
                v_flex()
                    .items_start()
                    .gap_0p5()
                    .child(
                        Button::new("source-control-use-token")
                            .debug_selector(|| "source-control-use-token".into())
                            .text()
                            .xsmall()
                            .label(crate::tr!("source_control.use_token_instead"))
                            .text_color(cx.theme().link)
                            .on_click(cx.listener(|editor, _, window, cx| {
                                editor.replacing = true;
                                editor.token.update(cx, |input, cx| input.focus(window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_secondary(compact, cx)
                            .child(crate::tr!("source_control.saved_first").into_owned()),
                    ),
            );
        }
        if let Some(CredentialSource::Cli { tool }) = &source {
            body = body.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("source_control.cli_logout_note", tool = tool).into_owned()),
            );
        }
        let store = self.store.clone();
        let host = self.host.clone();
        let kind = platform.kind;
        let switch = crate::settings_rows::ToggleRow::new(
            "source-control-enabled",
            crate::tr!("source_control.use_title").into_owned(),
            platform.choice.enabled,
            compact,
            move |enabled, _, cx| {
                store.update(cx, |store, _| {
                    store.patch_source_control_host(host.clone(), kind, Some(*enabled), None)
                })
            },
        )
        .description(if platform.added {
            crate::tr!(
                "source_control.use_desc",
                host = &self.host,
                pr = noun(kind)
            )
            .into_owned()
        } else {
            format!(
                "{} {}",
                crate::tr!(
                    "source_control.use_desc",
                    host = &self.host,
                    pr = noun(kind)
                ),
                crate::tr!("source_control.not_removable", host = &self.host)
            )
        });
        body.child(crate::material::group(cx).shadow_none().child(switch))
    }
}

impl Render for HostEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(platform) = self.platform(cx) else {
            return div();
        };
        let body = if self.connect {
            self.render_connect(&platform, cx)
        } else {
            self.render_manage(&platform, cx)
        };
        if !self.page {
            return body;
        }
        // A pushed page has no dialog footer: the check runs from a full-width button under the
        // token, and the platform's destructive actions close the page as their own group.
        let editor = cx.entity();
        let asks = self.accepted().is_none() && self.asks_for_token(&platform);
        let dangers = self.danger_actions(&editor, &platform, cx);
        body.when(asks, |body| {
            let verify = editor.clone();
            body.child(
                self.verify_button(&platform, cx)
                    .w_full()
                    .touch(true)
                    .on_click(move |_, window, cx| {
                        verify.update(cx, |editor, cx| editor.verify(window, cx))
                    }),
            )
        })
        .when(!dangers.is_empty(), |body| {
            body.child(crate::material::grouped(
                dangers
                    .into_iter()
                    .map(|action| {
                        h_flex()
                            .w_full()
                            .justify_center()
                            .py_1()
                            .child(action)
                            .into_any_element()
                    })
                    .collect(),
                cx,
            ))
        })
    }
}

/// Opens the dialog that connects or manages `host`.
pub(super) fn open(
    store: Entity<WorkspaceStore>,
    host: String,
    connect: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = cx.new(|cx| HostEditor::new(store, host, connect, false, window, cx));
    let focus = editor.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let content = editor.clone();
        dialog
            .sheet()
            .title(editor.read(cx).title(cx))
            .content(move |el, _, _| el.child(content.clone()))
            .footer(HostEditor::footer(&editor, cx))
    });
    // The dialog takes focus as it opens; connecting starts at the token.
    if connect {
        focus.update(cx, |editor, cx| editor.focus_token(window, cx));
    }
}
