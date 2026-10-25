//! Controls for the headless listener, carried on the authenticated host pipe.
use crate::{
    material::{empty_row, group_caption, group_row, group_row_stacked, section_gap},
    overlay::OverlayExt as _,
    settings_rows::ToggleRow,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::button::{Button, ButtonVariants as _},
};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement as _, ParentElement as _, Render,
    SharedString, Styled as _, Task, Window, div,
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
    spaces: Entity<super::spaces::SpacesSection>,
    group: Entity<super::group::GroupSection>,
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
            spaces: cx.new(|_| {
                super::spaces::SpacesSection::new(store.clone(), super::spaces::for_store)
            }),
            group: cx
                .new(|_| super::group::GroupSection::new(store.clone(), super::spaces::for_store)),
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
        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut column = v_flex().gap(section_gap(cx)).w_full();
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
                ToggleRow::new(
                    "headless-pairing-enabled",
                    crate::tr!("remote.pairing.title"),
                    state.enabled,
                    compact,
                    cx.listener(|panel, enabled: &bool, _, cx| {
                        panel.request(HostingAction::SetEnabled(*enabled), cx)
                    }),
                )
                .disabled(self.pending),
            ),
        );
        if state.enabled {
            let mut invitation = v_flex()
                .gap_2()
                .flex_1()
                .min_w_0()
                .child(crate::tr!("remote.invite.title"))
                .child(div().text_body(compact, cx).child(match &state.invite {
                    Some(_) => crate::tr!(
                        "remote.invite.expires",
                        time = crate::time::format_countdown(state.expires_in_secs)
                    ),
                    None => crate::tr!("remote.invite.expired"),
                }))
                .children(
                    state
                        .group
                        .as_ref()
                        .filter(|_| state.invite.is_some())
                        .map(|group| super::group::invitation_note(&group.name, compact, cx)),
                );
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
            invitation = invitation.child(
                actions.child(
                    Button::new("headless-new-invitation")
                        .disabled(self.pending)
                        .label(crate::tr!("remote.invite.new"))
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.request(HostingAction::NewInvitation, cx)
                        })),
                ),
            );
            let mut row = if compact {
                group_row_stacked(cx)
            } else {
                group_row(cx).items_start()
            }
            .child(invitation);
            if let Some(link) = &state.invite {
                row = row.children(super::qr::qr_element(link, compact, cx));
            }
            column = column.child(crate::material::group(cx).child(row));
        }
        let mut devices = crate::material::group(cx);
        if state.devices.is_empty() {
            devices = devices.child(empty_row(crate::tr!("remote.devices.empty"), cx));
        }
        let group_name = state.group.as_ref().map(|group| group.name.clone());
        for device in state.devices {
            // In a group every full-access device is a member; a browser is
            // listed under a UUID and never is.
            let in_group = group_name.clone().filter(|_| {
                device.access == tcode_protocol::DeviceAccess::Full
                    && tcode_client::pairing::valid_host_id(&device.id)
            });
            let space = match &device.access {
                tcode_protocol::DeviceAccess::Space { space_id } => state
                    .spaces
                    .iter()
                    .find(|space| &space.id == space_id)
                    .map(|space| space.name.clone()),
                tcode_protocol::DeviceAccess::Full => None,
            };
            let status_color = match &device.path {
                Some(_) => cx.theme().success,
                None => cx.theme().muted_foreground,
            };
            devices = devices.child(
                group_row(cx)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(div().min_w_0().child(super::device_label(
                                &device.name,
                                device.platform.as_deref(),
                            )))
                            .children(space.map(|name| super::spaces::member_badge(&name, cx))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_secondary(compact, cx)
                            .text_color(status_color)
                            .child(super::path_label(device.path.as_ref())),
                    )
                    .child(
                        Button::new(SharedString::from(format!("revoke-{}", device.id)))
                            .disabled(self.pending)
                            .danger()
                            .label(crate::tr!("remote.devices.revoke"))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let action = HostingAction::RevokeDevice(device.id.clone());
                                match in_group.clone() {
                                    Some(group) => {
                                        let panel = cx.entity().downgrade();
                                        super::group::confirm_group_removal(
                                            device.name.clone(),
                                            group,
                                            move |_, cx| {
                                                let _ = panel.update(cx, |panel, cx| {
                                                    panel.request(action.clone(), cx)
                                                });
                                            },
                                            window,
                                            cx,
                                        );
                                    }
                                    None => panel.request(action, cx),
                                }
                            })),
                    ),
            );
        }
        column
            .child(
                v_flex()
                    .child(group_caption(crate::tr!("remote.devices.section"), cx))
                    .child(devices),
            )
            .child(self.group.clone())
            .child(self.spaces.clone())
            .into_any_element()
    }
}
impl Render for HostedPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        self.body(window, cx)
    }
}
