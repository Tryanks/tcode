//! Settings → Remote's machine group: machines that admit the devices any
//! member admits, so a device paired with one reaches all of them. The
//! state and its changes go through [`Spaces`], the machine's hosting-state
//! owner.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, Styled as _, Task, Window, div, px,
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
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Input, InputState};
use crate::widgets::switch::Switch;

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

    fn link_block(
        &self,
        group: &GroupInfo,
        accepting: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let regenerate = |cx: &mut Context<Self>| {
            Button::new("group-regenerate")
                .compact()
                .label(crate::tr!("spaces.link.regenerate"))
                .on_click(cx.listener(|section, _, window, cx| {
                    section.act(GroupAction::RegenerateLink, window, cx);
                }))
        };
        let body = match &group.link {
            Some(link) => {
                let text = v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(Self::note(
                        crate::tr!("group.link.description").into_owned().into(),
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Button::new("group-copy")
                                    .ghost()
                                    .outline()
                                    .compact()
                                    .label(crate::tr!("spaces.link.copy"))
                                    .on_click({
                                        let link = link.clone();
                                        move |_, window, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                link.clone(),
                                            ));
                                            window.push_notification(
                                                Notification::info(
                                                    crate::tr!("group.link.copied").into_owned(),
                                                ),
                                                cx,
                                            );
                                        }
                                    }),
                            )
                            .child(regenerate(cx).ghost().outline()),
                    );
                super::qr::beside_qr(text, link, compact, cx)
                    .debug_selector(|| "group-link".into())
                    .into_any_element()
            }
            None => {
                let reason = if group.link_dead {
                    crate::tr!("group.link.dead")
                } else if !group.link_enabled {
                    crate::tr!("group.link.paused")
                } else if !accepting {
                    crate::tr!("group.link.pairing_off")
                } else {
                    crate::tr!("spaces.link.unavailable")
                };
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_center()
                    .flex_wrap()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.))
                            .text_size(px(13.))
                            .text_color(if group.link_dead {
                                cx.theme().danger_foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(reason),
                    )
                    .when(group.link_dead, |row| row.child(regenerate(cx).primary()))
                    .into_any_element()
            }
        };
        v_flex()
            .w_full()
            .gap_3()
            .child(body)
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .child(crate::tr!("group.link.enabled")),
                    )
                    .child(
                        Switch::new("group-link-enabled")
                            .checked(group.link_enabled)
                            .on_click(cx.listener(|section, enabled: &bool, window, cx| {
                                section.act(GroupAction::SetLinkEnabled(*enabled), window, cx);
                            })),
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

    fn detail(
        &self,
        group: &GroupInfo,
        accepting: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .child(self.link_block(group, accepting, compact, cx))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let spaces = self.spaces(cx);
        self.observer.watch(spaces.as_ref(), cx);
        let Some(spaces) = spaces else {
            return div().into_any_element();
        };
        let compact = crate::window_seam::window_is_compact(window, cx);
        let model = spaces.read(cx);
        let group = model.group().cloned();
        let accepting = model.accepting();
        let body = match &group {
            Some(group) => self.detail(group, accepting, compact, cx),
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

/// This machine's directly paired full-access devices, which a group it
/// creates or joins brings in unless they are unchecked.
type Brought = Rc<RefCell<Vec<(String, String, bool)>>>;

/// Browsers logged in to a headless machine are listed among its devices
/// under a UUID; they sign in with the machine's password and never belong
/// to a group, so only devices with a native device key are offered.
fn brought(spaces: &Entity<Spaces>, cx: &App) -> Brought {
    Rc::new(RefCell::new(
        spaces
            .read(cx)
            .devices()
            .iter()
            .filter(|device| {
                device.access == DeviceAccess::Full
                    && !device.group
                    && tcode_client::pairing::valid_host_id(&device.id)
            })
            .map(|device| {
                (
                    device.id.clone(),
                    super::device_label(&device.name, device.platform.as_deref()),
                    true,
                )
            })
            .collect(),
    ))
}

fn checked(brought: &Brought) -> Vec<String> {
    brought
        .borrow()
        .iter()
        .filter(|(_, _, checked)| *checked)
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// The warning every create and join carries: the devices this machine
/// brings, each one checked, admitted by every member once confirmed.
fn brought_warning(brought: &Brought, cx: &App) -> AnyElement {
    let devices = brought.borrow().clone();
    let mut column = v_flex()
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
        );
    if !devices.is_empty() {
        column = column.child(
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("group.brought.description")),
        );
    }
    for (index, (id, label, checked)) in devices.into_iter().enumerate() {
        let toggle = brought.clone();
        let selector = format!("group-brought-{id}");
        column = column.child(
            div().debug_selector(move || selector.clone()).child(
                Checkbox::new(SharedString::from(format!("group-brought-{id}")))
                    .label(label)
                    .checked(checked)
                    .on_click(move |checked: &bool, window, _| {
                        if let Some(entry) = toggle.borrow_mut().get_mut(index) {
                            entry.2 = *checked;
                        }
                        window.refresh();
                    }),
            ),
        );
    }
    column.into_any_element()
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
        let brought = brought.clone();
        move |window, cx| {
            let typed = name.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            let devices = checked(&brought);
            let task = spaces.update(cx, |spaces, cx| {
                spaces.act_group(
                    GroupAction::Create {
                        name: typed,
                        devices,
                    },
                    cx,
                )
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
        let brought = brought.clone();
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
                devices: checked(&brought),
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

/// The mark on a device that is a member of this machine's group.
pub(crate) fn member_badge(cx: &App) -> AnyElement {
    h_flex()
        .flex_none()
        .gap_1()
        .px_2()
        .py(px(1.))
        .items_center()
        .rounded(crate::material::radius_chip(cx))
        .bg(cx.theme().muted)
        .text_size(px(11.))
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(crate::tr!("group.member_badge"))
        .into_any_element()
}

#[cfg(all(test, feature = "remote-hosting"))]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, Render, TestAppContext, VisualTestContext};
    use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, TraverseHost};

    struct Probe(Entity<GroupSection>);

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex().size_full().child(self.0.clone())
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
        }
    }

    fn click(selector: String, cx: &mut VisualTestContext) {
        let selector: &'static str = selector.leak();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        draw(cx);
    }

    /// Creating a group brings this machine's directly paired devices into
    /// it, every one checked as the warning opens; a device unchecked there
    /// stays on this machine alone.
    #[gpui::test]
    fn an_unchecked_device_stays_out_of_a_created_group(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("group-create");
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        let mux = HostMux::new(attachments, from_host.clone());
        let host = TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: "Studio".into(),
                data_dir: root.join("machine"),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        let mut ids = Vec::new();
        for name in ["Phone", "Tablet"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let device = DeviceIdentity::load_or_create(&dir).unwrap();
            device.set_details(name.into(), None);
            let invite = host.new_invitation().invite;
            tcode_traverse::pair_blocking(
                &invite,
                Some(std::net::Ipv4Addr::LOCALHOST.into()),
                &device,
            )
            .unwrap();
            ids.push(device.endpoint_id().to_string());
        }
        cx.update(|cx| {
            let mut controller = crate::remote::RemoteController::new(
                mux,
                root.join("machine"),
                tcode_client::HostLink::new(to_host.clone(), from_host.clone()),
                Default::default(),
            );
            controller.adopt_host(host);
            cx.set_global(controller);
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    tcode_client::HostLink::new(to_host, from_host),
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            let section = cx.new(|_| GroupSection::new(store, super::super::spaces::this_machine));
            let probe = cx.new(|_| Probe(section));
            gpui_base::Root::new(probe, window, cx)
        });
        cx.simulate_resize(gpui::size(px(900.), px(1400.)));
        draw(cx);

        click("group-create".into(), cx);
        cx.simulate_input("Home");
        click(format!("group-brought-{}", ids[1]), cx);
        click("group-create-ok".into(), cx);

        let state = cx.read(|cx| {
            cx.global::<crate::remote::RemoteController>()
                .hosting(tcode_protocol::HostingAction::State)
                .unwrap()
        });
        assert_eq!(
            state.group.as_ref().map(|group| group.name.as_str()),
            Some("Home")
        );
        let in_group = |id: &str| {
            state
                .devices
                .iter()
                .find(|device| device.id == id)
                .is_some_and(|device| device.group)
        };
        assert!(in_group(&ids[0]), "the checked device is in the group");
        assert!(!in_group(&ids[1]), "the unchecked device is not");
        cx.update(|_, cx| {
            cx.update_global::<crate::remote::RemoteController, _>(|controller, _| {
                controller.stop_hosting()
            });
        });
        let _ = std::fs::remove_dir_all(root);
    }
}
