//! Controls for the headless listener, carried on the authenticated host pipe.
use crate::{
    overlay::OverlayExt as _,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        switch::Switch,
    },
};
use gpui::{
    AnyElement, Context, Entity, IntoElement as _, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::{HostingAction, HostingState};

pub(crate) struct HostedPanel {
    store: Entity<WorkspaceStore>,
    state: Option<HostingState>,
    error: Option<String>,
    pending: bool,
    refreshing: bool,
    generation: u64,
    _ticker: Task<()>,
}

impl HostedPanel {
    pub fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if this
                    .update(cx, |panel, cx| panel.request(HostingAction::State, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            store,
            state: None,
            error: None,
            pending: false,
            refreshing: false,
            generation: 0,
            _ticker: ticker,
        }
    }

    fn request(&mut self, action: HostingAction, cx: &mut Context<Self>) {
        let refresh = matches!(action, HostingAction::State);
        if self.pending || (refresh && self.refreshing) {
            return;
        }
        if refresh {
            self.refreshing = true;
        } else {
            // Background polling must not disable the user's controls. A
            // mutation supersedes any older read which is still in flight.
            self.pending = true;
            self.generation = self.generation.wrapping_add(1);
            cx.notify();
        }
        let generation = self.generation;
        let task = self.store.update(cx, |store, cx| store.hosting(action, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |panel, cx| {
                if refresh {
                    panel.refreshing = false;
                } else {
                    panel.pending = false;
                }
                if generation != panel.generation {
                    return;
                }
                match result {
                    Ok(state) => {
                        panel.state = Some(state);
                        panel.error = None;
                    }
                    Err(error) => panel.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn body(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let mut column = v_flex().gap_3().w_full();
        if let Some(error) = &self.error {
            column = column.child(
                div()
                    .text_color(cx.theme().danger_foreground)
                    .child(error.clone()),
            );
        }
        let Some(state) = self.state.clone() else {
            return column.into_any_element();
        };
        column = column.child(
            crate::material::group(cx).child(
                h_flex().p_3().gap_3().child(
                    Switch::new("headless-pairing-enabled")
                        .w_full()
                        .flex_row_reverse()
                        .justify_between()
                        .checked(state.enabled)
                        .label(crate::tr!("remote.allow_devices"))
                        .disabled(self.pending)
                        .on_click(cx.listener(|panel, enabled: &bool, _, cx| {
                            panel.request(HostingAction::SetEnabled(*enabled), cx)
                        })),
                ),
            ),
        );
        if state.enabled {
            let mut invitation =
                v_flex()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .child(crate::tr!("remote.invite.title"))
                    .child(div().text_size(px(15.)).line_height(px(20.)).child(
                        match &state.invite {
                            Some(_) => crate::tr!("remote.invite.how"),
                            None => crate::tr!("remote.invite.expired"),
                        },
                    ))
                    .children(state.invite.as_ref().map(|_| {
                        crate::tr!(
                            "remote.invite.expires",
                            time = format!(
                                "{}:{:02}",
                                state.expires_in_secs / 60,
                                state.expires_in_secs % 60
                            )
                        )
                        .into_owned()
                    }));
            let mut actions = h_flex().gap_2();
            if let Some(link) = state.invite.clone() {
                actions = actions.child(
                    Button::new("headless-copy-invitation")
                        .disabled(self.pending)
                        .label(crate::tr!("remote.invite.copy"))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(link.clone()));
                            window.push_notification(
                                crate::overlay::Notification::info(
                                    crate::tr!("remote.invite.copied").into_owned(),
                                ),
                                cx,
                            );
                        })),
                );
            }
            invitation =
                invitation.child(
                    actions.child(
                        Button::new("headless-new-invitation")
                            .disabled(self.pending)
                            .label(crate::tr!("remote.invite.new"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.request(HostingAction::NewCode, cx)
                            })),
                    ),
                );
            let mut row = if crate::window_seam::window_is_compact(window, cx) {
                v_flex()
            } else {
                h_flex()
            };
            row = row.p_3().gap_4().items_start().child(invitation);
            if let Some(link) = &state.invite {
                row = row.children(super::qr::qr_element(link));
            }
            column = column.child(crate::material::group(cx).child(row));
        }
        column = column.child(
            div()
                .text_size(px(13.))
                .child(crate::tr!("remote.devices.section")),
        );
        let mut devices = crate::material::group(cx);
        if state.devices.is_empty() {
            devices = devices.child(div().p_3().child(crate::tr!("remote.devices.empty")));
        }
        for device in state.devices {
            let status_color = match &device.path {
                Some(_) => cx.theme().success,
                None => cx.theme().muted_foreground,
            };
            devices = devices.child(
                h_flex()
                    .p_3()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(super::device_label(
                        &device.name,
                        device.platform.as_deref(),
                    )))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .text_color(status_color)
                            .child(super::path_label(device.path.as_ref())),
                    )
                    .child(
                        Button::new(SharedString::from(format!("revoke-{}", device.id)))
                            .disabled(self.pending)
                            .danger()
                            .label(crate::tr!("remote.devices.revoke"))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.request(HostingAction::RevokeDevice(device.id.clone()), cx)
                            })),
                    ),
            );
        }
        column.child(devices).into_any_element()
    }
}
impl Render for HostedPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        self.body(window, cx)
    }
}
