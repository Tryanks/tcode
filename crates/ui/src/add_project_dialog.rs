use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputState},
    progress::Progress,
    scroll::ScrollableElement as _,
    v_flex,
};

use crate::time::now_secs;
use tcode_runtime::app::AppState;
use tcode_runtime::ui_facade::{
    ExternalImportUpdate, ExternalThread, RecentDir, SourceTool, is_directory,
};

const RECENT_LIMIT: usize = 15;

enum RecentState {
    Loading,
    Ready(Vec<RecentDir>),
}

pub(super) struct AddProjectDialog {
    app_state: Entity<AppState>,
    path_input: Entity<InputState>,
    recent: RecentState,
    path_error: bool,
}

pub(super) fn open(app_state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| AddProjectDialog::new(app_state, window, cx));
    dialog.update(cx, |dialog, cx| dialog.scan(cx));
    let content = dialog.clone();
    let footer = dialog.clone();
    window.open_dialog(cx, move |builder, window, cx| {
        let dialog_content = content.clone();
        builder
            .w(px(680.))
            .title(tcode_i18n::tr!("sidebar.add_project").into_owned())
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_add_footer(&footer, window, cx))
    });
}

impl AddProjectDialog {
    fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tcode_i18n::tr!("sidebar.path_placeholder").into_owned())
        });
        Self {
            app_state,
            path_input,
            recent: RecentState::Loading,
            path_error: false,
        }
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let recent = self
            .app_state
            .read(cx)
            .scan_external_history(cx.background_executor());
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
            prompt: Some(
                tcode_i18n::tr!("sidebar.select_project")
                    .into_owned()
                    .into(),
            ),
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
        if !path.is_absolute() || !is_directory(&path) {
            self.path_error = true;
            cx.notify();
            return;
        }
        self.create_draft(path, window, cx);
    }

    fn create_draft(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let created = self.app_state.update(cx, |state, cx| {
            state.create_project(path.clone(), cx).map(|project_id| {
                state.start_draft(project_id, path, cx);
            })
        });
        if created.is_some() {
            window.close_dialog(cx);
        }
    }

    fn choose_recent(&mut self, recent: RecentDir, window: &mut Window, cx: &mut Context<Self>) {
        let path = recent.path.clone();
        let project_id = self
            .app_state
            .update(cx, |state, cx| state.create_project(path, cx));
        let Some(project_id) = project_id else {
            return;
        };
        let threads = recent.threads;
        let total = threads.len();
        let current_tool = threads
            .first()
            .map(|thread| thread.source.display_name().to_string())
            .unwrap_or_default();
        let Some(receiver) = self.app_state.read(cx).start_external_import(
            &project_id,
            threads,
            cx.background_executor(),
        ) else {
            return;
        };

        window.close_dialog(cx);
        let progress = cx.new(|_| ImportProgress::new(self.app_state.clone(), project_id));
        progress.update(cx, |progress, cx| {
            progress.start(receiver, total, current_tool, cx)
        });
        let content = progress.clone();
        window.open_dialog(cx, move |builder, _, _| {
            let progress_content = content.clone();
            builder
                .w(px(480.))
                .title(tcode_i18n::tr!("sidebar.importing").into_owned())
                .close_button(false)
                .overlay_closable(false)
                .keyboard(false)
                .content(move |content_el, _, _| content_el.child(progress_content.clone()))
        });
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.recent {
            RecentState::Loading => v_flex()
                .gap_3()
                .py_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tcode_i18n::tr!("sidebar.recent_loading"))
                .child(Progress::new("recent-directories-loading").loading(true))
                .into_any_element(),
            RecentState::Ready(recent) if recent.is_empty() => div()
                .py_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tcode_i18n::tr!("sidebar.recent_empty"))
                .into_any_element(),
            RecentState::Ready(recent) => {
                let mut list = v_flex()
                    .id("recent-directory-list")
                    .max_h(px(390.))
                    .gap_1()
                    .overflow_y_scrollbar();
                for (index, recent) in recent.iter().take(RECENT_LIMIT).enumerate() {
                    let selected = recent.clone();
                    let name = directory_name(&recent.path);
                    let path = middle_truncate(&recent.path, 76);
                    let ago = humanize_ago(now_secs().saturating_sub(recent.last_active_ms / 1000));
                    let counts = tool_counts(&recent.threads);
                    list = list.child(
                        v_flex()
                            .id(format!("recent-directory-{index}"))
                            .gap_1()
                            .px_3()
                            .py_2()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().accent))
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
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(ago),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(path),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(counts),
                            ),
                    );
                }
                list.into_any_element()
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
                            .text_sm()
                            .font_semibold()
                            .child(tcode_i18n::tr!("sidebar.recent_activity")),
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
                            .child(Input::new(&self.path_input).flex_1())
                            .child(
                                Button::new("browse-project-directory")
                                    .label(tcode_i18n::tr!("sidebar.browse"))
                                    .on_click(cx.listener(|dialog, _, window, cx| {
                                        dialog.browse(window, cx);
                                    })),
                            ),
                    )
                    .when(self.path_error, |column| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(tcode_i18n::tr!("sidebar.invalid_path")),
                        )
                    }),
            )
    }
}

struct ImportProgress {
    app_state: Entity<AppState>,
    project_id: String,
    done: usize,
    total: usize,
    current_tool: String,
    summary: Option<(usize, usize)>,
}

impl ImportProgress {
    fn new(app_state: Entity<AppState>, project_id: String) -> Self {
        Self {
            app_state,
            project_id,
            done: 0,
            total: 0,
            current_tool: String::new(),
            summary: None,
        }
    }

    fn start(
        &mut self,
        receiver: async_channel::Receiver<ExternalImportUpdate>,
        total: usize,
        current_tool: String,
        cx: &mut Context<Self>,
    ) {
        self.total = total;
        self.current_tool = current_tool;

        cx.spawn(async move |this, cx| {
            while let Ok(update) = receiver.recv().await {
                let finished = matches!(update, ExternalImportUpdate::Finished { .. });
                let _ = this.update(cx, |progress, cx| {
                    match update {
                        ExternalImportUpdate::Progress { done, total, tool } => {
                            progress.done = done;
                            progress.total = total;
                            progress.current_tool = tool;
                        }
                        ExternalImportUpdate::Finished { imported, skipped } => {
                            progress.summary = Some((imported, skipped));
                            progress.app_state.update(cx, |state, cx| {
                                state.finish_external_import(&progress.project_id, cx);
                            });
                        }
                    }
                    cx.notify();
                });
                if finished {
                    break;
                }
            }
        })
        .detach();
    }
}

impl Render for ImportProgress {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let percent = if self.total == 0 {
            100.0
        } else {
            self.done as f32 * 100.0 / self.total as f32
        };
        v_flex()
            .gap_3()
            .py_2()
            .child(Progress::new("external-import-progress").value(percent))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!(
                        "sidebar.import_progress",
                        done = self.done,
                        total = self.total,
                        tool = self.current_tool.clone()
                    )),
            )
            .when_some(self.summary, |column, (imported, skipped)| {
                column
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child(tcode_i18n::tr!(
                                "sidebar.import_summary",
                                imported = imported,
                                skipped = skipped
                            )),
                    )
                    .child(
                        h_flex().w_full().justify_end().child(
                            Button::new("external-import-ok")
                                .primary()
                                .label(tcode_i18n::tr!("sidebar.import_ok"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
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
    let cancel = dialog.clone();
    let open = dialog.clone();
    DialogFooter::new()
        .child(
            Button::new("add-project-cancel")
                .label(tcode_i18n::tr!("sidebar.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = &cancel;
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("add-project-open")
                .primary()
                .label(tcode_i18n::tr!("sidebar.open"))
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
