use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::progress::Progress;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, Role, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::time::{humanize_ago, now_secs};
use tcode_protocol::{CommandResponse, ExternalImportState, ExternalThread, RecentDir, SourceTool};

const RECENT_LIMIT: usize = 15;
const RECENT_ROW_HEIGHT_ESTIMATE: f32 = 64.;
const RECENT_VIEWPORT_MAX_HEIGHT: f32 = 390.;

enum RecentState {
    Loading,
    Ready(Vec<RecentDir>),
}

pub(super) struct AddProjectDialog {
    store: Entity<WorkspaceStore>,
    path_input: Entity<InputState>,
    recent: RecentState,
    path_error: bool,
}

pub(super) fn open(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| AddProjectDialog::new(store, window, cx));
    dialog.update(cx, |dialog, cx| dialog.scan(cx));
    let content = dialog.clone();
    let footer = dialog.clone();
    window.open_dialog(cx, move |builder, window, cx| {
        let dialog_content = content.clone();
        builder
            .w(px(680.))
            .rounded(crate::material::radius_overlay())
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .title(crate::tr!("sidebar.add_project").into_owned())
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_add_footer(&footer, window, cx))
    });
}

impl AddProjectDialog {
    fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::tr!("sidebar.path_placeholder").into_owned())
        });
        Self {
            store,
            path_input,
            recent: RecentState::Loading,
            path_error: false,
        }
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let recent = store.update(cx, |store, cx| store.scan_external_history(cx));
        cx.spawn(async move |this, cx| {
            let recent = recent.await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.recent = RecentState::Ready(recent);
                cx.notify();
            });
        })
        .detach();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(crate::tr!("sidebar.select_project").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update_in(cx, |dialog, window, cx| {
                    dialog.create_draft(path, window, cx);
                });
            }
        })
        .detach();
    }

    fn open_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = PathBuf::from(self.path_input.read(cx).value().trim());
        if !path.is_absolute() {
            self.path_error = true;
            cx.notify();
            return;
        }
        let is_directory = self
            .store
            .update(cx, |store, cx| store.is_directory(path.clone(), cx));
        cx.spawn_in(window, async move |this, cx| {
            let is_directory = is_directory.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                if is_directory {
                    dialog.create_draft(path, window, cx);
                } else {
                    dialog.path_error = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn create_draft(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path.clone(), cx));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(tcode_protocol::CommandResponse::ProjectId(Some(project_id))) = create.await
            else {
                let _ = this.update_in(cx, |dialog, _, cx| {
                    dialog.path_error = true;
                    cx.notify();
                });
                return;
            };
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.store.update(cx, |store, cx| {
                    store.start_draft(project_id, path, cx);
                });
                window.close_dialog(cx);
            });
        })
        .detach();
    }

    fn choose_recent(&mut self, recent: RecentDir, window: &mut Window, cx: &mut Context<Self>) {
        let path = recent.path.clone();
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path, cx));
        let threads = recent.threads;
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(CommandResponse::ProjectId(Some(project_id))) = create.await else {
                return;
            };
            // Subscribe before starting: an import short enough to finish
            // before the start reply lands is only recoverable through the
            // retained status snapshot.
            let import = store.update(cx, |store, cx| {
                store.watch_external_import(&project_id);
                store.start_external_import(&project_id, threads, cx)
            });
            if !matches!(
                import.await,
                Ok(CommandResponse::ExternalImportStarted(true))
            ) {
                store.update(cx, |store, _cx| {
                    store.unwatch_external_import(&project_id);
                });
                return;
            }
            let _ = this.update_in(cx, |dialog, window, cx| {
                window.close_dialog(cx);
                let store = dialog.store.clone();
                let progress = cx.new(|cx| ImportProgress::new(store, project_id, cx));
                let content = progress.clone();
                window.open_dialog(cx, move |builder, _, cx| {
                    let progress_content = content.clone();
                    builder
                        .w(px(480.))
                        .rounded(crate::material::radius_overlay())
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .shadow_xl()
                        .title(crate::tr!("sidebar.importing").into_owned())
                        .close_button(false)
                        .overlay_closable(false)
                        .keyboard(false)
                        .content(move |content_el, _, _| content_el.child(progress_content.clone()))
                });
            });
        })
        .detach();
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.recent {
            RecentState::Loading => v_flex()
                .gap_3()
                .py_4()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("sidebar.recent_loading"))
                .child(Progress::new("recent-directories-loading").loading(true))
                .into_any_element(),
            RecentState::Ready(recent) if recent.is_empty() => div()
                .py_4()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("sidebar.recent_empty"))
                .into_any_element(),
            RecentState::Ready(recent) => {
                // Keep the viewport and the flex column separate. Putting the
                // max height on the column itself lets flexbox shrink every row
                // until there is no overflow left for the wheel to scroll.
                let mut rows = v_flex().w_full().gap_1();
                for (index, recent) in recent.iter().take(RECENT_LIMIT).enumerate() {
                    let selected = recent.clone();
                    let name = directory_name(&recent.path);
                    let accessible_name =
                        crate::tr!("sidebar.open_recent", name = name.clone()).into_owned();
                    let path = middle_truncate(&recent.path, 76);
                    let ago = humanize_ago(now_secs().saturating_sub(recent.last_active_ms / 1000));
                    let counts = tool_counts(&recent.threads);
                    rows = rows.child(
                        crate::material::accessible_clickable(
                            v_flex(),
                            format!("recent-directory-{index}"),
                            Role::Button,
                            accessible_name,
                            cx,
                        )
                        .flex_none()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded(crate::material::radius_card())
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().list_hover))
                        .on_click(cx.listener(move |dialog, _, window, cx| {
                            dialog.choose_recent(selected.clone(), window, cx);
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .gap_3()
                                .child(div().font_bold().child(name))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(ago),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(path),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(counts),
                        ),
                    );
                }
                let visible_rows = recent.len().min(RECENT_LIMIT) as f32;
                let viewport_height =
                    px((visible_rows * RECENT_ROW_HEIGHT_ESTIMATE).min(RECENT_VIEWPORT_MAX_HEIGHT));
                div()
                    .id("recent-directory-list")
                    .w_full()
                    .h(viewport_height)
                    .overflow_y_scrollbar()
                    .child(div().size_full().child(rows))
                    .into_any_element()
            }
        }
    }
}

impl Render for AddProjectDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .child(crate::tr!("sidebar.recent_activity")),
                    )
                    .child(self.render_recent(cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                Input::new(&self.path_input)
                                    .flex_1()
                                    .rounded(crate::material::radius_input()),
                            )
                            // The native picker browses THIS machine; over a
                            // remote link the typed path is validated against
                            // the host instead (Query::IsDirectory).
                            .when(!self.store.read(cx).is_remote(), |row| {
                                row.child(
                                    Button::new("browse-project-directory")
                                        .rounded(crate::material::radius_button())
                                        .label(crate::tr!("sidebar.browse"))
                                        .on_click(cx.listener(|dialog, _, window, cx| {
                                            dialog.browse(window, cx);
                                        })),
                                )
                            }),
                    )
                    .when(self.path_error, |column| {
                        column.child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().danger)
                                .child(crate::tr!("sidebar.invalid_path")),
                        )
                    }),
            )
    }
}

/// Renders the host's replicated import status. It owns no progress state of
/// its own, so a completion that arrived before this view existed still shows
/// up: the subscription snapshot carries the retained latest run.
struct ImportProgress {
    store: Entity<WorkspaceStore>,
    project_id: String,
    _subscription: gpui::Subscription,
}

impl ImportProgress {
    fn new(store: Entity<WorkspaceStore>, project_id: String, cx: &mut Context<Self>) -> Self {
        Self {
            _subscription: observe_store_topics(&store, &[TopicKind::ExternalImport], cx),
            store,
            project_id,
        }
    }
}

impl Render for ImportProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self
            .store
            .read(cx)
            .external_import_status(&self.project_id)
            .map(|status| status.state.clone());
        // The "n of N" line describes a run still in flight; the summary below
        // replaces it once the host reports the outcome.
        let running = match &state {
            Some(ExternalImportState::Progress { done, total, tool }) => {
                Some((*done, *total, tool.clone()))
            }
            _ => None,
        };
        let summary = match state {
            Some(ExternalImportState::Finished { imported, skipped }) => Some((imported, skipped)),
            _ => None,
        };
        // A run with nothing to import is complete the moment it starts; a
        // status that has not arrived yet is not.
        let percent = match running {
            Some((_, 0, _)) | None if summary.is_none() => 0.0,
            Some((done, total, _)) if total > 0 => done as f32 * 100.0 / total as f32,
            _ => 100.0,
        };
        let project_id = self.project_id.clone();
        let store = self.store.clone();
        v_flex()
            .gap_3()
            .py_2()
            .child(Progress::new("external-import-progress").value(percent))
            .when_some(running, |column, (done, total, tool)| {
                column.child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!(
                            "sidebar.import_progress",
                            done = done,
                            total = total,
                            tool = tool
                        )),
                )
            })
            .when_some(summary, |column, (imported, skipped)| {
                column
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child(crate::tr!(
                                "sidebar.import_summary",
                                imported = imported,
                                skipped = skipped
                            )),
                    )
                    .child(
                        h_flex().w_full().justify_end().child(
                            Button::new("external-import-ok")
                                .rounded(crate::material::radius_button())
                                .primary()
                                .label(crate::tr!("sidebar.import_ok"))
                                .on_click(move |_, window, cx| {
                                    store.update(cx, |store, _cx| {
                                        store.unwatch_external_import(&project_id);
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                    )
            })
    }
}

fn render_add_footer(
    dialog: &Entity<AddProjectDialog>,
    _window: &mut Window,
    _cx: &mut App,
) -> AnyElement {
    let open = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("add-project-cancel")
                .rounded(crate::material::radius_button())
                .label(crate::tr!("sidebar.cancel"))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("add-project-open")
                .rounded(crate::material::radius_button())
                .primary()
                .label(crate::tr!("sidebar.open"))
                .on_click(move |_, window, cx| {
                    open.update(cx, |dialog, cx| dialog.open_typed_path(window, cx));
                }),
        )
        .into_any_element()
}

fn directory_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn middle_truncate(path: &Path, max_chars: usize) -> String {
    let text = path.display().to_string();
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= max_chars {
        return text;
    }
    let left = (max_chars - 1) / 2;
    let right = max_chars - left - 1;
    format!(
        "{}…{}",
        chars[..left].iter().collect::<String>(),
        chars[chars.len() - right..].iter().collect::<String>()
    )
}

fn tool_counts(threads: &[ExternalThread]) -> String {
    let mut counts = HashMap::new();
    for thread in threads {
        *counts.entry(thread.source).or_insert(0_usize) += 1;
    }
    [
        SourceTool::ClaudeCode,
        SourceTool::ClaudeDesktop,
        SourceTool::T3Code,
        SourceTool::CodexCli,
        SourceTool::CodexDesktop,
    ]
    .into_iter()
    .filter_map(|source| {
        counts
            .get(&source)
            .map(|count| format!("{} ×{count}", source.display_name()))
    })
    .collect::<Vec<_>>()
    .join(" · ")
}
