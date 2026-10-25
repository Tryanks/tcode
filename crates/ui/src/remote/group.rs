//! Settings → Remote's machine group: machines that admit the devices any
//! member admits, so a device paired with one reaches all of them. The
//! state and its changes go through [`Spaces`], the machine's hosting-state
//! owner.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::{DeviceAccess, GroupAction, GroupInfo, GroupMachineInfo, HostingState};

use super::report;
use super::spaces::{Spaces, SpacesObserver};
use crate::icon::IconName;
use crate::material::{CARD_INSET, group_caption, group_row, notice, section_gap};
use crate::overlay::{DialogActions, DialogButtons, Notification, OverlayExt as _};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::ButtonVariant;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};

type Resolve = fn(&Entity<WorkspaceStore>, &mut App) -> Option<Entity<Spaces>>;
/// A dialog's confirmation; `false` keeps the dialog open.
type Submit = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

pub(crate) struct GroupSection {
    store: Entity<WorkspaceStore>,
    resolve: Resolve,
    observer: SpacesObserver,
}

impl GroupSection {
    /// `resolve` names the machine, as for [`super::spaces::SpacesSection`].
    pub(crate) fn new(store: Entity<WorkspaceStore>, resolve: Resolve) -> Self {
        Self {
            store,
            resolve,
            observer: SpacesObserver::default(),
        }
    }

    fn spaces(&self, cx: &mut App) -> Option<Entity<Spaces>> {
        (self.resolve)(&self.store, cx)
    }

    fn act(&mut self, action: GroupAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            let task = spaces.update(cx, |spaces, cx| spaces.act_group(action, cx));
            report("group.failed", task, window, cx);
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            open_create(spaces, window, cx);
        }
    }

    fn join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            open_join(spaces, window, cx);
        }
    }

    fn remove_machine(
        &mut self,
        machine: &GroupMachineInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        let id = machine.id.clone();
        let name = machine.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let spaces = spaces.clone();
            let id = id.clone();
            alert
                .title(crate::tr!("group.machines.remove_title", name = name.clone()).into_owned())
                .description(crate::tr!("group.machines.remove_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("group.machines.remove"))
                        .cancel_text(crate::tr!("spaces.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let task = spaces.update(cx, |spaces, cx| {
                        spaces.act_group(GroupAction::RemoveMachine(id.clone()), cx)
                    });
                    report("group.failed", task, window, cx);
                    true
                })
        });
    }

    fn leave(&mut self, group: &GroupInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        let name = group.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let spaces = spaces.clone();
            alert
                .title(crate::tr!("group.leave_title", name = name.clone()).into_owned())
                .description(crate::tr!("group.leave_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("group.leave"))
                        .cancel_text(crate::tr!("spaces.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let task =
                        spaces.update(cx, |spaces, cx| spaces.act_group(GroupAction::Leave, cx));
                    report("group.failed", task, window, cx);
                    true
                })
        });
    }

    fn note(text: SharedString, compact: bool, cx: &App) -> AnyElement {
        div()
            .text_secondary(compact, cx)
            .child(text)
            .into_any_element()
    }

    fn intro(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .p(px(CARD_INSET))
            .gap(cx.theme().tokens.spacing.md)
            .child(Self::note(
                crate::tr!("group.intro").into_owned().into(),
                compact,
                cx,
            ))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .text_color(cx.theme().warning_foreground)
                    .child(crate::tr!("group.risk")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("group-create")
                            .debug_selector(|| "group-create".into())
                            .ghost()
                            .outline()
                            .compact()
                            .icon(IconName::Plus)
                            .label(crate::tr!("group.create.action"))
                            .on_click(
                                cx.listener(|section, _, window, cx| section.create(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("group-join")
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("group.join.action"))
                            .on_click(
                                cx.listener(|section, _, window, cx| section.join(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn machines_block(
        &self,
        group: &GroupInfo,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = v_flex().w_full();
        for machine in &group.machines {
            let remove = machine.clone();
            rows = rows.child(
                group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(compact, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(machine.name.clone())),
                    )
                    .when(machine.this_machine, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_secondary(compact, cx)
                                .child(crate::tr!("group.machines.this")),
                        )
                    })
                    .when(!machine.this_machine, |row| {
                        row.child(
                            Button::new(SharedString::from(format!(
                                "group-machine-remove-{}",
                                machine.id
                            )))
                            .ghost()
                            .compact()
                            .danger()
                            .label(crate::tr!("group.machines.remove"))
                            .on_click(cx.listener(
                                move |section, _, window, cx| {
                                    section.remove_machine(&remove, window, cx);
                                },
                            )),
                        )
                    }),
            );
        }
        v_flex()
            .w_full()
            .child(group_caption(crate::tr!("group.machines.title"), cx))
            .child(rows)
            .into_any_element()
    }

    fn declined_block(
        &self,
        group: &GroupInfo,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if group.declined.is_empty() {
            return None;
        }
        let mut rows = v_flex().w_full();
        for device in &group.declined {
            let id = device.id.clone();
            rows = rows.child(
                group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(compact, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(super::device_label(
                                &device.name,
                                device.platform.as_deref(),
                            ))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("group-admit-{id}")))
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("group.declined.admit"))
                            .on_click(cx.listener(move |section, _, window, cx| {
                                section.act(GroupAction::Admit(id.clone()), window, cx);
                            })),
                    ),
            );
        }
        Some(
            v_flex()
                .w_full()
                .child(group_caption(crate::tr!("group.declined.title"), cx))
                .child(div().px(px(CARD_INSET)).child(Self::note(
                    crate::tr!("group.declined.description").into_owned().into(),
                    compact,
                    cx,
                )))
                .child(rows)
                .into_any_element(),
        )
    }

    /// The group's name, notes and actions sit at the card inset; the machine
    /// and device lists are [`group_row`]s, which carry that inset themselves.
    fn detail(&self, group: &GroupInfo, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let leave = group.clone();
        v_flex()
            .w_full()
            .py(px(CARD_INSET))
            .gap(section_gap(cx))
            .debug_selector(|| "group-detail".into())
            .child(
                v_flex()
                    .px(px(CARD_INSET))
                    .gap(cx.theme().tokens.spacing.xxs)
                    .child(
                        div()
                            .text_title(cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(group.name.clone())),
                    )
                    .child(Self::note(
                        if group.machines.len() == 1 {
                            crate::tr!("group.summary.machine_one")
                        } else {
                            crate::tr!("group.summary.machines", count = group.machines.len())
                        }
                        .into_owned()
                        .into(),
                        compact,
                        cx,
                    )),
            )
            .child(div().px(px(CARD_INSET)).child(Self::note(
                crate::tr!("group.detail_note").into_owned().into(),
                compact,
                cx,
            )))
            .child(self.machines_block(group, compact, cx))
            .children(self.declined_block(group, compact, cx))
            .child(
                h_flex().px(px(CARD_INSET)).child(
                    Button::new("group-leave")
                        .ghost()
                        .compact()
                        .danger()
                        .label(crate::tr!("group.leave"))
                        .on_click(cx.listener(move |section, _, window, cx| {
                            section.leave(&leave, window, cx);
                        })),
                ),
            )
            .into_any_element()
    }
}

impl gpui::Render for GroupSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let spaces = self.spaces(cx);
        self.observer.watch(spaces.as_ref(), cx);
        let Some(spaces) = spaces else {
            return div().into_any_element();
        };
        let group = spaces.read(cx).group().cloned();
        let body = match &group {
            Some(group) => self.detail(group, compact, cx),
            None => self.intro(compact, cx),
        };
        v_flex()
            .w_full()
            .debug_selector(|| "group-section".into())
            .child(group_caption(crate::tr!("group.section"), cx))
            .child(crate::material::group(cx).child(body))
            .into_any_element()
    }
}

/// What creating or joining brings: every full-access device paired with
/// this machine. Browsers logged in to a headless machine are listed among
/// its devices under a UUID; they sign in with the machine's password and
/// never belong to a group.
fn brought(spaces: &Entity<Spaces>, cx: &App) -> Vec<String> {
    spaces
        .read(cx)
        .devices()
        .iter()
        .filter(|device| {
            device.access == DeviceAccess::Full && tcode_client::pairing::valid_host_id(&device.id)
        })
        .map(|device| super::device_label(&device.name, device.platform.as_deref()))
        .collect()
}

/// The warning every create and join carries: the devices this machine
/// brings, which every member admits once confirmed.
fn brought_warning(devices: &[String], cx: &App) -> AnyElement {
    notice(cx.theme().warning, cx)
        .flex_col()
        .items_start()
        .text_body(false, cx)
        .debug_selector(|| "group-brought".into())
        .child(
            div()
                .text_caption(cx)
                .text_color(cx.theme().warning_foreground)
                .child(if devices.is_empty() {
                    crate::tr!("group.brought.none")
                } else {
                    crate::tr!("group.brought.title")
                }),
        )
        .children(
            devices
                .iter()
                .map(|label| div().child(format!("• {label}"))),
        )
        .into_any_element()
}

fn open_create(spaces: Entity<Spaces>, window: &mut Window, cx: &mut App) {
    let name = cx
        .new(|cx| InputState::new(window, cx).placeholder(crate::tr!("group.create.placeholder")));
    let brought = brought(&spaces, cx);
    let submit: Submit = Rc::new({
        let name = name.clone();
        move |window, cx| {
            let typed = name.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            let task = spaces.update(cx, |spaces, cx| {
                spaces.act_group(GroupAction::Create { name: typed }, cx)
            });
            report("group.failed", task, window, cx);
            true
        }
    });
    let field = name.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let field = field.clone();
        let brought = brought.clone();
        let ok = submit.clone();
        let create = submit.clone();
        dialog
            .title(crate::tr!("group.create.title").into_owned())
            .on_ok(move |_, window, cx| ok(window, cx))
            .content(move |content, _, cx| {
                content.child(
                    v_flex()
                        .gap_3()
                        .debug_selector(|| "group-create-dialog".into())
                        .child(Input::new(&field))
                        .child(brought_warning(&brought, cx)),
                )
            })
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("group-create-cancel")
                            .label(crate::tr!("spaces.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("group-create-ok")
                            .debug_selector(|| "group-create-ok".into())
                            .primary()
                            .label(crate::tr!("group.create.confirm"))
                            .on_click(move |_, window, cx| {
                                if create(window, cx) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
    name.update(cx, |name, cx| name.focus(window, cx));
}

fn open_join(spaces: Entity<Spaces>, window: &mut Window, cx: &mut App) {
    let link =
        cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!("group.join.placeholder")));
    let address = cx.new(|cx| {
        InputState::new(window, cx).placeholder(crate::tr!("group.join.address_placeholder"))
    });
    let brought = brought(&spaces, cx);
    let submit: Submit = Rc::new({
        let link = link.clone();
        let address = address.clone();
        move |window, cx| {
            let typed = link.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            if !tcode_client::pairing::parse_pair_url(&typed)
                .is_some_and(|invite| invite.group.is_some())
            {
                window.push_notification(
                    Notification::error(crate::tr!("group.join.not_a_group_link").into_owned()),
                    cx,
                );
                return false;
            }
            let address = address.read(cx).value().trim().to_owned();
            let action = GroupAction::Join {
                link: typed,
                address: (!address.is_empty()).then_some(address),
            };
            let task = spaces.update(cx, |spaces, cx| spaces.act_group(action, cx));
            window
                .spawn(cx, async move |cx| {
                    let result: Result<HostingState, String> = task.await;
                    let _ = cx.update(|window, cx| {
                        let notification = match result {
                            Ok(state) => Notification::info(
                                crate::tr!(
                                    "group.join.joined",
                                    name = state.group.map(|group| group.name).unwrap_or_default()
                                )
                                .into_owned(),
                            ),
                            Err(error) => Notification::error(
                                crate::tr!("group.failed", error = error).into_owned(),
                            ),
                        };
                        window.push_notification(notification, cx);
                    });
                })
                .detach();
            true
        }
    });
    let (link_field, address_field) = (link.clone(), address.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        let link_field = link_field.clone();
        let address_field = address_field.clone();
        let brought = brought.clone();
        let ok = submit.clone();
        let join = submit.clone();
        dialog
            .title(crate::tr!("group.join.title").into_owned())
            .on_ok(move |_, window, cx| ok(window, cx))
            .content(move |content, _, cx| {
                content.child(
                    v_flex()
                        .gap_3()
                        .debug_selector(|| "group-join-dialog".into())
                        .child(
                            div()
                                .text_secondary(false, cx)
                                .child(crate::tr!("group.join.description")),
                        )
                        .child(Input::new(&link_field))
                        .child(
                            div()
                                .text_caption(cx)
                                .child(crate::tr!("group.join.address")),
                        )
                        .child(Input::new(&address_field))
                        .child(brought_warning(&brought, cx)),
                )
            })
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("group-join-cancel")
                            .label(crate::tr!("spaces.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("group-join-ok")
                            .primary()
                            .label(crate::tr!("group.join.confirm"))
                            .on_click(move |_, window, cx| {
                                if join(window, cx) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
    link.update(cx, |link, cx| link.focus(window, cx));
}

/// Removing a group member removes it from every machine of the group, so
/// the device lists ask first.
pub(crate) fn confirm_group_removal(
    device_name: String,
    group_name: String,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .title(crate::tr!("group.remove_device.title", name = device_name.clone()).into_owned())
            .description(
                crate::tr!(
                    "group.remove_device.description",
                    group = group_name.clone()
                )
                .into_owned(),
            )
            .button_props(
                DialogButtons::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(crate::tr!("group.remove_device.action"))
                    .cancel_text(crate::tr!("spaces.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

/// Under a machine's invitation while the machine is in a group: pairing
/// with it is joining the group, for a device and for another machine.
pub(crate) fn invitation_note(group: &str, compact: bool, cx: &App) -> AnyElement {
    div()
        .text_secondary(compact, cx)
        .debug_selector(|| "group-invitation-note".into())
        .child(crate::tr!("group.invitation_note", name = group.to_owned()).into_owned())
        .into_any_element()
}
