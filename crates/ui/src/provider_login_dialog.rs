//! Sign in to a profile's native account. The host runs the provider CLI's
//! own sign-in; this dialog shows where it stands and hands it the code the
//! sign-in page shows. The page opens on the device showing the dialog, which
//! need not be the host.

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use agent::ProviderKind;
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::provider_status::{AuthStatus, ProviderLogin};

pub(crate) struct ProviderLoginDialog {
    store: Entity<WorkspaceStore>,
    provider: ProviderKind,
    profile_id: String,
    code: Entity<InputState>,
    /// Whether the host has reported this sign-in; before it has, a snapshot
    /// without one means the sign-in has not started rather than ended.
    seen_login: bool,
    _subscriptions: [Subscription; 2],
}

/// Start the profile's sign-in on the host and show it.
pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    provider: ProviderKind,
    profile_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    store.update(cx, |store, _| {
        store.start_provider_login(profile_id.clone())
    });
    show(store, provider, profile_id, window, cx);
}

/// Show a sign-in the host started on its own.
pub(crate) fn show(
    store: Entity<WorkspaceStore>,
    provider: ProviderKind,
    profile_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let dialog = cx.new(|cx| {
        ProviderLoginDialog::new(store.clone(), provider, profile_id.clone(), window, cx)
    });
    window.open_dialog(cx, move |builder, _, cx| {
        let content = dialog.clone();
        let cancel = dialog.clone();
        let name = store.read(cx).provider_profile_display_name(&profile_id);
        builder
            .sheet()
            .title(crate::tr!("providers.login.title", name = name).into_owned())
            .content(move |body, _, _| body.child(content.clone()))
            .on_cancel(move |_, _, cx| {
                cancel.update(cx, |dialog, cx| dialog.cancel(cx));
                true
            })
    });
}

impl ProviderLoginDialog {
    fn new(
        store: Entity<WorkspaceStore>,
        provider: ProviderKind,
        profile_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let code = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("providers.login.code_hint"))
        });
        let subscriptions = [
            cx.subscribe_in(
                &store,
                window,
                |this, _, change: &StoreChange, window, cx| {
                    if change.topic == TopicKind::Providers {
                        this.follow_host(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&code, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.submit(window, cx);
                }
            }),
        ];
        Self {
            store,
            provider,
            profile_id,
            code,
            seen_login: false,
            _subscriptions: subscriptions,
        }
    }

    fn follow_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self
            .store
            .read(cx)
            .provider_profile_snapshot(&self.profile_id)
            .unwrap_or_default();
        if snapshot.login.is_some() {
            self.seen_login = true;
        } else if self.seen_login
            && snapshot
                .auth
                .is_some_and(|auth| auth.status == AuthStatus::Authenticated)
        {
            window.close_dialog(cx);
            return;
        }
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let code = self.code.read(cx).value().trim().to_owned();
        if code.is_empty() {
            return;
        }
        self.code
            .update(cx, |input, cx| input.set_value("", window, cx));
        let profile_id = self.profile_id.clone();
        self.store.update(cx, |store, _| {
            store.submit_provider_login_code(profile_id, code)
        });
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let profile_id = self.profile_id.clone();
        self.store
            .update(cx, |store, _| store.start_provider_login(profile_id));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let profile_id = self.profile_id.clone();
        self.store
            .update(cx, |store, _| store.cancel_provider_login(profile_id));
    }

    /// The host's sign-in state, or, once the sign-in ended without signing
    /// in, a failure with nothing more to say.
    fn shown(&self, cx: &App) -> Shown {
        let Some(snapshot) = self
            .store
            .read(cx)
            .provider_profile_snapshot(&self.profile_id)
        else {
            return Shown::Waiting;
        };
        match snapshot.login {
            Some(ProviderLogin::AwaitingCode { url }) => Shown::Code(url),
            Some(ProviderLogin::AwaitingDevice { url, code }) => Shown::Device(url, code),
            Some(ProviderLogin::Failed { message }) => Shown::Failed(Some(message)),
            Some(ProviderLogin::Starting | ProviderLogin::Finishing) => Shown::Waiting,
            None if self.seen_login
                && !snapshot.checking
                && !snapshot
                    .auth
                    .is_some_and(|auth| auth.status == AuthStatus::Authenticated) =>
            {
                Shown::Failed(None)
            }
            None => Shown::Waiting,
        }
    }
}

enum Shown {
    Waiting,
    Code(Option<String>),
    /// The one-time code to enter on the sign-in page.
    Device(Option<String>, String),
    Failed(Option<String>),
}

impl Render for ProviderLoginDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut actions = DialogActions::new().child(
            Button::new("provider-login-cancel")
                .ghost()
                .label(crate::tr!("settings.cancel"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.cancel(cx);
                    window.close_dialog(cx);
                })),
        );
        let body = match self.shown(cx) {
            Shown::Waiting => v_flex().child(
                div().text_secondary(false, cx).child(
                    crate::tr!(
                        "providers.login.waiting",
                        name = self.provider.display_name()
                    )
                    .into_owned(),
                ),
            ),
            Shown::Code(url) => {
                actions = actions.child(
                    Button::new("provider-login-continue")
                        .primary()
                        .label(crate::tr!("providers.login.continue"))
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                );
                v_flex()
                    .gap_4()
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.login.instructions")),
                    )
                    .when_some(url, |body, url| {
                        let copy = url.clone();
                        body.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("provider-login-open")
                                        .label(crate::tr!("providers.login.open_browser"))
                                        .on_click(move |_, _, cx| cx.open_url(&url)),
                                )
                                .child(
                                    Button::new("provider-login-copy")
                                        .ghost()
                                        .label(crate::tr!("providers.login.copy_link"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ))
                                        }),
                                ),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_caption(cx)
                                    .child(crate::tr!("providers.login.code")),
                            )
                            .child(Input::new(&self.code)),
                    )
            }
            Shown::Device(url, code) => {
                let copy = code.clone();
                v_flex()
                    .gap_4()
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.login.device_instructions")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .debug_selector(|| "provider-login-device-code".into())
                                    .text_title(cx)
                                    .text_code(cx)
                                    .child(code),
                            )
                            .child(
                                Button::new("provider-login-copy-code")
                                    .ghost()
                                    .label(crate::tr!("providers.login.copy_code"))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy.clone(),
                                        ))
                                    }),
                            ),
                    )
                    .when_some(url, |body, url| {
                        body.child(
                            Button::new("provider-login-open")
                                .label(crate::tr!("providers.login.open_browser"))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    })
            }
            Shown::Failed(message) => {
                actions = actions.child(
                    Button::new("provider-login-retry")
                        .primary()
                        .label(crate::tr!("providers.login.retry"))
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                );
                v_flex().when_some(message, |body, message| {
                    body.child(
                        div()
                            .text_secondary(false, cx)
                            .text_color(cx.theme().danger)
                            .child(
                                crate::tr!("providers.login.failed", message = message)
                                    .into_owned(),
                            ),
                    )
                })
            }
        };
        v_flex().gap_4().child(body).child(actions)
    }
}
