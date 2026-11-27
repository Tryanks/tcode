//! Settings → Machine group: machines that admit the devices any
//! member admits, so a device paired with one reaches all of them. The
//! state and its changes go through [`Spaces`], the machine's hosting-state
//! owner.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_base::v_flex;
use tcode_protocol::{
    DeviceAccess, GroupAction, GroupDeviceAccess, GroupInfo, GroupMachineInfo, HostingState,
};

use super::spaces::{Resolve, Spaces, SpacesObserver};
use super::{Submit, report, submit_actions};
use crate::icon::{Icon, IconName};
use crate::material::{empty_row, group_row, notice, section_gap};
use crate::overlay::{Notification, OverlayExt as _};
use crate::settings_rows::{SelectRowOption, ToggleRow, footnote, row_frame, settings_group};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) struct GroupSection {
    store: Entity<WorkspaceStore>,
    resolve: Resolve,
    observer: SpacesObserver,
    /// The section's size-class motion: the window draws it under another
    /// parent when it turns compact or wide.
    motion: crate::motion::owned::Owned,
}

/// How much of this machine the group's other devices reach.
fn reach(access: GroupDeviceAccess) -> u8 {
    match access {
        GroupDeviceAccess::Operate => 2,
        GroupDeviceAccess::View => 1,
        GroupDeviceAccess::None => 0,
    }
}

fn access_key(access: GroupDeviceAccess) -> &'static str {
    match access {
        GroupDeviceAccess::Operate => "machines.access.operate",
        GroupDeviceAccess::View => "machines.access.view",
        GroupDeviceAccess::None => "machines.access.none",
    }
}

fn access_description_key(access: GroupDeviceAccess) -> &'static str {
    match access {
        GroupDeviceAccess::Operate => "machines.access.operate_description",
        GroupDeviceAccess::View => "machines.access.view_description",
        GroupDeviceAccess::None => "machines.access.none_description",
    }
}

/// What another machine of the group says it takes.
fn accepts_line(machine: &GroupMachineInfo) -> SharedString {
    match (machine.accepts_threads, machine.accepts_projects) {
        (Some(true), Some(true)) => crate::tr!("group.machines.accepts.both"),
        (Some(true), Some(false)) => crate::tr!("group.machines.accepts.threads"),
        (Some(false), Some(true)) => crate::tr!("group.machines.accepts.projects"),
        (Some(false), Some(false)) => crate::tr!("group.machines.accepts.neither"),
        _ => crate::tr!("group.machines.accepts.unknown"),
    }
    .into_owned()
    .into()
}

impl GroupSection {
    /// `resolve` names the machine, as for [`super::spaces::SpacesSection`].
    pub(crate) fn new(store: Entity<WorkspaceStore>, resolve: Resolve) -> Self {
        Self {
            store,
            resolve,
            observer: SpacesObserver::default(),
            motion: Default::default(),
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
        super::confirm_destructive(
            crate::tr!("group.machines.remove_title", name = machine.title()).into_owned(),
            crate::tr!("group.machines.remove_description").into_owned(),
            crate::tr!("group.machines.remove").into_owned(),
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| {
                    spaces.act_group(GroupAction::RemoveMachine(id.clone()), cx)
                });
                report("group.failed", task, window, cx);
            },
            window,
            cx,
        );
    }

    fn rename(&mut self, machine: &GroupMachineInfo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            open_label(
                spaces,
                &machine.id,
                &machine.name,
                machine.label.as_deref(),
                window,
                cx,
            );
        }
    }

    /// Narrowing what the group's other devices may do disconnects them, so
    /// it asks first; widening it does not.
    fn set_access(
        &mut self,
        current: GroupDeviceAccess,
        next: GroupDeviceAccess,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if next == current {
            return;
        }
        if reach(next) > reach(current) {
            self.act(GroupAction::SetDeviceAccess(next), window, cx);
            return;
        }
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        let (title, description) = match next {
            GroupDeviceAccess::View => (
                "machines.access.confirm_view_title",
                "machines.access.confirm_view",
            ),
            _ => (
                "machines.access.confirm_none_title",
                "machines.access.confirm_none",
            ),
        };
        super::confirm_destructive(
            crate::tr!(title).into_owned(),
            crate::tr!(description).into_owned(),
            crate::tr!(access_key(next)).into_owned(),
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| {
                    spaces.act_group(GroupAction::SetDeviceAccess(next), cx)
                });
                report("group.failed", task, window, cx);
            },
            window,
            cx,
        );
    }

    /// This machine's switches: what it takes from the group's other
    /// machines, and what the group's other devices may do on it. Below
    /// operate the machine takes nothing from the others whatever the
    /// switches hold, so they show off and do not move.
    fn switch_rows(
        &self,
        group: &GroupInfo,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let operate = group.device_access == GroupDeviceAccess::Operate;
        let accept = |id: &'static str,
                      title: &'static str,
                      description: &'static str,
                      checked: bool,
                      action: fn(bool) -> GroupAction| {
            ToggleRow::new(
                id,
                crate::tr!(title),
                operate && checked,
                compact,
                cx.listener(move |section, checked: &bool, window, cx| {
                    section.act(action(*checked), window, cx);
                }),
            )
            .description(if operate {
                crate::tr!(description)
            } else {
                crate::tr!("machines.access.follows")
            })
            .disabled(!operate)
            .into_any_element()
        };
        let threads = accept(
            "group-accept-threads",
            "group.accept_threads.title",
            "group.accept_threads.description",
            group.accept_threads,
            GroupAction::SetAcceptThreads,
        );
        let projects = accept(
            "group-accept-projects",
            "group.accept_projects.title",
            "group.accept_projects.description",
            group.accept_projects,
            GroupAction::SetAcceptProjects,
        );
        let current = group.device_access;
        let options = [
            GroupDeviceAccess::Operate,
            GroupDeviceAccess::View,
            GroupDeviceAccess::None,
        ]
        .into_iter()
        .map(|access| SelectRowOption {
            value: access,
            id: SharedString::from(match access {
                GroupDeviceAccess::Operate => "group-device-access-operate",
                GroupDeviceAccess::View => "group-device-access-view",
                GroupDeviceAccess::None => "group-device-access-none",
            }),
            label: crate::tr!(access_key(access)).into_owned().into(),
            description: None,
            selected: access == current,
        })
        .collect();
        let section = cx.entity().downgrade();
        let access = crate::settings_rows::select_row(
            "group-device-access",
            "group-device-access-popover",
            "group-device-access-menu",
            200.,
            crate::tr!("machines.access.title").into_owned().into(),
            Some(
                crate::tr!(access_description_key(current))
                    .into_owned()
                    .into(),
            ),
            crate::tr!(access_key(current)).into_owned().into(),
            options,
            None,
            crate::settings_rows::flip_row(&self.motion, "group-device-access", compact),
            move |next, window, cx| {
                let _ = section.update(cx, |section, cx| {
                    section.set_access(current, next, window, cx);
                });
            },
            cx,
        );
        vec![threads, projects, access]
    }

    fn leave(&mut self, group: &GroupInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        super::confirm_destructive(
            crate::tr!("group.leave_title", name = group.name.clone()).into_owned(),
            crate::tr!("group.leave_description").into_owned(),
            crate::tr!("group.leave").into_owned(),
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| spaces.act_group(GroupAction::Leave, cx));
                report("group.failed", task, window, cx);
            },
            window,
            cx,
        );
    }

    /// Not in a group: what one risks, the two ways in, and what one is.
    fn intro(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let risk = notice(cx.theme().warning, cx)
            .text_body(compact, cx)
            .debug_selector(|| "group-risk".into())
            .child(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning_foreground))
            .child(div().flex_1().min_w_0().child(crate::tr!("group.risk")))
            .into_any_element();
        let actions = group_row(cx)
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("group-create")
                    .debug_selector(|| "group-create".into())
                    .ghost()
                    .outline()
                    .compact()
                    .icon(IconName::Plus)
                    .label(crate::tr!("group.create.action"))
                    .on_click(cx.listener(|section, _, window, cx| section.create(window, cx))),
            )
            .child(
                Button::new("group-join")
                    .debug_selector(|| "group-join".into())
                    .ghost()
                    .outline()
                    .compact()
                    .label(crate::tr!("group.join.action"))
                    .on_click(cx.listener(|section, _, window, cx| section.join(window, cx))),
            )
            .into_any_element();
        vec![
            risk,
            settings_group(crate::tr!("group.section"), vec![actions], cx)
                .child(super::footnote_with_docs(
                    crate::tr!("group.intro"),
                    "machine-groups",
                    compact,
                    cx,
                ))
                .into_any_element(),
        ]
    }

    fn machines_group(
        &self,
        group: &GroupInfo,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A machine that joins folds in; one removed folds away and the ones
        // below move up.
        let machines = group
            .machines
            .iter()
            .map(|machine| (SharedString::from(machine.id.clone()), machine.clone()));
        let rows =
            crate::motion::keyed(
                "group-machines",
                machines,
                ROWS,
                window,
                cx,
                |_, machine, _, cx| {
                    let remove = machine.clone();
                    let rename = machine.clone();
                    let secondary = |text: SharedString| {
                        div()
                            .text_secondary(compact, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(text))
                    };
                    let id = machine.id.clone();
                    group_row(cx)
                        .debug_selector(move || format!("group-machine-{id}"))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(cx.theme().tokens.spacing.xxs)
                                .child(
                                    div().text_body(compact, cx).overflow_hidden().child(
                                        crate::widgets::fade_text(machine.title().to_owned()),
                                    ),
                                )
                                .when(machine.label.is_some(), |column| {
                                    column.child(secondary(
                                        crate::tr!(
                                            "machines.label.machine_name",
                                            name = machine.name.clone()
                                        )
                                        .into_owned()
                                        .into(),
                                    ))
                                })
                                .when(!machine.this_machine, |column| {
                                    column.child(secondary(accepts_line(machine)))
                                }),
                        )
                        .when(machine.this_machine, |row| {
                            row.child(div().flex_none().text_secondary(compact, cx).child(
                                crate::widgets::fade_text(crate::tr!("group.machines.this")),
                            ))
                        })
                        .child(
                            Button::new(SharedString::from(format!(
                                "group-machine-rename-{}",
                                machine.id
                            )))
                            .debug_selector({
                                let id = machine.id.clone();
                                move || format!("group-machine-rename-{id}")
                            })
                            .ghost()
                            .compact()
                            .label(crate::tr!("machines.label.rename"))
                            .on_click(cx.listener(
                                move |section, _, window, cx| {
                                    section.rename(&rename, window, cx);
                                },
                            )),
                        )
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
                        })
                        .into_any_element()
                },
            );
        settings_group(
            crate::tr!("group.machines.section"),
            vec![rows.render(v_flex().w_full())],
            cx,
        )
        .into_any_element()
    }

    /// The devices waiting for this machine, folding in as the first
    /// arrives and out once the last is admitted; `gap` sets it off from
    /// the group above.
    fn declined_group(
        &self,
        group: &GroupInfo,
        compact: bool,
        gap: gpui::Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let shown = crate::motion::shown(
            "group-declined",
            (!group.declined.is_empty()).then(|| group.declined.clone()),
            SHOW,
            window,
            cx,
        )?;
        // The list leaving is drawn from the last devices it had.
        let devices = shown
            .value
            .iter()
            .map(|device| (SharedString::from(device.id.clone()), device.clone()));
        let rows = crate::motion::keyed(
            "group-declined-rows",
            devices,
            ROWS,
            window,
            cx,
            |_, device, _, cx| {
                let id = device.id.clone();
                row_frame(compact, cx)
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
                    )
                    .into_any_element()
            },
        );
        Some(
            shown.fold(
                div().pt(gap).child(
                    settings_group(
                        crate::tr!("group.declined.section"),
                        vec![rows.render(v_flex().w_full())],
                        cx,
                    )
                    .child(footnote(
                        crate::tr!("group.declined.description"),
                        compact,
                        cx,
                    )),
                ),
            ),
        )
    }

    /// In a group: its name and size, its machines, the devices waiting for
    /// this machine, and the way out.
    fn detail(
        &self,
        group: &GroupInfo,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = if group.machines.len() == 1 {
            crate::tr!("group.summary.machine_one")
        } else {
            crate::tr!("group.summary.machines", count = group.machines.len())
        };
        let name = row_frame(compact, cx)
            .debug_selector(|| "group-detail".into())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_body(compact, cx)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(group.name.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .text_secondary(compact, cx)
                    .child(crate::widgets::fade_text(count)),
            )
            .into_any_element();
        let leave = group.clone();
        let (fill, on_hover) = crate::motion::hover_fill(
            crate::motion::Clock::Element,
            "group-leave",
            cx.theme().list_hover.opacity(0.),
            cx.theme().list_hover,
            &crate::material::HOVER.into(),
            window,
            cx,
        );
        let leave_row = crate::material::accessible_clickable(
            group_row(cx),
            "group-leave",
            gpui::Role::Button,
            crate::tr!("group.leave"),
            cx,
        )
        .debug_selector(|| "group-leave".into())
        .cursor_pointer()
        .bg(fill)
        .on_hover(on_hover)
        .text_body(compact, cx)
        .text_color(cx.theme().danger_foreground)
        .child(crate::widgets::fade_text(crate::tr!("group.leave")))
        .on_click(cx.listener(move |section, _, window, cx| {
            section.leave(&leave, window, cx);
        }))
        .into_any_element();
        // Each part carries the gap above it, so the declined devices fold
        // away with theirs.
        let gap = section_gap(cx);
        v_flex()
            .w_full()
            .child(
                settings_group(
                    crate::tr!("group.section"),
                    std::iter::once(name)
                        .chain(self.switch_rows(group, compact, cx))
                        .collect(),
                    cx,
                )
                .child(super::footnote_with_docs(
                    crate::tr!("group.detail_note"),
                    "machine-groups",
                    compact,
                    cx,
                )),
            )
            .child(
                div()
                    .pt(gap)
                    .child(self.machines_group(group, compact, window, cx)),
            )
            .children(self.declined_group(group, compact, gap, window, cx))
            .child(
                div()
                    .pt(gap)
                    .child(crate::material::group(cx).child(leave_row)),
            )
            .into_any_element()
    }
}

impl gpui::Render for GroupSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let spaces = self.spaces(cx);
        self.observer.watch(spaces.as_ref(), cx);
        // Until the machine answers, it is neither in a group nor out of one.
        let state = spaces.map(|spaces| {
            let spaces = spaces.read(cx);
            spaces.loaded().then(|| spaces.group().cloned())
        });
        // Creating, joining or leaving a group replaces the whole page: the
        // state that goes folds into the one that comes.
        let off =
            crate::motion::shown("group-off", state.is_none().then_some(()), SHOW, window, cx).map(
                |shown| {
                    shown.fold(settings_group(
                        crate::tr!("group.section"),
                        vec![empty_row(crate::tr!("remote.hosting_off"), cx).into_any_element()],
                        cx,
                    ))
                },
            );
        let detail = crate::motion::shown(
            "group-detail",
            state.clone().flatten().flatten(),
            SHOW,
            window,
            cx,
        )
        .map(|shown| shown.fold(self.detail(&shown.value, compact, window, cx)));
        let intro = crate::motion::shown(
            "group-intro",
            matches!(state, Some(Some(None))).then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold(
                v_flex()
                    .w_full()
                    .gap(section_gap(cx))
                    .children(self.intro(compact, cx)),
            )
        });
        v_flex()
            .w_full()
            .debug_selector(|| "group-section".into())
            .children(off)
            .children(detail)
            .children(intro)
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
fn brought_warning(devices: &[String], compact: bool, cx: &App) -> AnyElement {
    notice(cx.theme().warning, cx)
        .flex_col()
        .items_start()
        .text_body(compact, cx)
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
        submit_actions(
            dialog
                .title(crate::tr!("group.create.title").into_owned())
                .content(move |content, window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "group-create-dialog".into())
                            .child(Input::new(&field))
                            .child(brought_warning(&brought, compact, cx)),
                    )
                }),
            "group-create",
            crate::tr!("group.create.confirm").into_owned().into(),
            submit.clone(),
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
        submit_actions(
            dialog
                .title(crate::tr!("group.join.title").into_owned())
                .content(move |content, window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "group-join-dialog".into())
                            .child(
                                div()
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!("group.join.description")),
                            )
                            .child(Input::new(&link_field))
                            .child(
                                div().text_caption(cx).child(crate::widgets::fade_text(
                                    crate::tr!("group.join.address"),
                                )),
                            )
                            .child(Input::new(&address_field))
                            .child(brought_warning(&brought, compact, cx)),
                    )
                }),
            "group-join",
            crate::tr!("group.join.confirm").into_owned().into(),
            submit.clone(),
        )
    });
    link.update(cx, |link, cx| link.focus(window, cx));
}

/// Give `machine` the label the whole group shows for it, through
/// `spaces`, a machine of that group; an empty field clears it.
pub(crate) fn open_label(
    spaces: Entity<Spaces>,
    id: &str,
    name: &str,
    label: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    let field = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(name.to_owned())
            .default_value(label.unwrap_or_default().to_owned())
    });
    let id = id.to_owned();
    let submit: Submit = Rc::new({
        let field = field.clone();
        move |window, cx| {
            let typed = field.read(cx).value().trim().to_owned();
            let action = GroupAction::SetLabel {
                machine: id.clone(),
                label: (!typed.is_empty()).then_some(typed),
            };
            let task = spaces.update(cx, |spaces, cx| spaces.act_group(action, cx));
            report("group.failed", task, window, cx);
            true
        }
    });
    let title = crate::tr!("machines.label.title", name = label.unwrap_or(name)).into_owned();
    let input = field.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let input = input.clone();
        submit_actions(
            dialog
                .title(title.clone())
                .content(move |content, window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "group-label-dialog".into())
                            .child(Input::new(&input))
                            .child(
                                div()
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!("machines.label.note")),
                            ),
                    )
                }),
            "group-label",
            crate::tr!("machines.label.save").into_owned().into(),
            submit.clone(),
        )
    });
    field.update(cx, |field, cx| field.focus(window, cx));
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
    super::confirm_destructive(
        crate::tr!("group.remove_device.title", name = device_name).into_owned(),
        crate::tr!("group.remove_device.description", group = group_name).into_owned(),
        crate::tr!("group.remove_device.action").into_owned(),
        on_ok,
        window,
        cx,
    );
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

#[cfg(all(test, feature = "remote-hosting"))]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, Render, TestAppContext, VisualTestContext, px};
    use tcode_protocol::HostingAction;
    use tcode_traverse::HostMux;

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

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        draw(cx);
    }

    fn group(cx: &mut VisualTestContext) -> GroupInfo {
        cx.read(|cx| {
            cx.global::<super::super::RemoteController>()
                .hosting(HostingAction::State)
                .unwrap()
                .group
                .unwrap()
        })
    }

    /// Choose `option` in the devices' access row, confirming a narrowing.
    fn choose_access(option: &'static str, cx: &mut VisualTestContext) {
        click("group-device-access", cx);
        click(option, cx);
        if option != "group-device-access-operate" {
            cx.simulate_keystrokes("enter");
            draw(cx);
        }
    }

    /// While the group's other devices may not operate this machine, it
    /// takes nothing from the other machines whatever its switches hold:
    /// the switches show off and a click on them changes nothing, until
    /// the devices may operate it again.
    #[gpui::test]
    fn the_accept_switches_follow_what_other_devices_may_do(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::test_client::init);
        // The machine's host tells the window of hosting changes from its
        // own threads, as it does in the app.
        cx.executor().allow_parking();
        let root = crate::store::tests::scratch_root("group-accept");
        std::fs::create_dir_all(&root).unwrap();
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        let mut controller = super::super::RemoteController::new(
            HostMux::new(attachments, from_host.clone()),
            root.clone(),
            None,
            tcode_client::HostLink::new(to_host.clone(), from_host.clone()),
            Default::default(),
        );
        controller
            .start_hosting(
                &tcode_core::settings::TraverseSetting {
                    sources: Vec::new(),
                },
                "Studio".into(),
            )
            .unwrap();
        controller
            .hosting(HostingAction::Group(GroupAction::Create {
                name: "Home".into(),
            }))
            .unwrap();
        cx.update(|cx| cx.set_global(controller));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    tcode_client::HostLink::new(to_host, from_host),
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    cx,
                )
            });
            let section = cx.new(|_| GroupSection::new(store, super::super::spaces::this_machine));
            let probe = cx.new(|_| Probe(section));
            gpui_base::Root::new(probe, window, cx)
        });
        cx.simulate_resize(gpui::size(px(900.), px(1400.)));
        draw(cx);

        choose_access("group-device-access-view", cx);
        assert_eq!(group(cx).device_access, GroupDeviceAccess::View);
        click("group-accept-projects", cx);
        assert!(
            !group(cx).accept_projects,
            "a switch that follows the devices' access sends nothing"
        );

        choose_access("group-device-access-operate", cx);
        click("group-accept-projects", cx);
        assert!(group(cx).accept_projects, "the switch works again");

        cx.update(|_, cx| {
            cx.update_global::<super::super::RemoteController, _>(|controller, _| {
                controller.stop_hosting()
            });
        });
        let _ = std::fs::remove_dir_all(root);
    }
}
