use std::{borrow::Cow, collections::HashSet};

use gpui::{
    Action, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::ContextMenuExt as _,
    scroll::ScrollableElement as _,
    v_flex,
};
use serde::Deserialize;

use tcode_core::project::SessionMeta;
use tcode_runtime::app::{AppState, ProjectGroup};

use crate::time::now_secs;
use crate::window_drag_area;

/// Left padding on the sidebar's top row so branding clears the native macOS
/// traffic lights (positioned at ~(9, 9)); a small inset elsewhere.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 74.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Max threads shown per project group before the "Show more" row.
const THREADS_COLLAPSED_LIMIT: usize = 6;

/// Localized thread-list toggle, when the project has enough threads to need
/// one. Keeping the toggle present in both states is what lets an expanded list
/// be collapsed again.
fn thread_list_toggle_label(total: usize, expanded: bool) -> Option<Cow<'static, str>> {
    (total > THREADS_COLLAPSED_LIMIT).then(|| {
        if expanded {
            tcode_i18n::tr!("sidebar.show_less")
        } else {
            tcode_i18n::tr!("sidebar.show_more")
        }
    })
}

/// A sidebar label that owns the remaining row width and always truncates on
/// one line. `text_ellipsis` alone still leaves GPUI's default wrapping on,
/// which lets a glyph move onto a second line at resize boundaries.
fn truncated_sidebar_label() -> gpui::Div {
    div().flex_1().min_w_0().truncate()
}

fn active_child_count_badge(count: usize, cx: &mut Context<SessionsSidebar>) -> gpui::Div {
    div()
        .flex_none()
        .min_w(px(18.))
        .px_1()
        .py(px(1.))
        .rounded_full()
        .bg(cx.theme().muted)
        .text_center()
        .text_size(px(10.))
        .font_semibold()
        .text_color(cx.theme().success)
        .child(count.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct ThreadRenderState {
    is_child: bool,
    show_unread: bool,
    active_direct_children: usize,
}

fn derive_thread_render_state(
    meta: &SessionMeta,
    sessions: &[SessionMeta],
    unread: bool,
    working: bool,
    mut is_working: impl FnMut(&str) -> bool,
) -> ThreadRenderState {
    let is_child = meta
        .parent_session_id
        .as_ref()
        .is_some_and(|parent_id| sessions.iter().any(|session| session.id == *parent_id));
    let active_direct_children = sessions
        .iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(meta.id.as_str()))
        .filter(|session| is_working(&session.id))
        .count();

    ThreadRenderState {
        is_child,
        // Orphaned child metadata is still child metadata and must not surface
        // completion unread state as an ordinary-thread blue dot.
        show_unread: meta.parent_session_id.is_none() && unread && !working,
        active_direct_children,
    }
}

fn thread_visible(meta: &SessionMeta, collapsed_parents: &HashSet<String>) -> bool {
    meta.parent_session_id
        .as_ref()
        .is_none_or(|parent_id| !collapsed_parents.contains(parent_id))
}

fn toggle_parent_for_row_click(
    collapsed_parents: &mut HashSet<String>,
    parent_id: &str,
    is_selected: bool,
    has_direct_children: bool,
) {
    if is_selected && has_direct_children && !collapsed_parents.remove(parent_id) {
        collapsed_parents.insert(parent_id.to_string());
    }
}

// Thread-row context-menu actions (each carries the target session id, so a
// single set of handlers on the sidebar root serves every row).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadRename(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadMarkUnread(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyPath(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadCopyId(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadArchive(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_thread, no_json)]
struct ThreadDelete(String);

// Project-header context-menu actions.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectArchiveAll(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectDelete(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_project, no_json)]
struct ProjectReveal(String);

/// In-progress inline rename of a thread row.
struct RenameState {
    session_id: String,
    input: Entity<InputState>,
    _sub: Subscription,
}

pub struct SessionsSidebar {
    app_state: Entity<AppState>,
    /// Project ids whose thread list is expanded past the collapsed limit.
    expanded_groups: HashSet<String>,
    /// Parent session ids whose direct child rows are folded away.
    collapsed_parents: HashSet<String>,
    /// The thread currently being renamed inline, if any.
    renaming: Option<RenameState>,
    _subscriptions: Vec<Subscription>,
}

impl SessionsSidebar {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        Self {
            app_state,
            expanded_groups: HashSet::new(),
            collapsed_parents: HashSet::new(),
            renaming: None,
            _subscriptions: subscriptions,
        }
    }

    // -- actions ------------------------------------------------------------

    /// Prompt for a directory, then create a project rooted there.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::add_project_dialog::open(self.app_state.clone(), window, cx);
    }

    fn toggle_group(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_groups.remove(project_id) {
            self.expanded_groups.insert(project_id.to_string());
        }
        cx.notify();
    }

    // -- context-menu action handlers ---------------------------------------

    fn on_rename(&mut self, action: &ThreadRename, window: &mut Window, cx: &mut Context<Self>) {
        let session_id = action.0.clone();
        let title = self
            .app_state
            .read(cx)
            .sessions
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(&title, window, cx);
            state.focus(window, cx);
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(window, cx),
                InputEvent::Blur => this.cancel_rename(cx),
                _ => {}
            },
        );
        self.renaming = Some(RenameState {
            session_id,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    fn commit_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.renaming.take() {
            let value = state.input.read(cx).value().to_string();
            self.app_state.update(cx, |app, cx| {
                app.rename_session(&state.session_id, &value, cx);
            });
            cx.notify();
        }
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    fn on_mark_unread(
        &mut self,
        action: &ThreadMarkUnread,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = action.0.clone();
        self.app_state
            .update(cx, |app, cx| app.mark_session_unread(&id, cx));
    }

    fn on_copy_path(
        &mut self,
        action: &ThreadCopyPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(meta) = self
            .app_state
            .read(cx)
            .sessions
            .iter()
            .find(|m| m.id == action.0)
        {
            let path = meta.cwd.to_string_lossy().into_owned();
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(path));
        }
    }

    fn on_copy_id(&mut self, action: &ThreadCopyId, _window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(action.0.clone()));
    }

    fn on_archive(&mut self, action: &ThreadArchive, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.0.clone();
        let title = self
            .app_state
            .read(cx)
            .sessions
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        self.archive_thread(&id, &title, window, cx);
    }

    fn on_delete(&mut self, action: &ThreadDelete, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.0.clone();
        let title = self
            .app_state
            .read(cx)
            .sessions
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.title.clone())
            .unwrap_or_default();
        self.delete_thread(&id, &title, window, cx);
    }

    fn on_project_archive_all(
        &mut self,
        action: &ProjectArchiveAll,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app_state = self.app_state.clone();
        let session_ids: Vec<String> = app_state
            .read(cx)
            .sessions
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(action.0.as_str())
                    && meta.archived_at.is_none()
                    && !app_state.read(cx).turn_running_for(&meta.id)
            })
            .map(|meta| meta.id.clone())
            .collect();
        if session_ids.is_empty() {
            return;
        }
        let count = session_ids.len();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let session_ids = session_ids.clone();
            alert
                .title(tcode_i18n::tr!("sidebar.archive_all_title"))
                .description(tcode_i18n::tr!(
                    "sidebar.archive_all_description",
                    count = count
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("sidebar.archive_all_action"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| {
                        for session_id in &session_ids {
                            state.archive_session(session_id, cx);
                        }
                    });
                    true
                })
        });
    }

    fn on_project_delete(
        &mut self,
        action: &ProjectDelete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app_state = self.app_state.clone();
        let project_id = action.0.clone();
        let (project_name, count) = {
            let state = app_state.read(cx);
            let Some(project) = state
                .projects
                .iter()
                .find(|project| project.id == project_id)
            else {
                return;
            };
            let count = state
                .sessions
                .iter()
                .filter(|meta| meta.project_id.as_deref() == Some(project_id.as_str()))
                .count();
            (project.name.clone(), count)
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let project_id = project_id.clone();
            alert
                .title(tcode_i18n::tr!(
                    "sidebar.remove_project_title",
                    project = project_name.clone()
                ))
                .description(tcode_i18n::tr!(
                    "sidebar.remove_project_description",
                    count = count
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("sidebar.remove_project_action"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.delete_project(&project_id, cx));
                    true
                })
        });
    }

    fn on_project_reveal(
        &mut self,
        action: &ProjectReveal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(root) = self
            .app_state
            .read(cx)
            .projects
            .iter()
            .find(|project| project.id == action.0)
            .map(|project| project.root.clone())
        {
            cx.reveal_path(&root);
        }
    }

    /// Archive a thread, honoring the delete-confirmation setting. Blocked while
    /// the turn runs (`archive_session` no-ops then; the caller's tooltip warns).
    fn archive_thread(
        &mut self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app_state = self.app_state.clone();
        if app_state.read(cx).turn_running_for(session_id) {
            return;
        }
        let session_id = session_id.to_string();
        if app_state.read(cx).settings.skip_delete_confirmation {
            app_state.update(cx, |state, cx| state.archive_session(&session_id, cx));
            return;
        }
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let session_id = session_id.clone();
            alert
                .title(tcode_i18n::tr!("sidebar.archive_title"))
                .description(tcode_i18n::tr!(
                    "sidebar.archive_description",
                    title = title
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("sidebar.archive_action"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    app_state.update(cx, |state, cx| state.archive_session(&session_id, cx));
                    true
                })
        });
    }

    /// Permanently delete a thread: an optional confirm, then (when it orphans a
    /// worktree) a second "remove the worktree too?" prompt.
    fn delete_thread(
        &mut self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let app_state = self.app_state.clone();
        let session_id = session_id.to_string();
        let skip = app_state.read(cx).settings.skip_delete_confirmation;
        if skip {
            proceed_delete(app_state, session_id, window, cx);
            return;
        }
        let title = title.to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let app_state = app_state.clone();
            let session_id = session_id.clone();
            alert
                .title(tcode_i18n::tr!(
                    "sidebar.delete_title",
                    title = title.clone()
                ))
                .description(tcode_i18n::tr!("sidebar.delete_description"))
                .button_props(
                    DialogButtonProps::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(tcode_i18n::tr!("sidebar.delete_action"))
                        .cancel_text(tcode_i18n::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    proceed_delete(app_state.clone(), session_id.clone(), window, cx);
                    true
                })
        });
    }

    // -- rendering ----------------------------------------------------------

    fn render_app_row(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window_drag_area(
            "sidebar-app-row-drag",
            h_flex()
                .h(px(52.))
                .flex_none()
                .items_center()
                .gap_2()
                .pl(px(TRAFFIC_LIGHT_INSET))
                .pr_2(),
            window,
            cx,
        )
        .child(
            Button::new("collapse-sidebar")
                .ghost()
                .small()
                .compact()
                .icon(IconName::PanelLeft)
                .tooltip(tcode_i18n::tr!("sidebar.collapse"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app_state.update(cx, |state, cx| {
                        state.toggle_sidebar_collapsed(cx);
                    });
                })),
        )
        .child(
            div()
                .text_sm()
                .font_bold()
                .text_color(cx.theme().sidebar_foreground)
                .child("tcode"),
        )
        .child(
            div()
                .px_1()
                .py(px(1.))
                .rounded_sm()
                .bg(cx.theme().muted)
                .text_color(cx.theme().muted_foreground)
                .text_size(px(9.))
                .font_semibold()
                .child("DEV"),
        )
    }

    fn render_search_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex_none().px_2().pb_1().child(
            h_flex()
                .id("sidebar-search")
                .h(px(32.))
                .items_center()
                .gap_2()
                .px_2()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().sidebar_accent))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.app_state
                        .update(cx, |state, cx| state.open_palette(cx));
                }))
                .child(
                    Icon::new(IconName::Search)
                        .small()
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(tcode_i18n::tr!("sidebar.search")),
                )
                .child(
                    div()
                        .px_1()
                        .py(px(1.))
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().muted_foreground)
                        .text_size(px(10.))
                        .child("⌘K"),
                ),
        )
    }

    fn render_projects_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sort_label =
            crate::settings::project_sort_label(self.app_state.read(cx).project_sort());
        h_flex()
            .flex_none()
            .h(px(28.))
            .items_center()
            .justify_between()
            .px_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("sidebar.projects")),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        Button::new("sort-projects")
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::SortAscending)
                            .tooltip(tcode_i18n::tr!("sidebar.sort", mode = sort_label))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    state.cycle_project_sort(cx);
                                });
                            })),
                    )
                    .child(
                        Button::new("add-project")
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(
                                Icon::empty()
                                    .path("icons/folder-plus.svg")
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .tooltip(tcode_i18n::tr!("sidebar.add_project"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_project(window, cx);
                            })),
                    ),
            )
    }

    fn render_group(
        &self,
        group: &ProjectGroup,
        active_id: Option<&str>,
        turn_running: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project_id = group.project.id.clone();
        let collapsed = self.app_state.read(cx).is_project_collapsed(&project_id);
        let has_unread = group.sessions.iter().any(|meta| {
            meta.parent_session_id.is_none()
                && self.app_state.read(cx).session_unread(meta.id.as_str())
        });
        let group_key = format!("group-{project_id}");

        let expanded = self.expanded_groups.contains(&project_id);
        let total = group.sessions.len();
        let visible = if expanded {
            total
        } else {
            total.min(THREADS_COLLAPSED_LIMIT)
        };

        let header_toggle_id = project_id.clone();
        let plus_cwd = group.project.root.clone();
        let plus_project_id = project_id.clone();
        let menu_project_id = project_id.clone();
        let can_archive = group
            .sessions
            .iter()
            .any(|meta| !self.app_state.read(cx).turn_running_for(&meta.id));

        let header = h_flex()
            .id(gpui::SharedString::from(format!(
                "project-header-{project_id}"
            )))
            .group(group_key.clone())
            .h(px(30.))
            .items_center()
            .gap_1()
            .px_2()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.app_state.update(cx, |state, cx| {
                    state.toggle_project_collapsed(&header_toggle_id, cx);
                });
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size_4()
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                Icon::new(IconName::Folder)
                    .size_4()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                truncated_sidebar_label()
                    .text_sm()
                    .font_medium()
                    .text_color(cx.theme().sidebar_foreground)
                    .child(group.project.name.clone()),
            )
            // Unread dot when any child thread is unread (hidden on hover so
            // the "+" can take the slot).
            .when(has_unread, |row| {
                row.child(
                    div()
                        .flex_none()
                        .group_hover(group_key.clone(), |s| s.invisible())
                        .child(div().size(px(6.)).rounded_full().bg(cx.theme().primary)),
                )
            })
            .child(
                div()
                    .invisible()
                    .group_hover(group_key.clone(), |s| s.visible())
                    .child(
                        Button::new("new-thread")
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::Plus)
                            .tooltip(tcode_i18n::tr!("sidebar.create_thread"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let cwd = plus_cwd.clone();
                                let project_id = plus_project_id.clone();
                                this.app_state.update(cx, |state, cx| {
                                    state.start_draft(project_id, cwd, cx);
                                });
                            })),
                    ),
            );
        let mut container =
            v_flex()
                .flex_none()
                .child(header.context_menu(move |menu, _window, _cx| {
                    let id = menu_project_id.clone();
                    let delete_label = tcode_i18n::tr!("sidebar.remove_project").into_owned();
                    menu.menu_with_enable(
                        tcode_i18n::tr!("sidebar.archive_all").into_owned(),
                        Box::new(ProjectArchiveAll(id.clone())),
                        can_archive,
                    )
                    .menu_element(Box::new(ProjectDelete(id.clone())), move |_window, cx| {
                        div()
                            .flex_1()
                            .text_color(cx.theme().danger)
                            .child(delete_label.clone())
                    })
                    .menu(
                        tcode_i18n::tr!("sidebar.reveal_project").into_owned(),
                        Box::new(ProjectReveal(id)),
                    )
                }));

        if !collapsed {
            for meta in group
                .sessions
                .iter()
                .take(visible)
                .filter(|meta| thread_visible(meta, &self.collapsed_parents))
            {
                let is_active = active_id == Some(meta.id.as_str());
                // "Working" covers parked sessions too — a thread that keeps
                // running in the background keeps its green dot.
                let working = (is_active && turn_running)
                    || (!is_active && self.app_state.read(cx).turn_running_for(&meta.id));
                container = container.child(self.render_thread(meta, is_active, working, cx));
            }
            if let Some(toggle_label) = thread_list_toggle_label(total, expanded) {
                let toggle_id = project_id.clone();
                container = container.child(
                    div()
                        .id(gpui::SharedString::from(format!("show-more-{project_id}")))
                        .pl(px(30.))
                        .py_1()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().sidebar_foreground))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_group(&toggle_id, cx);
                        }))
                        .child(toggle_label),
                );
            }
        }

        container
    }

    fn render_thread(
        &self,
        meta: &SessionMeta,
        is_active: bool,
        working: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session_id = meta.id.clone();
        let row_key = format!("thread-{session_id}");
        let ago = humanize_ago(now_secs().saturating_sub(meta.updated_at));
        let unread = self.app_state.read(cx).session_unread(&session_id);
        let is_worktree = meta.worktree.is_some();
        let render_state = {
            let state = self.app_state.read(cx);
            derive_thread_render_state(meta, &state.sessions, unread, working, |id| {
                state.turn_running_for(id)
            })
        };
        let is_child = render_state.is_child;
        let show_unread = render_state.show_unread;
        let active_direct_children = render_state.active_direct_children;
        let has_direct_children = self
            .app_state
            .read(cx)
            .sessions
            .iter()
            .any(|session| session.parent_session_id.as_deref() == Some(session_id.as_str()));

        // Inline rename takes over the whole row's content area.
        let renaming = self
            .renaming
            .as_ref()
            .filter(|r| r.session_id == session_id)
            .map(|r| r.input.clone());

        // Menu-builder captures.
        let menu_id = session_id.clone();
        let menu_running = working;

        let row = h_flex()
            .id(gpui::SharedString::from(format!("thread-row-{session_id}")))
            .group(row_key.clone())
            .h(px(30.))
            .items_center()
            .gap_2()
            .pl(px(if is_child { 42. } else { 30. }))
            .pr_2()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(is_active, |s| s.bg(cx.theme().sidebar_accent))
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .on_click(cx.listener({
                let session_id = session_id.clone();
                move |this, _, _, cx| {
                    let session_id = session_id.clone();
                    toggle_parent_for_row_click(
                        &mut this.collapsed_parents,
                        &session_id,
                        is_active,
                        has_direct_children,
                    );
                    this.app_state.update(cx, |state, cx| {
                        state.select_session(&session_id, cx);
                    });
                    cx.notify();
                }
            }))
            .when(working, |row| {
                row.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(6.)).rounded_full().bg(cx.theme().success))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_size(px(11.))
                                .text_color(cx.theme().success)
                                .child(tcode_i18n::tr!("sidebar.working")),
                        ),
                )
            })
            .when(is_child, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child("↳"),
                )
            });

        // Row body: rename input, or the (unread dot + worktree glyph + title).
        let row = if let Some(input) = renaming {
            row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| this.cancel_rename(cx)))
                    .child(Input::new(&input).small()),
            )
            .when(active_direct_children > 0, |row| {
                row.child(active_child_count_badge(active_direct_children, cx))
            })
        } else {
            row.when(show_unread, |row| {
                row.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().primary),
                )
            })
            .when(is_worktree, |row| {
                row.child(
                    Icon::empty()
                        .path("icons/git-branch.svg")
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(
                truncated_sidebar_label()
                    .text_size(px(13.))
                    .text_color(cx.theme().sidebar_foreground)
                    .child(meta.title.clone()),
            )
            .when(active_direct_children > 0, |row| {
                row.child(active_child_count_badge(active_direct_children, cx))
            })
            .when(!working, |row| {
                // Right slot: relative time, replaced by an archive button on hover.
                let title = meta.title.clone();
                let archive_id = session_id.clone();
                row.child(
                    div()
                        .relative()
                        .flex_none()
                        .h(px(20.))
                        .min_w(px(52.))
                        .child(
                            div()
                                .absolute()
                                .right_0()
                                .top(px(3.))
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .group_hover(row_key.clone(), |s| s.invisible())
                                .child(ago),
                        )
                        .child(
                            div()
                                .absolute()
                                .right_0()
                                .top_0()
                                .invisible()
                                .group_hover(row_key.clone(), |s| s.visible())
                                .child(
                                    Button::new("archive-thread")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .icon(
                                            Icon::empty()
                                                .path("icons/archive.svg")
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                        .tooltip(tcode_i18n::tr!("sidebar.archive"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.archive_thread(&archive_id, &title, window, cx);
                                        })),
                                ),
                        ),
                )
            })
        };

        row.context_menu(move |menu, _window, _cx| {
            let id = menu_id.clone();
            menu.menu(
                tcode_i18n::tr!("sidebar.ctx_rename").into_owned(),
                Box::new(ThreadRename(id.clone())),
            )
            .menu(
                tcode_i18n::tr!("sidebar.ctx_mark_unread").into_owned(),
                Box::new(ThreadMarkUnread(id.clone())),
            )
            .separator()
            .menu(
                tcode_i18n::tr!("sidebar.ctx_copy_path").into_owned(),
                Box::new(ThreadCopyPath(id.clone())),
            )
            .menu(
                tcode_i18n::tr!("sidebar.ctx_copy_id").into_owned(),
                Box::new(ThreadCopyId(id.clone())),
            )
            .separator()
            .menu_with_enable(
                tcode_i18n::tr!("sidebar.archive").into_owned(),
                Box::new(ThreadArchive(id.clone())),
                !menu_running,
            )
            .menu(
                tcode_i18n::tr!("sidebar.ctx_delete").into_owned(),
                Box::new(ThreadDelete(id.clone())),
            )
        })
        .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .id("sidebar-settings")
                    .h(px(40.))
                    .items_center()
                    .gap_2()
                    .px_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().sidebar_accent))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.open_settings(cx));
                    }))
                    .child(
                        Icon::new(IconName::Settings)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().sidebar_foreground)
                            .child(tcode_i18n::tr!("settings.title")),
                    ),
            )
    }

    fn render_collapsed(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .items_center()
            .pb_2()
            .gap_2()
            .child(window_drag_area(
                "sidebar-collapsed-drag",
                h_flex().h(px(52.)).w_full().flex_none(),
                window,
                cx,
            ))
            .child(
                Button::new("expand-sidebar")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelLeftOpen)
                    .tooltip(tcode_i18n::tr!("sidebar.expand"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state.update(cx, |state, cx| {
                            state.toggle_sidebar_collapsed(cx);
                        });
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("collapsed-settings")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Settings)
                    .tooltip(tcode_i18n::tr!("settings.title"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state
                            .update(cx, |state, cx| state.open_settings(cx));
                    })),
            )
    }
}

/// Delete `session_id`, first asking whether to also remove an orphaned worktree.
fn proceed_delete(
    app_state: Entity<AppState>,
    session_id: String,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let orphan = app_state.read(cx).worktree_orphaned_by_delete(&session_id);
    let Some(worktree) = orphan else {
        app_state.update(cx, |state, cx| {
            state.delete_session(&session_id, false, cx);
        });
        return;
    };
    let path = worktree.root_project_path.display().to_string();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let app_state = app_state.clone();
        let session_id = session_id.clone();
        let remove = session_id.clone();
        let keep = session_id.clone();
        let app_remove = app_state.clone();
        alert
            .title(tcode_i18n::tr!("sidebar.worktree_cleanup_title"))
            .description(tcode_i18n::tr!(
                "sidebar.worktree_cleanup_description",
                path = path.clone()
            ))
            .button_props(
                DialogButtonProps::default()
                    .ok_variant(ButtonVariant::Danger)
                    .ok_text(tcode_i18n::tr!("sidebar.worktree_cleanup_remove"))
                    .cancel_text(tcode_i18n::tr!("sidebar.worktree_cleanup_keep"))
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                app_remove.update(cx, |state, cx| {
                    state.delete_session(&remove, true, cx);
                });
                true
            })
            .on_cancel(move |_, _, cx| {
                app_state.update(cx, |state, cx| {
                    state.delete_session(&keep, false, cx);
                });
                true
            })
    });
}

impl Render for SessionsSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.app_state.read(cx).sidebar_collapsed {
            return self.render_collapsed(window, cx).into_any_element();
        }

        let (groups, active_id, turn_running) = {
            let state = self.app_state.read(cx);
            let active_id = state.active_session_id().map(str::to_string);
            let turn_running = state
                .active
                .as_ref()
                .map(|a| a.timeline.turn_running)
                .unwrap_or(false);
            (state.grouped_sessions(), active_id, turn_running)
        };

        let mut list_content = v_flex().w_full().px_2().pb_2().gap(px(2.));

        if groups.is_empty() {
            list_content = list_content.child(
                div()
                    .px_2()
                    .py_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("sidebar.empty")),
            );
        } else {
            for group in &groups {
                list_content = list_content.child(self.render_group(
                    group,
                    active_id.as_deref(),
                    turn_running,
                    cx,
                ));
            }
        }

        let list = div()
            .id("sidebar-project-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(div().size_full().child(list_content));

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .text_color(cx.theme().sidebar_foreground)
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_mark_unread))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_id))
            .on_action(cx.listener(Self::on_archive))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_project_archive_all))
            .on_action(cx.listener(Self::on_project_delete))
            .on_action(cx.listener(Self::on_project_reveal))
            .child(self.render_app_row(window, cx))
            .child(self.render_search_row(cx))
            .child(self.render_projects_header(cx))
            .child(list)
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Relative-time humanizer
// ---------------------------------------------------------------------------

fn humanize_ago(secs: u64) -> String {
    if secs < 60 {
        tcode_i18n::tr!("time.just_now").into_owned()
    } else if secs < 3600 {
        tcode_i18n::tr!("time.minutes_ago", count = secs / 60).into_owned()
    } else if secs < 86_400 {
        tcode_i18n::tr!("time.hours_ago", count = secs / 3600).into_owned()
    } else {
        tcode_i18n::tr!("time.days_ago", count = secs / 86_400).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::ProviderKind;
    use gpui::{TestAppContext, VisualTestContext, size};
    use std::path::PathBuf;
    use tcode_core::project::Project;
    use tcode_services::store::SessionStore;

    struct WorkingThreadRowProbe;

    impl Render for WorkingThreadRowProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            h_flex()
                .w_full()
                .h(px(30.))
                .items_center()
                .gap_2()
                .pl(px(42.))
                .pr_2()
                .debug_selector(|| "thread-row".into())
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(6.)))
                        .child(div().whitespace_nowrap().text_size(px(11.)).child("工作中")),
                )
                .child(div().flex_none().text_size(px(13.)).child("↳"))
                .child(
                    truncated_sidebar_label()
                        .debug_selector(|| "thread-title".into())
                        .text_size(px(13.))
                        .child("Phase 0 修复架构约束测试"),
                )
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn session(id: &str, parent_id: Option<&str>) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/project"), None);
        meta.id = id.to_string();
        meta.title = id.to_string();
        meta.parent_session_id = parent_id.map(str::to_string);
        meta
    }

    #[gpui::test]
    fn working_thread_title_stays_inside_row_at_every_sidebar_width(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| WorkingThreadRowProbe);
        let cx: &mut VisualTestContext = cx;

        // The resizable sidebar is constrained to 220..=380px. Half-pixel
        // increments cover Retina resize boundaries where glyph rounding used
        // to push the final character onto a second line.
        for half_pixel_width in 440..=760 {
            let width = half_pixel_width as f32 / 2.;
            cx.simulate_resize(size(px(width), px(60.)));
            draw(cx);

            let row = cx.debug_bounds("thread-row").expect("row bounds");
            let title = cx.debug_bounds("thread-title").expect("title bounds");
            assert!(
                title.top() >= row.top() && title.bottom() <= row.bottom(),
                "title escaped the row vertically at {width}px: row={row:?}, title={title:?}"
            );
            assert!(
                title.left() >= row.left() && title.right() <= row.right(),
                "title escaped the row horizontally at {width}px: row={row:?}, title={title:?}"
            );
        }
    }

    #[gpui::test]
    fn canceling_inline_rename_discards_the_unsaved_title(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "tcode-sidebar-rename-test-{}",
            tcode_services::store::now_millis()
        ));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let app_state = cx.new(|_| AppState::new(store));
        let project = Project::from_root(root.clone());
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.project_id = Some(project.id.clone());
        meta.title = "Original title".into();
        let session_id = meta.id.clone();
        app_state.update(cx, |state, _| {
            state.projects = vec![project];
            state.sessions = vec![meta];
        });

        let sidebar = cx.new(|cx| SessionsSidebar::new(app_state.clone(), cx));
        let (_, cx) = cx.add_window_view(|_, _| WorkingThreadRowProbe);
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.on_rename(&ThreadRename(session_id.clone()), window, cx);
                let input = sidebar.renaming.as_ref().unwrap().input.clone();
                input.update(cx, |input, cx| input.set_value("Unsaved title", window, cx));
                sidebar.cancel_rename(cx);
            });
        });

        cx.update(|_, cx| {
            assert!(sidebar.read(cx).renaming.is_none());
            assert_eq!(app_state.read(cx).sessions[0].title, "Original title");
        });

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_thread_list_keeps_its_toggle_after_expanding() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        tcode_i18n::set_locale(tcode_i18n::LANGUAGE_SIMPLIFIED_CHINESE);

        assert_eq!(thread_list_toggle_label(6, false), None);
        assert_eq!(
            thread_list_toggle_label(7, false).as_deref(),
            Some("显示更多")
        );
        assert_eq!(thread_list_toggle_label(7, true).as_deref(), Some("收起"));
    }

    #[test]
    fn child_unread_is_suppressed_by_render_state_derivation() {
        let parent = session("parent", None);
        let child = session("child", Some("parent"));
        let sessions = vec![parent, child.clone()];

        let state = derive_thread_render_state(&child, &sessions, true, false, |_| false);

        assert!(state.is_child);
        assert!(!state.show_unread);

        let orphan = session("orphan-child", Some("missing-parent"));
        let state =
            derive_thread_render_state(&orphan, std::slice::from_ref(&orphan), true, false, |_| {
                false
            });
        assert!(!state.is_child);
        assert!(!state.show_unread);
    }

    #[test]
    fn repeat_click_on_selected_parent_toggles_direct_child_rows() {
        let mut collapsed = HashSet::new();

        toggle_parent_for_row_click(&mut collapsed, "parent", false, true);
        assert!(!collapsed.contains("parent"), "first click only selects");

        toggle_parent_for_row_click(&mut collapsed, "parent", true, true);
        assert!(collapsed.contains("parent"));

        toggle_parent_for_row_click(&mut collapsed, "parent", true, true);
        assert!(!collapsed.contains("parent"), "repeat click restores rows");
    }

    #[test]
    fn active_direct_child_count_excludes_grandchildren() {
        let parent = session("parent", None);
        let child_working = session("child-working", Some("parent"));
        let child_idle = session("child-idle", Some("parent"));
        let grandchild_working = session("grandchild-working", Some("child-working"));
        let sessions = vec![
            parent.clone(),
            child_working,
            child_idle,
            grandchild_working,
        ];

        let state = derive_thread_render_state(&parent, &sessions, false, false, |id| {
            matches!(id, "child-working" | "grandchild-working")
        });

        assert_eq!(state.active_direct_children, 1);
    }

    #[test]
    fn collapsed_parent_hides_only_its_own_direct_children() {
        let collapsed = HashSet::from(["parent-a".to_string()]);

        assert!(!thread_visible(
            &session("child-a", Some("parent-a")),
            &collapsed
        ));
        assert!(thread_visible(
            &session("child-b", Some("parent-b")),
            &collapsed
        ));
        assert!(thread_visible(&session("parent-a", None), &collapsed));
    }
}
