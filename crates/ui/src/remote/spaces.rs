//! Shared spaces: named sets of projects this machine shares with
//! collaborators, each behind a reusable link.
//!
//! [`Spaces`] is the one owner of a machine's spaces on this client: the
//! window's own machine through [`RemoteController`] while it hosts, or a
//! machine this window is attached to as a full device through
//! `Query::Hosting`. Every surface — Settings → Remote, the Machines page and
//! the sidebar's share items — reads and changes spaces through it, so a
//! change made on one repaints the others.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EntityId, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
use tcode_core::project::Project;
use tcode_protocol::{HostingAction, HostingState, SpaceAction, SpaceInfo};

use crate::icon::Icon;
use crate::overlay::{DialogActions, Notification, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::menu::PopupMenu;

#[cfg(feature = "remote-hosting")]
use super::RemoteController;

/// Add a project to a space, or take it out again.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_spaces, no_json)]
pub(crate) struct ToggleShare {
    pub space_id: String,
    pub project_id: String,
}

/// Create a space holding this project, then show its link.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_spaces, no_json)]
pub(crate) struct NewSpaceAndShare(pub String);

/// Which machine a [`Spaces`] manages.
enum SpacesHost {
    #[cfg(feature = "remote-hosting")]
    Local,
    Remote(WeakEntity<WorkspaceStore>),
}

pub(crate) struct Spaces {
    host: SpacesHost,
    state: Option<HostingState>,
    /// Bumped by every change made here, so a read that was in flight across
    /// it cannot put back what the change replaced.
    generation: u64,
    refreshing: bool,
    /// This machine's projects by the last local window that showed them: a
    /// window attached elsewhere still names the projects of the spaces this
    /// machine hosts.
    #[cfg(feature = "remote-hosting")]
    local_projects: Option<Vec<Project>>,
    #[cfg(feature = "remote-hosting")]
    projects_from: Option<(EntityId, Subscription)>,
    _poll: Task<()>,
}

impl Spaces {
    fn new(host: SpacesHost, cx: &mut Context<Self>) -> Self {
        // Members come and go and other clients change spaces too; local
        // reads are a lock away, remote ones a round trip.
        let interval = match &host {
            #[cfg(feature = "remote-hosting")]
            SpacesHost::Local => Duration::from_secs(2),
            SpacesHost::Remote(_) => Duration::from_secs(3),
        };
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                if this.update(cx, |spaces, cx| spaces.refresh(cx)).is_err() {
                    return;
                }
            }
        });
        let mut spaces = Self {
            host,
            state: None,
            generation: 0,
            refreshing: false,
            #[cfg(feature = "remote-hosting")]
            local_projects: None,
            #[cfg(feature = "remote-hosting")]
            projects_from: None,
            _poll: poll,
        };
        spaces.refresh(cx);
        spaces
    }

    pub(crate) fn spaces(&self) -> &[SpaceInfo] {
        self.state
            .as_ref()
            .map(|state| state.spaces.as_slice())
            .unwrap_or_default()
    }

    pub(crate) fn space(&self, id: &str) -> Option<&SpaceInfo> {
        self.spaces().iter().find(|space| space.id == id)
    }

    /// Whether the machine accepts new devices at all; a space's link works
    /// only while it does.
    pub(crate) fn accepting(&self) -> bool {
        self.state.as_ref().is_some_and(|state| state.enabled)
    }

    /// The names of the spaces sharing `project_id`.
    pub(crate) fn sharing(&self, project_id: &str) -> Vec<String> {
        self.spaces()
            .iter()
            .filter(|space| space.project_ids.iter().any(|id| id == project_id))
            .map(|space| space.name.clone())
            .collect()
    }

    /// The managed machine's projects, when this client knows them.
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    pub(crate) fn projects(&self, cx: &App) -> Option<Vec<Project>> {
        match &self.host {
            #[cfg(feature = "remote-hosting")]
            SpacesHost::Local => self.local_projects.clone(),
            SpacesHost::Remote(store) => Some(store.upgrade()?.read(cx).projects()),
        }
    }

    #[cfg(feature = "remote-hosting")]
    fn follow_projects(&mut self, store: &Entity<WorkspaceStore>, cx: &mut Context<Self>) {
        if self
            .projects_from
            .as_ref()
            .is_some_and(|(id, _)| *id == store.entity_id())
        {
            return;
        }
        self.local_projects = Some(store.read(cx).projects());
        let subscription = cx.observe(store, |spaces, store, cx| {
            let projects = store.read(cx).projects();
            if spaces.local_projects.as_ref() != Some(&projects) {
                spaces.local_projects = Some(projects);
                cx.notify();
            }
        });
        self.projects_from = Some((store.entity_id(), subscription));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        match &self.host {
            #[cfg(feature = "remote-hosting")]
            SpacesHost::Local => {
                let state = local_hosting(HostingAction::State, cx).ok();
                self.set_state(state, cx);
            }
            SpacesHost::Remote(store) => {
                if self.refreshing {
                    return;
                }
                let Ok(task) =
                    store.update(cx, |store, cx| store.hosting(HostingAction::State, cx))
                else {
                    return;
                };
                self.refreshing = true;
                let generation = self.generation;
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |spaces, cx| {
                        spaces.refreshing = false;
                        // A failed read keeps what was last known: an offline
                        // link is the connection banner's to report.
                        if let Ok(state) = result
                            && spaces.generation == generation
                        {
                            spaces.set_state(Some(state), cx);
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn set_state(&mut self, state: Option<HostingState>, cx: &mut Context<Self>) {
        // The invitation countdown changes every read; only what spaces show
        // is worth a repaint.
        let shown = |state: &Option<HostingState>| {
            state
                .as_ref()
                .map(|state| (state.enabled, state.spaces.clone(), state.devices.clone()))
        };
        let changed = shown(&self.state) != shown(&state);
        self.state = state;
        if changed {
            cx.notify();
        }
    }

    /// Run `actions` in order, stopping at the first that fails, and answer
    /// with the machine's state after the last.
    pub(crate) fn act(
        &mut self,
        actions: Vec<SpaceAction>,
        cx: &mut Context<Self>,
    ) -> Task<Result<HostingState, String>> {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        match &self.host {
            #[cfg(feature = "remote-hosting")]
            SpacesHost::Local => {
                let mut result = Err(String::new());
                for action in actions {
                    result = local_hosting(HostingAction::Spaces(action), cx);
                    if result.is_err() {
                        break;
                    }
                }
                self.settle(generation, &result, cx);
                Task::ready(result)
            }
            SpacesHost::Remote(store) => {
                let store = store.clone();
                cx.spawn(async move |this, cx| {
                    let mut result = Err(String::new());
                    for action in actions {
                        result = match store.update(cx, |store, cx| {
                            store.hosting(HostingAction::Spaces(action), cx)
                        }) {
                            Ok(task) => task.await,
                            Err(_) => Err(crate::tr!("spaces.detached").into_owned()),
                        };
                        if result.is_err() {
                            break;
                        }
                    }
                    let _ = this.update(cx, |spaces, cx| spaces.settle(generation, &result, cx));
                    result
                })
            }
        }
    }

    fn settle(
        &mut self,
        generation: u64,
        result: &Result<HostingState, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(state) if self.generation == generation => self.set_state(Some(state.clone()), cx),
            _ => self.refresh(cx),
        }
    }

    /// Create a space and, given a project, share it there. Answers with the
    /// new space as the machine reports it.
    pub(crate) fn create(
        &mut self,
        name: String,
        share: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SpaceInfo, String>> {
        let known: HashSet<String> = self.spaces().iter().map(|space| space.id.clone()).collect();
        let created = self.act(vec![SpaceAction::Create { name }], cx);
        cx.spawn(async move |this, cx| {
            let state = created.await?;
            // A new space is appended; one created elsewhere meanwhile is
            // told apart by not having been known before.
            let space = state
                .spaces
                .into_iter()
                .rev()
                .find(|space| !known.contains(&space.id))
                .ok_or_else(|| crate::tr!("spaces.missing").into_owned())?;
            let Some(project_id) = share else {
                return Ok(space);
            };
            let shared = this
                .update(cx, |spaces, cx| {
                    spaces.act(
                        vec![SpaceAction::SetProjects {
                            id: space.id.clone(),
                            project_ids: vec![project_id],
                        }],
                        cx,
                    )
                })
                .map_err(|_| crate::tr!("spaces.detached").into_owned())?;
            shared
                .await?
                .spaces
                .into_iter()
                .find(|shared| shared.id == space.id)
                .ok_or_else(|| crate::tr!("spaces.missing").into_owned())
        })
    }

    pub(crate) fn toggle_project(
        &mut self,
        space_id: &str,
        project_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<HostingState, String>> {
        let Some(space) = self.space(space_id) else {
            return Task::ready(Err(crate::tr!("spaces.missing").into_owned()));
        };
        let mut project_ids = space.project_ids.clone();
        if let Some(index) = project_ids.iter().position(|id| id == project_id) {
            project_ids.remove(index);
        } else {
            project_ids.push(project_id.to_owned());
        }
        self.act(
            vec![SpaceAction::SetProjects {
                id: space_id.to_owned(),
                project_ids,
            }],
            cx,
        )
    }

    /// Remove a member, and with `regenerate` retire the link it joined by,
    /// so it cannot simply join again.
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    pub(crate) fn remove_member(
        &mut self,
        device_id: String,
        space_id: String,
        regenerate: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<HostingState, String>> {
        let mut actions = vec![SpaceAction::RemoveMember { device_id }];
        if regenerate {
            actions.push(SpaceAction::RegenerateLink { id: space_id });
        }
        self.act(actions, cx)
    }
}

#[cfg(feature = "remote-hosting")]
fn local_hosting(action: HostingAction, cx: &App) -> Result<HostingState, String> {
    cx.try_global::<RemoteController>()
        .ok_or_else(|| crate::tr!("spaces.not_hosting").into_owned())?
        .hosting(action)
}

#[derive(Default)]
struct Registry {
    #[cfg(feature = "remote-hosting")]
    local: Option<Entity<Spaces>>,
    /// Per remote attachment; `None` where it is not a full device.
    remote: HashMap<EntityId, Option<Entity<Spaces>>>,
}

impl Global for Registry {}

/// This machine's spaces, while it hosts.
#[cfg(feature = "remote-hosting")]
pub(crate) fn local(cx: &mut App) -> Option<Entity<Spaces>> {
    if !cx
        .try_global::<RemoteController>()
        .is_some_and(RemoteController::is_hosting)
    {
        return None;
    }
    if let Some(spaces) = cx.default_global::<Registry>().local.clone() {
        return Some(spaces);
    }
    let spaces = cx.new(|cx| Spaces::new(SpacesHost::Local, cx));
    cx.default_global::<Registry>().local = Some(spaces.clone());
    Some(spaces)
}

/// This machine's spaces, for its own hosting settings in any window: the
/// window's store names the projects while it shows this machine.
#[cfg(feature = "remote-hosting")]
pub(crate) fn this_machine(store: &Entity<WorkspaceStore>, cx: &mut App) -> Option<Entity<Spaces>> {
    if store.read(cx).is_remote() {
        local(cx)
    } else {
        for_store(store, cx)
    }
}

/// The spaces of the machine `store` shows, where this client may manage
/// them: this machine while it hosts, or a machine attached as a full device.
/// A device that joined through a space link manages nothing.
pub(crate) fn for_store(store: &Entity<WorkspaceStore>, cx: &mut App) -> Option<Entity<Spaces>> {
    if !store.read(cx).is_remote() {
        #[cfg(feature = "remote-hosting")]
        {
            let spaces = local(cx)?;
            spaces.update(cx, |spaces, cx| spaces.follow_projects(store, cx));
            return Some(spaces);
        }
        #[cfg(not(feature = "remote-hosting"))]
        return None;
    }
    let id = store.entity_id();
    if let Some(spaces) = cx.default_global::<Registry>().remote.get(&id) {
        return spaces.clone();
    }
    let host_id = store.read(cx).remote_host_id().map(str::to_owned);
    let full = cx
        .try_global::<super::ClientAttachment>()
        .map(super::ClientAttachment::hosts)
        .unwrap_or_default()
        .into_iter()
        .find(|host| Some(&host.host_id) == host_id.as_ref())
        .is_some_and(|host| host.space_id.is_none());
    let spaces = full.then(|| {
        let weak = store.downgrade();
        cx.new(|cx| Spaces::new(SpacesHost::Remote(weak), cx))
    });
    cx.observe_release(store, move |_, cx| {
        cx.default_global::<Registry>().remote.remove(&id);
    })
    .detach();
    cx.default_global::<Registry>()
        .remote
        .insert(id, spaces.clone());
    spaces
}

/// Repaint a view whenever the spaces it shows change.
#[derive(Default)]
pub(crate) struct SpacesObserver(Option<(EntityId, Subscription)>);

impl SpacesObserver {
    pub(crate) fn watch<V: 'static>(
        &mut self,
        spaces: Option<&Entity<Spaces>>,
        cx: &mut Context<V>,
    ) {
        match spaces {
            Some(spaces)
                if self
                    .0
                    .as_ref()
                    .is_some_and(|(id, _)| *id == spaces.entity_id()) => {}
            Some(spaces) => {
                self.0 = Some((
                    spaces.entity_id(),
                    cx.observe(spaces, |_, _, cx| cx.notify()),
                ))
            }
            None => self.0 = None,
        }
    }
}

/// Tell the user why a change did not happen; a change that did shows itself.
fn report<T: 'static>(task: Task<Result<T, String>>, window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                let _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::error(
                            crate::tr!("spaces.failed", error = error).into_owned(),
                        ),
                        cx,
                    );
                });
            }
        })
        .detach();
}

fn quoted(key: &str, name: &str) -> String {
    crate::tr!(key, name = name).into_owned()
}

/// A project as the sidebar's share items name it.
#[derive(Clone)]
pub(crate) struct ShareTarget {
    pub spaces: Entity<Spaces>,
    pub project_id: String,
    pub project_name: String,
}

/// The share items ending a project's or thread's context menu: one check
/// item per space, then a new space. A thread's items share its whole
/// project, so they sit under a header naming it.
pub(crate) fn share_items(
    menu: PopupMenu,
    target: &ShareTarget,
    thread: bool,
    cx: &App,
) -> PopupMenu {
    let mut menu = menu.separator();
    if thread {
        menu = menu.menu_with_enable(
            quoted("spaces.share.project", &target.project_name),
            Box::new(gpui::NoAction),
            false,
        );
    }
    for space in target.spaces.read(cx).spaces() {
        menu = menu.menu_with_check(
            quoted("spaces.share.to", &space.name),
            space.project_ids.contains(&target.project_id),
            Box::new(ToggleShare {
                space_id: space.id.clone(),
                project_id: target.project_id.clone(),
            }),
        );
    }
    menu.menu(
        crate::tr!("spaces.share.new").into_owned(),
        Box::new(NewSpaceAndShare(target.project_id.clone())),
    )
}

pub(crate) fn toggle_share(
    spaces: &Entity<Spaces>,
    action: &ToggleShare,
    window: &mut Window,
    cx: &mut App,
) {
    let task = spaces.update(cx, |spaces, cx| {
        spaces.toggle_project(&action.space_id, &action.project_id, cx)
    });
    report(task, window, cx);
}

/// The mark on a project shared in at least one space, naming them.
pub(crate) fn shared_badge(project_id: &str, names: &[String], cx: &App) -> AnyElement {
    let tooltip = crate::tr!(
        "spaces.share.badge",
        names = names.join(&crate::tr!("spaces.share.separator"))
    )
    .into_owned();
    let selector = format!("project-shared-{project_id}");
    div()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .flex_none()
        .child(
            Icon::empty()
                .path("icons/users.svg")
                .xsmall()
                .text_color(cx.theme().muted_foreground),
        )
        .tooltip(move |window, cx| crate::widgets::Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

type Created = Rc<dyn Fn(&SpaceInfo, &mut Window, &mut App)>;
/// A dialog's confirmation; `false` keeps the dialog open.
type Submit = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// Name a new space. With `share`, the project is added once the space
/// exists. The disclosure is part of the decision, so it is never optional.
pub(crate) fn open_create(
    spaces: Entity<Spaces>,
    share: Option<String>,
    on_created: impl Fn(&SpaceInfo, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let name = cx
        .new(|cx| InputState::new(window, cx).placeholder(crate::tr!("spaces.create.placeholder")));
    let on_created: Created = Rc::new(on_created);
    let title = if share.is_some() {
        crate::tr!("spaces.create.share_title")
    } else {
        crate::tr!("spaces.create.title")
    }
    .into_owned();
    let submit: Submit = Rc::new({
        let name = name.clone();
        move |window, cx| {
            let typed = name.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            let task = spaces.update(cx, |spaces, cx| spaces.create(typed, share.clone(), cx));
            let on_created = on_created.clone();
            window
                .spawn(cx, async move |cx| {
                    let result = task.await;
                    let _ = cx.update(|window, cx| match result {
                        Ok(space) => on_created(&space, window, cx),
                        Err(error) => window.push_notification(
                            Notification::error(
                                crate::tr!("spaces.failed", error = error).into_owned(),
                            ),
                            cx,
                        ),
                    });
                })
                .detach();
            true
        }
    });
    let field = name.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let field = field.clone();
        let ok = submit.clone();
        let create = submit.clone();
        dialog
            .w(px(420.))
            .title(title.clone())
            .on_ok(move |_, window, cx| ok(window, cx))
            .content(move |content, _, cx| {
                content.child(
                    v_flex()
                        .gap_3()
                        .debug_selector(|| "spaces-create".into())
                        .child(Input::new(&field).rounded(crate::material::radius_input(cx)))
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(crate::tr!("spaces.create.disclosure")),
                        ),
                )
            })
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("spaces-create-cancel")
                            .label(crate::tr!("spaces.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("spaces-create-ok")
                            .primary()
                            .label(crate::tr!("spaces.create.action"))
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

/// From a project's or thread's menu: a new space holding the project, and
/// then its link to send.
pub(crate) fn new_space_and_share(
    spaces: Entity<Spaces>,
    project_id: String,
    project_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let shown = spaces.clone();
    open_create(
        spaces,
        Some(project_id),
        move |space, window, cx| {
            open_shared(
                shown.clone(),
                space.id.clone(),
                project_name.clone(),
                window,
                cx,
            )
        },
        window,
        cx,
    );
}

/// The link of a space just made for `project_name`, ready to send. It is
/// read from the machine on every paint, so a link that only becomes
/// reachable a moment later still arrives.
fn open_shared(
    spaces: Entity<Spaces>,
    space_id: String,
    project_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let model = spaces.read(cx);
        let Some(space) = model.space(&space_id).cloned() else {
            return dialog;
        };
        let accepting = model.accepting();
        let description =
            crate::tr!("spaces.shared.description", project = project_name.clone()).into_owned();
        let title = quoted("spaces.shared.title", &space.name);
        dialog
            .w(px(440.))
            .title(title)
            .content(move |content, _, cx| {
                content.child(link_body(&space, accepting, description.clone(), true, cx))
            })
            .footer(
                DialogActions::new().child(
                    Button::new("spaces-shared-done")
                        .primary()
                        .label(crate::tr!("spaces.done"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    });
}

fn copy_link(link: String, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(link));
    window.push_notification(
        Notification::info(crate::tr!("spaces.link.copied").into_owned()),
        cx,
    );
}

/// Why a space has no link to show, or `None` when it has one.
fn link_unavailable(space: &SpaceInfo, accepting: bool) -> Option<SharedString> {
    if space.link.is_some() {
        return None;
    }
    Some(
        if space.link_dead {
            crate::tr!("spaces.link.dead")
        } else if !space.link_enabled {
            crate::tr!("spaces.link.paused")
        } else if !accepting {
            crate::tr!("spaces.link.pairing_off")
        } else {
            crate::tr!("spaces.link.unavailable")
        }
        .into_owned()
        .into(),
    )
}

/// A space's link as the dialog after sharing shows it: the QR, the line
/// about it and Copy — or why there is no link.
fn link_body(
    space: &SpaceInfo,
    accepting: bool,
    description: String,
    compact: bool,
    cx: &App,
) -> AnyElement {
    let Some(link) = space.link.clone() else {
        let reason = link_unavailable(space, accepting).unwrap_or_default();
        return div()
            .text_size(px(13.))
            .text_color(if space.link_dead {
                cx.theme().danger_foreground
            } else {
                cx.theme().muted_foreground
            })
            .child(reason)
            .into_any_element();
    };
    let text = v_flex()
        .flex_1()
        .min_w_0()
        .gap_3()
        .child(
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
        .child(
            h_flex().gap_2().flex_wrap().child(
                Button::new(SharedString::from(format!("space-copy-{}", space.id)))
                    .ghost()
                    .outline()
                    .compact()
                    .label(crate::tr!("spaces.link.copy"))
                    .on_click({
                        let link = link.clone();
                        move |_, window, cx| copy_link(link.clone(), window, cx)
                    }),
            ),
        );
    super::qr::beside_qr(text, &link, compact, cx).into_any_element()
}

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use section::SpacesSection;

#[cfg(feature = "remote-hosting")]
pub(super) use machines::machine_links;

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod section {
    use super::*;
    use crate::icon::IconName;
    use crate::overlay::DialogButtons;
    use crate::widgets::ButtonVariant;
    use crate::widgets::checkbox::Checkbox;
    use crate::widgets::menu::DropdownMenu as _;
    use crate::widgets::switch::Switch;
    use gpui::prelude::FluentBuilder as _;
    use gpui_base::StyledExt as _;
    use tcode_protocol::HostedDevice;

    /// Move a member to another space.
    #[derive(Action, Clone, PartialEq, Eq, Deserialize)]
    #[action(namespace = tcode_spaces, no_json)]
    struct MoveMember {
        device_id: String,
        space_id: String,
    }

    type Resolve = fn(&Entity<WorkspaceStore>, &mut App) -> Option<Entity<Spaces>>;

    /// Settings → Remote's spaces: one row per space, the selected one open
    /// on its link, projects and members.
    pub(crate) struct SpacesSection {
        store: Entity<WorkspaceStore>,
        resolve: Resolve,
        expanded: Option<String>,
        observer: SpacesObserver,
    }

    impl SpacesSection {
        /// `resolve` names the machine: this one for the hosting panel, the
        /// attached one for the browser's.
        pub(crate) fn new(store: Entity<WorkspaceStore>, resolve: Resolve) -> Self {
            Self {
                store,
                resolve,
                expanded: None,
                observer: SpacesObserver::default(),
            }
        }

        fn act(&mut self, action: SpaceAction, window: &mut Window, cx: &mut Context<Self>) {
            if let Some(spaces) = (self.resolve)(&self.store, cx) {
                let task = spaces.update(cx, |spaces, cx| spaces.act(vec![action], cx));
                report(task, window, cx);
            }
        }

        fn on_toggle_share(
            &mut self,
            action: &ToggleShare,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if let Some(spaces) = (self.resolve)(&self.store, cx) {
                toggle_share(&spaces, action, window, cx);
            }
        }

        fn on_move_member(
            &mut self,
            action: &MoveMember,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.act(
                SpaceAction::MoveMember {
                    device_id: action.device_id.clone(),
                    space_id: action.space_id.clone(),
                },
                window,
                cx,
            );
        }

        fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let Some(spaces) = (self.resolve)(&self.store, cx) else {
                return;
            };
            let section = cx.entity().downgrade();
            open_create(
                spaces,
                None,
                move |space, _, cx| {
                    let id = space.id.clone();
                    let _ = section.update(cx, |section, cx| {
                        section.expanded = Some(id);
                        cx.notify();
                    });
                },
                window,
                cx,
            );
        }

        fn rename(&mut self, space: &SpaceInfo, window: &mut Window, cx: &mut Context<Self>) {
            let Some(spaces) = (self.resolve)(&self.store, cx) else {
                return;
            };
            let name = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::tr!("spaces.create.placeholder"))
                    .default_value(space.name.clone())
            });
            let id = space.id.clone();
            let submit: Submit = Rc::new({
                let name = name.clone();
                move |window, cx| {
                    let typed = name.read(cx).value().trim().to_owned();
                    if typed.is_empty() {
                        return false;
                    }
                    let task = spaces.update(cx, |spaces, cx| {
                        spaces.act(
                            vec![SpaceAction::Rename {
                                id: id.clone(),
                                name: typed,
                            }],
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
                let ok = submit.clone();
                let save = submit.clone();
                dialog
                    .w(px(420.))
                    .title(crate::tr!("spaces.rename_title").into_owned())
                    .on_ok(move |_, window, cx| ok(window, cx))
                    .content(move |content, _, cx| {
                        content.child(Input::new(&field).rounded(crate::material::radius_input(cx)))
                    })
                    .footer(
                        DialogActions::new()
                            .child(
                                Button::new("spaces-rename-cancel")
                                    .label(crate::tr!("spaces.cancel"))
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("spaces-rename-ok")
                                    .primary()
                                    .label(crate::tr!("spaces.rename_action"))
                                    .on_click(move |_, window, cx| {
                                        if save(window, cx) {
                                            window.close_dialog(cx);
                                        }
                                    }),
                            ),
                    )
            });
            name.update(cx, |name, cx| name.focus(window, cx));
        }

        fn delete(&mut self, space: &SpaceInfo, window: &mut Window, cx: &mut Context<Self>) {
            let Some(spaces) = (self.resolve)(&self.store, cx) else {
                return;
            };
            let id = space.id.clone();
            let name = space.name.clone();
            let members = space.members.len();
            window.open_alert_dialog(cx, move |alert, _, cx| {
                let spaces = spaces.clone();
                let id = id.clone();
                alert
                    .bg(cx.theme().popover)
                    .title(quoted("spaces.delete_title", &name))
                    .description(if members == 0 {
                        crate::tr!("spaces.delete_description_empty")
                    } else {
                        crate::tr!("spaces.delete_description")
                    })
                    .button_props(
                        DialogButtons::default()
                            .ok_variant(ButtonVariant::Danger)
                            .ok_text(crate::tr!("spaces.delete_action"))
                            .cancel_text(crate::tr!("spaces.cancel"))
                            .show_cancel(true),
                    )
                    .on_ok(move |_, window, cx| {
                        let task = spaces.update(cx, |spaces, cx| {
                            spaces.act(vec![SpaceAction::Delete { id: id.clone() }], cx)
                        });
                        report(task, window, cx);
                        true
                    })
            });
        }

        fn remove_member(
            &mut self,
            space: &SpaceInfo,
            member: &HostedDevice,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            let Some(spaces) = (self.resolve)(&self.store, cx) else {
                return;
            };
            open_remove_member(
                spaces,
                space.id.clone(),
                space.name.clone(),
                member.id.clone(),
                member.name.clone(),
                window,
                cx,
            );
        }

        fn space_row(
            &self,
            space: &SpaceInfo,
            expanded: bool,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            let id = space.id.clone();
            let status = if space.link_dead {
                Some((
                    crate::tr!("spaces.status.dead"),
                    cx.theme().danger.opacity(0.14),
                    cx.theme().danger_foreground,
                ))
            } else if !space.link_enabled {
                Some((
                    crate::tr!("spaces.status.paused"),
                    cx.theme().muted,
                    cx.theme().muted_foreground,
                ))
            } else {
                None
            };
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("space-row-{id}")),
                gpui::Role::Button,
                space.name.clone(),
                cx,
            )
            .aria_expanded(expanded)
            .debug_selector({
                let id = id.clone();
                move || format!("space-row-{id}")
            })
            .w_full()
            .min_h(px(44.))
            .px_3()
            .py_2p5()
            .gap_3()
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().list_hover))
            .on_click(cx.listener(move |section, _, _, cx| {
                section.expanded = if section.expanded.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id.clone())
                };
                cx.notify();
            }))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .small()
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_medium()
                            .truncate()
                            .child(space.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(summary(space)),
                    ),
            )
            .children(
                status.map(|(label, bg, fg)| crate::material::semantic_chip(label, bg, fg, cx)),
            )
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

        fn link_block(
            &self,
            space: &SpaceInfo,
            accepting: bool,
            compact: bool,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            let id = space.id.clone();
            let regenerate = |cx: &mut Context<Self>| {
                let id = id.clone();
                Button::new(SharedString::from(format!("space-regenerate-{id}")))
                    .compact()
                    .label(crate::tr!("spaces.link.regenerate"))
                    .on_click(cx.listener(move |section, _, window, cx| {
                        section.act(SpaceAction::RegenerateLink { id: id.clone() }, window, cx);
                    }))
            };
            let body = match &space.link {
                Some(link) => {
                    let text = v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().muted_foreground)
                                .child(quoted("spaces.link.description", &space.name)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .flex_wrap()
                                .child(
                                    Button::new(SharedString::from(format!("space-copy-{id}")))
                                        .ghost()
                                        .outline()
                                        .compact()
                                        .label(crate::tr!("spaces.link.copy"))
                                        .on_click({
                                            let link = link.clone();
                                            move |_, window, cx| copy_link(link.clone(), window, cx)
                                        }),
                                )
                                .child(regenerate(cx).ghost().outline()),
                        );
                    super::super::qr::beside_qr(text, link, compact, cx)
                        .debug_selector(|| "space-link".into())
                        .into_any_element()
                }
                None => {
                    let reason = link_unavailable(space, accepting).unwrap_or_default();
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
                                .text_color(if space.link_dead {
                                    cx.theme().danger_foreground
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(reason),
                        )
                        .when(space.link_dead, |row| row.child(regenerate(cx).primary()))
                        .into_any_element()
                }
            };
            let toggle_id = space.id.clone();
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
                                .child(crate::tr!("spaces.link.enabled")),
                        )
                        .child(
                            Switch::new(SharedString::from(format!("space-link-enabled-{id}")))
                                .checked(space.link_enabled)
                                .on_click(cx.listener(
                                    move |section, enabled: &bool, window, cx| {
                                        section.act(
                                            SpaceAction::SetLinkEnabled {
                                                id: toggle_id.clone(),
                                                enabled: *enabled,
                                            },
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                        ),
                )
                .into_any_element()
        }

        fn projects_block(
            &self,
            space: &SpaceInfo,
            projects: Option<&[Project]>,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            let mut chips = h_flex().w_full().gap_2().flex_wrap();
            for project_id in &space.project_ids {
                let known = projects.map(|projects| {
                    projects
                        .iter()
                        .find(|project| &project.id == project_id)
                        .map(|project| project.name.clone())
                });
                // Without a project list there is nothing to tell a removed
                // project from one this client has not seen.
                let (label, removed) = match known {
                    Some(Some(name)) => (name, false),
                    Some(None) => (crate::tr!("spaces.projects.removed").into_owned(), true),
                    None => (crate::tr!("spaces.projects.unknown").into_owned(), false),
                };
                let action = ToggleShare {
                    space_id: space.id.clone(),
                    project_id: project_id.clone(),
                };
                chips = chips.child(
                    h_flex()
                        .flex_none()
                        .max_w_full()
                        .h(px(28.))
                        .pl_2p5()
                        .pr_0p5()
                        .gap_1()
                        .items_center()
                        .rounded(crate::material::radius_chip(cx))
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().muted.opacity(0.5))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.))
                                .when(removed, |label| {
                                    label.italic().text_color(cx.theme().muted_foreground)
                                })
                                .child(label.clone()),
                        )
                        .child(
                            Button::new(SharedString::from(format!(
                                "space-unshare-{}-{project_id}",
                                space.id
                            )))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .aria_label(crate::tr!("spaces.projects.remove", name = label))
                            .on_click(cx.listener(
                                move |section, _, window, cx| {
                                    section.on_toggle_share(&action, window, cx);
                                },
                            )),
                        ),
                );
            }
            let add = projects
                .filter(|projects| !projects.is_empty())
                .map(|projects| {
                    let options: Vec<(String, String, bool)> = projects
                        .iter()
                        .map(|project| {
                            (
                                project.id.clone(),
                                project.name.clone(),
                                space.project_ids.contains(&project.id),
                            )
                        })
                        .collect();
                    let space_id = space.id.clone();
                    Button::new(SharedString::from(format!("space-add-project-{space_id}")))
                        .ghost()
                        .outline()
                        .compact()
                        .icon(IconName::Plus)
                        .label(crate::tr!("spaces.projects.add"))
                        .dropdown_menu(move |mut menu, _, _| {
                            for (project_id, name, shared) in &options {
                                menu = menu.menu_with_check(
                                    name.clone(),
                                    *shared,
                                    Box::new(ToggleShare {
                                        space_id: space_id.clone(),
                                        project_id: project_id.clone(),
                                    }),
                                );
                            }
                            menu
                        })
                });
            v_flex()
                .w_full()
                .gap_2()
                .child(Self::subheading(
                    crate::tr!("spaces.projects.title").into_owned().into(),
                    cx,
                ))
                .when(space.project_ids.is_empty(), |column| {
                    column.child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("spaces.projects.empty")),
                    )
                })
                .when(!space.project_ids.is_empty(), |column| column.child(chips))
                .children(add.map(|add| h_flex().child(add)))
                .into_any_element()
        }

        fn members_block(
            &self,
            space: &SpaceInfo,
            others: &[(String, String)],
            compact: bool,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            let mut rows = v_flex().w_full();
            for member in &space.members {
                let status_color = match &member.path {
                    Some(_) => cx.theme().success,
                    None => cx.theme().muted_foreground,
                };
                let move_to = (!others.is_empty()).then(|| {
                    let device_id = member.id.clone();
                    let others = others.to_vec();
                    Button::new(SharedString::from(format!("member-move-{}", member.id)))
                        .ghost()
                        .compact()
                        .label(crate::tr!("spaces.members.move"))
                        .dropdown_menu(move |mut menu, _, _| {
                            for (space_id, name) in &others {
                                menu = menu.menu(
                                    name.clone(),
                                    Box::new(MoveMember {
                                        device_id: device_id.clone(),
                                        space_id: space_id.clone(),
                                    }),
                                );
                            }
                            menu
                        })
                });
                let remove_space = space.clone();
                let remove_member = member.clone();
                let controls = h_flex()
                    .gap_2()
                    .items_center()
                    .when(compact, |controls| controls.w_full())
                    .child(
                        div()
                            .flex_none()
                            .when(compact, |status| status.flex_1())
                            .text_size(px(13.))
                            .text_color(status_color)
                            .child(super::super::path_label(member.path.as_ref())),
                    )
                    .children(move_to)
                    .child(
                        Button::new(SharedString::from(format!("member-remove-{}", member.id)))
                            .debug_selector({
                                let id = member.id.clone();
                                move || format!("member-remove-{id}")
                            })
                            .ghost()
                            .compact()
                            .danger()
                            .label(crate::tr!("spaces.members.remove"))
                            .on_click(cx.listener(move |section, _, window, cx| {
                                section.remove_member(&remove_space, &remove_member, window, cx);
                            })),
                    );
                let name = div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(15.))
                    .truncate()
                    .child(super::super::device_label(
                        &member.name,
                        member.platform.as_deref(),
                    ));
                rows = rows.child(if compact {
                    v_flex()
                        .w_full()
                        .py_1p5()
                        .gap_1()
                        .child(name)
                        .child(controls)
                        .into_any_element()
                } else {
                    h_flex()
                        .w_full()
                        .min_h(px(36.))
                        .gap_3()
                        .items_center()
                        .child(name)
                        .child(controls)
                        .into_any_element()
                });
            }
            v_flex()
                .w_full()
                .gap_1()
                .child(Self::subheading(
                    crate::tr!("spaces.members.title").into_owned().into(),
                    cx,
                ))
                .when(space.members.is_empty(), |column| {
                    column.child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("spaces.members.empty")),
                    )
                })
                .child(rows)
                .into_any_element()
        }

        fn detail(
            &self,
            space: &SpaceInfo,
            all: &[SpaceInfo],
            accepting: bool,
            projects: Option<&[Project]>,
            compact: bool,
            cx: &mut Context<Self>,
        ) -> AnyElement {
            let others: Vec<(String, String)> = all
                .iter()
                .filter(|other| other.id != space.id)
                .map(|other| (other.id.clone(), other.name.clone()))
                .collect();
            let rename_space = space.clone();
            let delete_space = space.clone();
            v_flex()
                .w_full()
                .px_3()
                .pt_1()
                .pb_3()
                .gap_5()
                .debug_selector(|| "space-detail".into())
                .child(self.link_block(space, accepting, compact, cx))
                .child(self.projects_block(space, projects, cx))
                .child(self.members_block(space, &others, compact, cx))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new(SharedString::from(format!("space-rename-{}", space.id)))
                                .ghost()
                                .outline()
                                .compact()
                                .label(crate::tr!("spaces.rename"))
                                .on_click(cx.listener(move |section, _, window, cx| {
                                    section.rename(&rename_space, window, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("space-delete-{}", space.id)))
                                .ghost()
                                .compact()
                                .danger()
                                .label(crate::tr!("spaces.delete"))
                                .on_click(cx.listener(move |section, _, window, cx| {
                                    section.delete(&delete_space, window, cx);
                                })),
                        ),
                )
                .into_any_element()
        }

        fn hairline(cx: &App) -> AnyElement {
            div()
                .w_full()
                .pl_3()
                .child(div().w_full().h(px(1.)).bg(cx.theme().border.opacity(0.6)))
                .into_any_element()
        }
    }

    impl gpui::Render for SpacesSection {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let spaces = (self.resolve)(&self.store, cx);
            self.observer.watch(spaces.as_ref(), cx);
            let Some(spaces) = spaces else {
                return div().into_any_element();
            };
            let compact = crate::window_seam::window_is_compact(window, cx);
            let model = spaces.read(cx);
            let list = model.spaces().to_vec();
            let accepting = model.accepting();
            let projects = model.projects(cx);
            if self
                .expanded
                .as_ref()
                .is_some_and(|id| !list.iter().any(|space| &space.id == id))
            {
                self.expanded = None;
            }
            let mut group = crate::material::group(cx);
            if list.is_empty() {
                group = group.child(
                    div()
                        .w_full()
                        .px_3()
                        .pt_3()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("spaces.intro")),
                );
            }
            for space in &list {
                let expanded = self.expanded.as_deref() == Some(space.id.as_str());
                group = group.child(self.space_row(space, expanded, cx));
                if expanded {
                    let detail =
                        self.detail(space, &list, accepting, projects.as_deref(), compact, cx);
                    group = group.child(detail);
                }
                group = group.child(Self::hairline(cx));
            }
            group = group.child(
                h_flex().w_full().px_3().py_2p5().child(
                    Button::new("spaces-new")
                        .ghost()
                        .compact()
                        .icon(IconName::Plus)
                        .label(crate::tr!("spaces.new"))
                        .on_click(cx.listener(|section, _, window, cx| section.create(window, cx))),
                ),
            );
            v_flex()
                .w_full()
                .debug_selector(|| "spaces-section".into())
                .on_action(cx.listener(Self::on_toggle_share))
                .on_action(cx.listener(Self::on_move_member))
                .child(
                    div()
                        .pl_3()
                        .pb(px(6.))
                        .text_size(px(11.))
                        .font_medium()
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("spaces.section")),
                )
                .child(group)
                .into_any_element()
        }
    }

    /// "2 projects · 3 members · 1 online".
    fn summary(space: &SpaceInfo) -> String {
        let counted = |one: &str, many: &str, count: usize| {
            if count == 1 {
                crate::tr!(one).into_owned()
            } else {
                crate::tr!(many, count = count).into_owned()
            }
        };
        let mut parts = vec![
            counted(
                "spaces.summary.project_one",
                "spaces.summary.projects",
                space.project_ids.len(),
            ),
            counted(
                "spaces.summary.member_one",
                "spaces.summary.members",
                space.members.len(),
            ),
        ];
        let online = space
            .members
            .iter()
            .filter(|member| member.path.is_some())
            .count();
        if online > 0 {
            parts.push(crate::tr!("spaces.summary.online", count = online).into_owned());
        }
        parts.join(" · ")
    }

    /// Removing a member unpairs the device. The link it joined by still
    /// works for anyone holding it, so retiring that link is the default.
    pub(crate) fn open_remove_member(
        spaces: Entity<Spaces>,
        space_id: String,
        space_name: String,
        device_id: String,
        device_name: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        let regenerate = Rc::new(std::cell::Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let checked = regenerate.get();
            let toggle = regenerate.clone();
            let confirm = regenerate.clone();
            let spaces = spaces.clone();
            let space_id = space_id.clone();
            let device_id = device_id.clone();
            dialog
                .w(px(420.))
                .close_button(false)
                .title(
                    crate::tr!(
                        "spaces.members.remove_title",
                        name = device_name.clone(),
                        space = space_name.clone()
                    )
                    .into_owned(),
                )
                .content(move |content, _, cx| {
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "spaces-remove-member".into())
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(crate::tr!("spaces.members.remove_description")),
                            )
                            .child(
                                Checkbox::new("spaces-remove-regenerate")
                                    .label(crate::tr!("spaces.members.regenerate"))
                                    .checked(checked)
                                    .on_click({
                                        let toggle = toggle.clone();
                                        move |checked: &bool, window, _| {
                                            toggle.set(*checked);
                                            window.refresh();
                                        }
                                    }),
                            ),
                    )
                })
                .footer(
                    DialogActions::new()
                        .child(
                            Button::new("spaces-remove-cancel")
                                .label(crate::tr!("spaces.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("spaces-remove-ok")
                                .debug_selector(|| "spaces-remove-ok".into())
                                .danger()
                                .label(crate::tr!("spaces.members.remove"))
                                .on_click(move |_, window, cx| {
                                    let task = spaces.update(cx, |spaces, cx| {
                                        spaces.remove_member(
                                            device_id.clone(),
                                            space_id.clone(),
                                            confirm.get(),
                                            cx,
                                        )
                                    });
                                    report(task, window, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    /// A member's space, as the Connected devices list marks it.
    pub(crate) fn member_badge(name: &str, cx: &App) -> AnyElement {
        h_flex()
            .flex_none()
            .max_w(px(160.))
            .gap_1()
            .px_2()
            .py(px(1.))
            .items_center()
            .rounded(crate::material::radius_chip(cx))
            .bg(cx.theme().muted)
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(Icon::empty().path("icons/users.svg").xsmall())
            .child(div().min_w_0().truncate().child(name.to_owned()))
            .into_any_element()
    }
}

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use section::member_badge;

#[cfg(feature = "remote-hosting")]
mod machines {
    use super::*;
    use crate::icon::IconName;
    use gpui_base::StyledExt as _;

    /// This machine's spaces on the Machines page, one compact row each:
    /// the link's state, its QR on demand and Copy. A paused or dead link
    /// says so and offers the one action that brings it back.
    pub(in crate::remote) fn machine_links<V: 'static>(
        spaces: &Entity<Spaces>,
        open_qr: Option<&str>,
        toggle_qr: fn(&mut V, String, &mut Context<V>),
        compact: bool,
        inset: f32,
        cx: &mut Context<V>,
    ) -> Option<AnyElement> {
        let model = spaces.read(cx);
        let list = model.spaces().to_vec();
        if list.is_empty() {
            return None;
        }
        let accepting = model.accepting();
        let mut rows = Vec::new();
        for space in list {
            let id = space.id.clone();
            let open = open_qr == Some(id.as_str()) && space.link.is_some();
            let (status, color) = match link_unavailable(&space, accepting) {
                None => (summary_line(&space), cx.theme().muted_foreground),
                Some(reason) if space.link_dead => (reason, cx.theme().danger_foreground),
                Some(reason) => (reason, cx.theme().muted_foreground),
            };
            let mut controls = h_flex().flex_none().gap_2().items_center();
            if let Some(link) = space.link.clone() {
                let toggle_id = id.clone();
                controls = controls
                    .child(
                        Button::new(SharedString::from(format!("machine-space-qr-{id}")))
                            .ghost()
                            .outline()
                            .compact()
                            .label(if open {
                                crate::tr!("spaces.machines.hide_qr")
                            } else {
                                crate::tr!("spaces.machines.show_qr")
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                toggle_qr(view, toggle_id.clone(), cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("machine-space-copy-{id}")))
                            .ghost()
                            .outline()
                            .compact()
                            .icon(IconName::Copy)
                            .aria_label(crate::tr!("spaces.link.copy"))
                            .tooltip(crate::tr!("spaces.link.copy").into_owned())
                            .on_click(move |_, window, cx| copy_link(link.clone(), window, cx)),
                    );
            } else if space.link_dead || !space.link_enabled {
                let action = if space.link_dead {
                    SpaceAction::RegenerateLink { id: id.clone() }
                } else {
                    SpaceAction::SetLinkEnabled {
                        id: id.clone(),
                        enabled: true,
                    }
                };
                let spaces = spaces.clone();
                controls = controls.child(
                    Button::new(SharedString::from(format!("machine-space-fix-{id}")))
                        .ghost()
                        .outline()
                        .compact()
                        .label(if space.link_dead {
                            crate::tr!("spaces.link.regenerate")
                        } else {
                            crate::tr!("spaces.machines.resume")
                        })
                        .on_click(move |_, window, cx| {
                            let task = spaces
                                .update(cx, |spaces, cx| spaces.act(vec![action.clone()], cx));
                            report(task, window, cx);
                        }),
                );
            }
            let line = h_flex()
                .w_full()
                .px(px(inset))
                .py(px(8.))
                .gap_3()
                .items_center()
                .debug_selector({
                    let id = id.clone();
                    move || format!("machine-space-{id}")
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_medium()
                                .truncate()
                                .child(space.name.clone()),
                        )
                        .child(div().text_size(px(13.)).text_color(color).child(status)),
                )
                .child(controls);
            let mut row = v_flex().w_full().child(line);
            if open && let Some(link) = &space.link {
                row = row.child(
                    super::super::qr::beside_qr(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .line_height(px(20.))
                            .child(quoted("spaces.link.description", &space.name)),
                        link,
                        compact,
                        cx,
                    )
                    .px(px(inset))
                    .pb(px(8.)),
                );
            }
            rows.push(row.into_any_element());
        }
        Some(
            v_flex()
                .w_full()
                .debug_selector(|| "hosts-spaces".into())
                .child(crate::material::list_caption(
                    crate::tr!("spaces.machines.section").into_owned().into(),
                    cx,
                ))
                .child(crate::material::plain_list(rows, cx))
                .into_any_element(),
        )
    }

    fn summary_line(space: &SpaceInfo) -> SharedString {
        let projects = space.project_ids.len();
        if projects == 1 {
            crate::tr!("spaces.summary.project_one")
        } else {
            crate::tr!("spaces.summary.projects", count = projects)
        }
        .into_owned()
        .into()
    }
}

#[cfg(all(test, feature = "remote-hosting"))]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, Render, TestAppContext, VisualTestContext};
    use tcode_traverse::{DeviceIdentity, HostConfig, HostMux, TraverseHost, TraverseMode};

    struct Probe(Entity<SpacesSection>);

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
        // Debug selectors are looked up by `'static` name.
        let selector: &'static str = selector.leak();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        draw(cx);
    }

    /// Removing a member from Settings → Remote unpairs the device and, with
    /// the dialog's checkbox left as it opens, retires the link it joined
    /// by: the old link no longer names the space's secret.
    #[gpui::test]
    fn removing_a_member_regenerates_the_link_by_default(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-spaces-remove-{}",
            tcode_services::store::now_millis()
        ));
        let member_dir = root.join("member");
        std::fs::create_dir_all(&member_dir).unwrap();
        // Idle pipes: no client attaches through the mux here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let mux = HostMux::new(to_host.clone(), from_host.clone());
        // A random port: the desktop's fixed one may be taken on this machine.
        let host = TraverseHost::start(
            mux.clone(),
            HostConfig {
                host_name: "Studio".into(),
                data_dir: root.clone(),
                traverse: TraverseMode::Off,
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        let space_id = host.create_space("Design".into()).unwrap();
        let joined_by = host.space_link_url(&space_id).unwrap();
        let invite = tcode_client::pairing::parse_pair_url(&joined_by).unwrap();
        let device = DeviceIdentity::load_or_create(&member_dir).unwrap();
        device.set_details("Phone".into(), None);
        tcode_traverse::pair_blocking(&invite, &device).unwrap();
        let member_id = device.endpoint_id().to_string();
        cx.update(|cx| {
            let mut controller = crate::remote::RemoteController::new(
                mux,
                root.clone(),
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
            let section = cx.new(|_| SpacesSection::new(store, this_machine));
            let probe = cx.new(|_| Probe(section));
            gpui_base::Root::new(probe, window, cx)
        });
        cx.simulate_resize(gpui::size(px(900.), px(1400.)));
        draw(cx);

        click(format!("space-row-{space_id}"), cx);
        click(format!("member-remove-{member_id}"), cx);
        click("spaces-remove-ok".into(), cx);

        let (devices, link) = cx.read(|cx| {
            let controller = cx.global::<crate::remote::RemoteController>();
            (
                controller.devices(),
                controller.hosting(HostingAction::State).unwrap(),
            )
        });
        assert!(
            devices.iter().all(|device| device.id != member_id),
            "the member is unpaired"
        );
        let link = link.spaces[0]
            .link
            .clone()
            .expect("the space still has a link");
        assert_ne!(
            tcode_client::pairing::parse_pair_url(&link).unwrap().secret,
            invite.secret,
            "the link the member joined by is retired"
        );
        cx.update(|_, cx| {
            cx.update_global::<crate::remote::RemoteController, _>(|controller, _| {
                controller.stop_hosting()
            });
        });
        let _ = std::fs::remove_dir_all(root);
    }
}
