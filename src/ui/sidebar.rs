use std::collections::HashSet;
use std::path::PathBuf;

use agent::ProviderKind;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement as _,
    select::{SearchableVec, Select, SelectState},
    v_flex,
};

use crate::app::{AppState, ProjectGroup};
use crate::store::{SessionMeta, now_secs};
use crate::ui::window_drag_area;

/// Left padding on the sidebar's top row so branding clears the native macOS
/// traffic lights (positioned at ~(9, 9)); a small inset elsewhere.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_INSET: f32 = 74.;
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHT_INSET: f32 = 8.;

/// Max threads shown per project group before the "Show more" row.
const THREADS_COLLAPSED_LIMIT: usize = 6;

pub struct SessionsSidebar {
    app_state: Entity<AppState>,
    /// Project ids whose thread list is expanded past the collapsed limit.
    expanded_groups: HashSet<String>,
    _subscriptions: Vec<Subscription>,
}

impl SessionsSidebar {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        Self {
            app_state,
            expanded_groups: HashSet::new(),
            _subscriptions: subscriptions,
        }
    }

    // -- actions ------------------------------------------------------------

    /// Prompt for a directory, then create a project rooted there.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select project directory".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(path) = paths.pop() {
                    let _ = this.update(cx, |this, cx| {
                        this.app_state.update(cx, |state, cx| {
                            state.create_project(path, cx);
                        });
                    });
                }
            }
        })
        .detach();
    }

    fn toggle_group(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_groups.remove(project_id) {
            self.expanded_groups.insert(project_id.to_string());
        }
        cx.notify();
    }

    // -- rendering ----------------------------------------------------------

    fn render_app_row(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The window has no separate titlebar: this row hosts the traffic
        // lights (native, top-left) plus branding, and doubles as the drag
        // handle for the sidebar side of the window top.
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
                    .tooltip("Collapse sidebar")
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
                    this.app_state.update(cx, |state, cx| state.open_palette(cx));
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
                        .child("Search"),
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
                    .child("PROJECTS"),
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
                            .tooltip("Sort"),
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
                            .tooltip("Add project")
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

        let mut container = v_flex().flex_none().child(
            h_flex()
                .id(gpui::SharedString::from(format!("project-header-{project_id}")))
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
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_sm()
                        .font_medium()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(group.project.name.clone()),
                )
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
                                .tooltip("Create new thread")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    open_new_session_dialog(
                                        this.app_state.clone(),
                                        Some(plus_cwd.clone()),
                                        Some(plus_project_id.clone()),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                ),
        );

        if !collapsed {
            for meta in group.sessions.iter().take(visible) {
                let is_active = active_id == Some(meta.id.as_str());
                let working = is_active && turn_running;
                container = container.child(self.render_thread(meta, is_active, working, cx));
            }
            if total > THREADS_COLLAPSED_LIMIT && !expanded {
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
                        .child("Show more"),
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
        let delete_session_id = meta.id.clone();
        let session_title = meta.title.clone();
        let app_state = self.app_state.clone();
        let row_key = format!("thread-{session_id}");
        let ago = humanize_ago(now_secs().saturating_sub(meta.updated_at));

        h_flex()
            .id(gpui::SharedString::from(format!("thread-row-{session_id}")))
            .group(row_key.clone())
            .h(px(30.))
            .items_center()
            .gap_2()
            .pl(px(30.))
            .pr_2()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(is_active, |s| s.bg(cx.theme().sidebar_accent))
            .hover(|s| s.bg(cx.theme().sidebar_accent))
            .on_click(cx.listener(move |this, _, _, cx| {
                let session_id = session_id.clone();
                this.app_state.update(cx, |state, cx| {
                    state.select_session(&session_id, cx);
                });
            }))
            .when(working, |row| {
                row.child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .bg(cx.theme().success),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().success)
                                .child("Working"),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .text_color(cx.theme().sidebar_foreground)
                    .child(meta.title.clone()),
            )
            .when(!working, |row| {
                // Right slot: relative time, replaced by an archive button on hover.
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
                                        .tooltip("Archive thread")
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            let app_state = app_state.clone();
                                            let session_id = delete_session_id.clone();
                                            let title = session_title.clone();
                                            // "Delete confirmation" off → archive immediately.
                                            if app_state.read(cx).settings.skip_delete_confirmation {
                                                app_state.update(cx, |state, cx| {
                                                    state.delete_session(&session_id, cx);
                                                });
                                                return;
                                            }
                                            window.open_alert_dialog(cx, move |alert, _, _| {
                                                let app_state = app_state.clone();
                                                let session_id = session_id.clone();
                                                alert
                                                    .title("Archive thread?")
                                                    .description(format!(
                                                        "Archive \"{title}\" and its saved conversation? This cannot be undone."
                                                    ))
                                                    .button_props(
                                                        DialogButtonProps::default()
                                                            .ok_variant(ButtonVariant::Danger)
                                                            .ok_text("Archive")
                                                            .cancel_text("Cancel")
                                                            .show_cancel(true),
                                                    )
                                                    .on_ok(move |_, _, cx| {
                                                        app_state.update(cx, |state, cx| {
                                                            state.delete_session(&session_id, cx);
                                                        });
                                                        true
                                                    })
                                            });
                                        }),
                                ),
                        ),
                )
            })
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
                        this.app_state.update(cx, |state, cx| state.open_settings(cx));
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
                            .child("Settings"),
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
            // Clear the native traffic lights (top-left) and keep the top of the
            // strip draggable like the expanded app row.
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
                    .tooltip("Expand sidebar")
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
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.app_state.update(cx, |state, cx| state.open_settings(cx));
                    })),
            )
    }
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

        let mut list = v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px_2()
            .pb_2()
            .gap(px(2.));

        if groups.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_3()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No projects yet. Add one to start."),
            );
        } else {
            for group in &groups {
                list = list.child(self.render_group(
                    group,
                    active_id.as_deref(),
                    turn_running,
                    cx,
                ));
            }
        }

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .text_color(cx.theme().sidebar_foreground)
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
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

// ---------------------------------------------------------------------------
// New-session dialog (shared by the sidebar plus-button and the ⌘K palette)
// ---------------------------------------------------------------------------

/// Open the "New thread" dialog, optionally preset to a project's directory.
/// Shared entry point so the command palette can start a thread too.
pub(crate) fn open_new_session_dialog(
    app_state: Entity<AppState>,
    preset_cwd: Option<PathBuf>,
    project_id: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = cx.new(|cx| NewSessionForm::new(preset_cwd, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        let form = form.clone();
        let app_state = app_state.clone();
        let project_id = project_id.clone();
        dialog
            .title("New thread")
            .w(px(480.))
            .content({
                let form = form.clone();
                move |content, _, _| content.child(form.clone())
            })
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Create")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let (provider, cwd, model) = form.read(cx).values(cx);
                if cwd.as_os_str().is_empty() || !cwd.is_dir() {
                    window.push_notification(
                        Notification::error(format!(
                            "Working directory does not exist: {}",
                            cwd.display()
                        )),
                        cx,
                    );
                    return false;
                }
                let project_id = project_id.clone();
                app_state.update(cx, |state, cx| {
                    state.create_session(provider, cwd, model, project_id, cx);
                });
                true
            })
    });
}

// ---------------------------------------------------------------------------
// New-session dialog form
// ---------------------------------------------------------------------------

const PROVIDERS: [ProviderKind; 2] = [ProviderKind::Codex, ProviderKind::ClaudeCode];
const CODEX_MODELS: [&str; 3] = ["gpt-5.6-sol", "gpt-5.6-sol-mini", "gpt-5.5-codex"];
const CLAUDE_MODELS: [&str; 4] = ["Provider default", "opus", "sonnet", "haiku"];

struct NewSessionForm {
    provider_index: usize,
    cwd_input: Entity<InputState>,
    model_select: Entity<SelectState<SearchableVec<&'static str>>>,
    custom_model_input: Entity<InputState>,
}

impl NewSessionForm {
    fn new(preset_cwd: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let default_cwd = preset_cwd
            .map(|p| p.display().to_string())
            .or_else(|| dirs::home_dir().map(|p| p.display().to_string()))
            .unwrap_or_default();
        let cwd_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("/path/to/project");
            state.set_value(default_cwd, window, cx);
            state
        });
        let model_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(CODEX_MODELS.to_vec()),
                Some(IndexPath::default()),
                window,
                cx,
            )
            .searchable(true)
        });
        let custom_model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Optional custom model ID"));
        Self {
            provider_index: 0,
            cwd_input,
            model_select,
            custom_model_input,
        }
    }

    fn values(&self, cx: &gpui::App) -> (ProviderKind, PathBuf, Option<String>) {
        let provider = PROVIDERS[self.provider_index.min(PROVIDERS.len() - 1)];
        let cwd = PathBuf::from(self.cwd_input.read(cx).value().trim().to_string());
        let custom = self.custom_model_input.read(cx).value().trim().to_string();
        let model = if !custom.is_empty() {
            Some(custom)
        } else {
            self.model_select
                .read(cx)
                .selected_value()
                .copied()
                .filter(|model| *model != "Provider default")
                .map(str::to_string)
        };
        (provider, cwd, model)
    }

    fn select_provider(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_index = index.min(PROVIDERS.len() - 1);
        let models = if self.provider_index == 0 {
            CODEX_MODELS.to_vec()
        } else {
            CLAUDE_MODELS.to_vec()
        };
        self.model_select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(models), window, cx);
            state.set_selected_index(Some(IndexPath::default()), window, cx);
        });
        self.custom_model_input.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
        });
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select working directory".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(path) = paths.pop() {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.cwd_input.update(cx, |state, cx| {
                            state.set_value(path.display().to_string(), window, cx);
                        });
                    });
                }
            }
        })
        .detach();
    }
}

impl Render for NewSessionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .py_2()
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().font_medium().child("Provider"))
                    .child(
                        RadioGroup::horizontal("new-session-provider")
                            .selected_index(Some(self.provider_index))
                            .child(Radio::new("provider-codex").label("Codex"))
                            .child(Radio::new("provider-claude").label("Claude Code"))
                            .on_click(cx.listener(|this, index: &usize, window, cx| {
                                this.select_provider(*index, window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().font_medium().child("Working directory"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(Input::new(&self.cwd_input)))
                            .child(
                                Button::new("browse-cwd")
                                    .outline()
                                    .icon(IconName::FolderOpen)
                                    .tooltip("Choose a directory")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.browse(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().font_medium().child("Model"))
                    .child(
                        Select::new(&self.model_select)
                            .search_placeholder("Search models")
                            .w_full(),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Or enter a custom model ID"),
                    )
                    .child(Input::new(&self.custom_model_input)),
            )
    }
}
