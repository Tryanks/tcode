//! Settings → Remote's machine group: machines that admit the devices any
//! member admits, so a device paired with one reaches all of them. The
//! state and its changes go through [`Spaces`], the machine's hosting-state
//! owner.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Task, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_protocol::{DeviceAccess, GroupAction, GroupInfo, GroupMachineInfo, HostingState};

use super::spaces::{Spaces, SpacesObserver};
use crate::icon::IconName;
use crate::overlay::{DialogActions, DialogButtons, Notification, OverlayExt as _};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
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
            report(task, window, cx);
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
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let spaces = spaces.clone();
            let id = id.clone();
            alert
                .bg(cx.theme().popover)
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
                    report(task, window, cx);
                    true
                })
        });
    }

    fn leave(&mut self, group: &GroupInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        let name = group.name.clone();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let spaces = spaces.clone();
            alert
                .bg(cx.theme().popover)
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
                    report(task, window, cx);
                    true
                })
        });
    }

    fn note(text: SharedString, cx: &App) -> AnyElement {
        div()
            .text_size(px(13.))
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    fn subheading(label: SharedString, cx: &App) -> AnyElement {
        div()
            .text_size(px(13.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element()
    }

    fn intro(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .p_3()
            .gap_3()
            .child(Self::note(
                crate::tr!("group.intro").into_owned().into(),
                cx,
            ))
            .child(
                div()
                    .text_size(px(13.))
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

    fn machines_block(&self, group: &GroupInfo, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = v_flex().w_full();
        for machine in &group.machines {
            let remove = machine.clone();
            rows = rows.child(
                h_flex()
                    .w_full()
                    .min_h(px(36.))
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .truncate()
                            .child(machine.name.clone()),
                    )
                    .when(machine.this_machine, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
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
            .gap_1()
            .child(Self::subheading(
                crate::tr!("group.machines.title").into_owned().into(),
                cx,
            ))
            .child(rows)
            .into_any_element()
    }

    fn declined_block(&self, group: &GroupInfo, cx: &mut Context<Self>) -> Option<AnyElement> {
        if group.declined.is_empty() {
            return None;
        }
        let mut rows = v_flex().w_full();
        for device in &group.declined {
            let id = device.id.clone();
            rows = rows.child(
                h_flex()
                    .w_full()
                    .min_h(px(36.))
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .truncate()
                            .child(super::device_label(
                                &device.name,
                                device.platform.as_deref(),
                            )),
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
                .gap_1()
                .child(Self::subheading(
                    crate::tr!("group.declined.title").into_owned().into(),
                    cx,
                ))
                .child(Self::note(
                    crate::tr!("group.declined.description").into_owned().into(),
                    cx,
                ))
                .child(rows)
                .into_any_element(),
        )
    }

    fn detail(&self, group: &GroupInfo, cx: &mut Context<Self>) -> AnyElement {
        let leave = group.clone();
        v_flex()
            .w_full()
            .p_3()
            .gap_5()
            .debug_selector(|| "group-detail".into())
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_medium()
                            .truncate()
                            .child(group.name.clone()),
                    )
                    .child(Self::note(
                        if group.machines.len() == 1 {
                            crate::tr!("group.summary.machine_one")
                        } else {
                            crate::tr!("group.summary.machines", count = group.machines.len())
                        }
                        .into_owned()
                        .into(),
                        cx,
                    )),
            )
            .child(Self::note(
                crate::tr!("group.detail_note").into_owned().into(),
                cx,
            ))
            .child(self.machines_block(group, cx))
            .children(self.declined_block(group, cx))
            .child(
                h_flex().child(
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let spaces = self.spaces(cx);
        self.observer.watch(spaces.as_ref(), cx);
        let Some(spaces) = spaces else {
            return div().into_any_element();
        };
        let group = spaces.read(cx).group().cloned();
        let body = match &group {
            Some(group) => self.detail(group, cx),
            None => self.intro(cx),
        };
        v_flex()
            .w_full()
            .debug_selector(|| "group-section".into())
            .child(
                div()
                    .pl_3()
                    .pb(px(6.))
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("group.section")),
            )
            .child(crate::material::group(cx).child(body))
            .into_any_element()
    }
}

/// Tell the user why a change did not happen; a change that did shows itself.
fn report<T: 'static>(task: Task<Result<T, String>>, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                let _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::error(crate::tr!("group.failed", error = error).into_owned()),
                        cx,
                    );
                });
            }
        })
        .detach();
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
    v_flex()
        .w_full()
        .gap_2()
        .p_3()
        .rounded(crate::material::radius_chip(cx))
        .border_1()
        .border_color(cx.theme().warning.opacity(0.5))
        .bg(cx.theme().warning.opacity(0.08))
        .debug_selector(|| "group-brought".into())
        .child(
            div()
                .text_size(px(13.))
                .font_medium()
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
                .map(|label| div().text_size(px(13.)).child(format!("• {label}"))),
        )
        .into_any_element()
}

fn field_label(label: SharedString, cx: &App) -> AnyElement {
    div()
        .text_size(px(13.))
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(label)
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
            report(task, window, cx);
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
            .w(px(460.))
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
            .w(px(460.))
            .title(crate::tr!("group.join.title").into_owned())
            .on_ok(move |_, window, cx| ok(window, cx))
            .content(move |content, _, cx| {
                content.child(
                    v_flex()
                        .gap_3()
                        .debug_selector(|| "group-join-dialog".into())
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::tr!("group.join.description")),
                        )
                        .child(Input::new(&link_field))
                        .child(field_label(
                            crate::tr!("group.join.address").into_owned().into(),
                            cx,
                        ))
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
    window.open_alert_dialog(cx, move |alert, _, cx| {
        let on_ok = on_ok.clone();
        alert
            .bg(cx.theme().popover)
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
pub(crate) fn invitation_note(group: &str, cx: &App) -> AnyElement {
    div()
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .debug_selector(|| "group-invitation-note".into())
        .child(crate::tr!("group.invitation_note", name = group.to_owned()).into_owned())
        .into_any_element()
}
