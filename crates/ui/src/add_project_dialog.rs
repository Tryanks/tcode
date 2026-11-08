//! Add Project: create or pick a directory on the *host* and, optionally, import the
//! external-agent threads it already has.
//!
//! Everything here is host state reached over the pipe — the recents scan, the
//! folders listed under the path field, the project root and the import run.
//! Nothing browses *this* machine, so a local and a remote host pick a folder
//! the same way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::icon::{Icon, IconName};
use crate::overlay::{DialogActions, OverlayExt as _};
use crate::scroll::ScrollableElement as _;
use crate::sizing::fit_viewport;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::progress::Progress;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::time::{humanize_ago, now_secs};
use tcode_protocol::{
    CommandResponse, DirectoryListing, ExternalImportState, ExternalThread, RecentDir, SourceTool,
};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

const RECENT_LIMIT: usize = 15;
const RECENT_ROW_HEIGHT_ESTIMATE: f32 = 64.;
const RECENT_VIEWPORT_MAX_HEIGHT: f32 = 390.;
/// The recents list needs a definite height to scroll, and the dialog's flex
/// content height is not readable from its builder; reserve room for the
/// project inputs and footer so recents scroll instead of pushing the Open
/// button off a short window.
const RECENT_VIEWPORT_CHROME: f32 = 380.;
/// Where the folder list starts when Settings names no start folder: where
/// Create puts a new project. A host where nothing was created yet has no such
/// folder, and the list starts at the home instead.
pub(crate) const DEFAULT_START_DIRECTORY: &str = "~/TcodeProjects";
const FOLDER_ROW_HEIGHT: f32 = 30.;
const FOLDER_VIEWPORT_MAX_HEIGHT: f32 = 6.5 * FOLDER_ROW_HEIGHT;
const FOLDER_VIEWPORT_MIN_HEIGHT: f32 = 2. * FOLDER_ROW_HEIGHT;
/// The folder list's caption line, above its viewport.
const FOLDER_CAPTION: f32 = 28.;
/// The least the recents viewport is given before the folder list gives up
/// rows to it on a short window.
const RECENT_VIEWPORT_FLOOR: f32 = 120.;

/// The tallest the folder list may be in this window: what the recents do not
/// need of the room both lists share, so a short window shrinks the folder
/// list first instead of pushing the footer off.
fn folder_viewport_cap(window: &Window) -> f32 {
    let lists = f32::from(window.viewport_size().height)
        - RECENT_VIEWPORT_CHROME
        - FOLDER_CAPTION
        - RECENT_VIEWPORT_FLOOR;
    lists.clamp(FOLDER_VIEWPORT_MIN_HEIGHT, FOLDER_VIEWPORT_MAX_HEIGHT)
}

/// The host folders under the path field: those of the directory the typed
/// path is in, narrowed to the names that start with what follows its last
/// separator. With nothing typed, those of the start directory in Settings.
#[derive(Default)]
struct FolderBrowser {
    /// The directory text last asked of the host.
    requested: Option<String>,
    generation: u64,
    listing: Option<DirectoryListing>,
    /// Why the host could not list `requested`.
    error: Option<String>,
}

enum RecentState {
    Loading,
    Ready(Vec<RecentDir>),
    Failed(String),
}

pub(super) struct AddProjectDialog {
    store: Entity<WorkspaceStore>,
    path_input: Entity<InputState>,
    recent: RecentState,
    /// The last failure to show under the path row. Host-authored where the
    /// host produced it, so the user reads the host's own reason.
    error: Option<String>,
    name_input: Entity<InputState>,
    pending_created_project: Option<String>,
    folders: FolderBrowser,
    _subscriptions: Vec<gpui::Subscription>,
}

pub(super) fn open(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| AddProjectDialog::new(store, window, cx));
    dialog.update(cx, |dialog, cx| dialog.scan(cx));
    let content = dialog.clone();
    let footer = dialog.clone();
    window.open_dialog(cx, move |builder, _, _| {
        let dialog_content = content.clone();
        let confirm = content.clone();
        builder
            .wide()
            .title(crate::tr!("sidebar.add_project").into_owned())
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_add_footer(&footer))
            .on_ok(move |_, window, cx| {
                confirm.update(cx, |dialog, cx| {
                    if dialog
                        .name_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                    {
                        dialog.create_new_project(window, cx);
                    } else {
                        dialog.open_typed_path(window, cx);
                    }
                });
                // The host result owns dismissal, including validation failures.
                false
            })
    });
}

impl AddProjectDialog {
    fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = match store.read(cx).remote_host_name() {
            Some(host) => crate::tr!("sidebar.host_path_placeholder", host = host).into_owned(),
            None => crate::tr!("sidebar.path_placeholder").into_owned(),
        };
        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let name_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(crate::tr!("sidebar.project_name")));
        let subscriptions = vec![
            cx.subscribe(&name_input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&path_input, |dialog, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    dialog.refresh_folders(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &store,
                window,
                |dialog, _, change: &crate::store::StoreChange, window, cx| {
                    if change.topic == TopicKind::Index {
                        dialog.open_created_project(window, cx);
                    }
                },
            ),
        ];
        let mut dialog = Self {
            name_input,
            pending_created_project: None,
            folders: FolderBrowser::default(),
            _subscriptions: subscriptions,
            store,
            path_input,
            recent: RecentState::Loading,
            error: None,
        };
        dialog.refresh_folders(cx);
        dialog
    }

    /// Where the folder list starts when nothing is typed: the host directory
    /// Settings names, or [`DEFAULT_START_DIRECTORY`].
    fn start_directory(&self, cx: &App) -> String {
        self.store
            .read(cx)
            .settings()
            .project_start_dir
            .filter(|dir| !dir.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_START_DIRECTORY.to_owned())
    }

    /// Ask the host for the folders of the directory the typed path is in,
    /// unless that is the directory already asked for.
    fn refresh_folders(&mut self, cx: &mut Context<Self>) {
        let typed = self.path_input.read(cx).value().to_string();
        let (directory, _) = browse_target(&typed, &self.start_directory(cx));
        if self.folders.requested.as_ref() == Some(&directory) {
            return;
        }
        self.folders.requested = Some(directory.clone());
        self.folders.generation += 1;
        self.list_folders(directory, cx);
    }

    fn list_folders(&mut self, directory: String, cx: &mut Context<Self>) {
        let generation = self.folders.generation;
        let listing = self.store.update(cx, |store, cx| {
            store.browse_directories(directory.clone(), cx)
        });
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            let _ = this.update(cx, |dialog, cx| {
                if dialog.folders.generation != generation {
                    return;
                }
                match listing {
                    Ok(listing) => {
                        dialog.folders.listing = Some(listing);
                        dialog.folders.error = None;
                    }
                    // The default start exists only once a project has been
                    // created there; until then the list starts at the home.
                    Err(_) if directory == DEFAULT_START_DIRECTORY => {
                        dialog.list_folders("~".to_owned(), cx);
                    }
                    // The last listing stays up, so a half-typed name does
                    // not blank the list.
                    Err(error) => dialog.folders.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Put `path` in the path field, as a directory to go on typing in.
    fn enter_folder(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.path_input.update(cx, |input, cx| {
            input.replace_all(path, window, cx);
            input.focus(window, cx);
        });
        self.refresh_folders(cx);
        cx.notify();
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let recent = store.update(cx, |store, cx| store.scan_external_history(cx));
        cx.spawn(async move |this, cx| {
            let recent = recent.await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.recent = match recent {
                    Ok(recent) => RecentState::Ready(recent),
                    Err(error) => RecentState::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Send the typed text to the host as-is. Whether it is absolute and whether
    /// it is a directory are facts about the host's filesystem and path rules,
    /// so the host decides and its answer is what the user reads. The path is
    /// never canonicalized or otherwise touched here.
    fn open_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.path_input.read(cx).value().trim().to_owned();
        if typed.is_empty() {
            self.error = Some(crate::tr!("sidebar.path_required").into_owned());
            cx.notify();
            return;
        }
        self.create_draft(PathBuf::from(trim_trailing_separators(&typed)), window, cx);
    }

    fn create_draft(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path.clone(), cx));
        cx.spawn_in(window, async move |this, cx| {
            let project_id = match create.await {
                Ok(CommandResponse::ProjectId(Some(project_id))) => project_id,
                Ok(other) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(format!("unexpected create-project response: {other:?}"), cx);
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(error.message, cx);
                    });
                    return;
                }
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

    fn create_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let name = self.name_input.read(cx).value().to_string();
        let create = self
            .store
            .update(cx, |store, cx| store.create_new_project(name, cx));
        cx.spawn_in(window, async move |this, cx| {
            let response = create.await;
            let _ = this.update_in(cx, |dialog, window, cx| match response {
                Ok(CommandResponse::ProjectId(Some(id))) => {
                    dialog.pending_created_project = Some(id);
                    dialog.open_created_project(window, cx);
                }
                Ok(_) => dialog.fail(
                    crate::tr!("sidebar.create_project_no_response").into_owned(),
                    cx,
                ),
                Err(error) => dialog.fail(error.message, cx),
            });
        })
        .detach();
        cx.notify();
    }

    fn open_created_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.pending_created_project.as_ref() else {
            return;
        };
        let Some(root) = self.store.read(cx).project_root(id) else {
            return;
        };
        let id = self.pending_created_project.take().unwrap();
        self.store
            .update(cx, |store, cx| store.start_draft(id, root, cx));
        window.close_dialog(cx);
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        self.error = Some(crate::tr!("sidebar.path_rejected", reason = reason).into_owned());
        cx.notify();
    }

    fn choose_recent(&mut self, recent: RecentDir, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let path = recent.path.clone();
        let create = self
            .store
            .update(cx, |store, cx| store.create_project(path, cx));
        let threads = recent.threads;
        let store = self.store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let project_id = match create.await {
                Ok(CommandResponse::ProjectId(Some(project_id))) => project_id,
                Ok(other) => {
                    let _ = this.update_in(cx, |dialog, _, cx| {
                        dialog.fail(format!("unexpected create-project response: {other:?}"), cx);
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update_in(cx, |dialog, _, cx| dialog.fail(error.message, cx));
                    return;
                }
            };
            // Subscribe before starting: an import short enough to finish
            // before the start reply lands is only recoverable through the
            // retained status snapshot.
            let import = store.update(cx, |store, cx| {
                store.watch_external_import(&project_id);
                store.start_external_import(&project_id, threads, cx)
            });
            let started = import.await;
            if !matches!(started, Ok(CommandResponse::ExternalImportStarted(true))) {
                store.update(cx, |store, _cx| {
                    store.unwatch_external_import(&project_id);
                });
                let reason = match started {
                    Err(error) => error.message,
                    _ => crate::tr!("sidebar.import_refused").into_owned(),
                };
                let _ = this.update_in(cx, |dialog, _, cx| {
                    dialog.error =
                        Some(crate::tr!("sidebar.import_failed", reason = reason).into_owned());
                    cx.notify();
                });
                return;
            }
            let _ = this.update_in(cx, |dialog, window, cx| {
                window.close_dialog(cx);
                let store = dialog.store.clone();
                let progress = cx.new(|cx| ImportProgress::new(store, project_id, cx));
                let content = progress.clone();
                window.open_dialog(cx, move |builder, _, _| {
                    let progress_content = content.clone();
                    builder
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

    /// The folders of the typed path's directory, t3code-style: `..` first,
    /// then each folder whose name starts with what follows the last separator.
    /// Choosing one puts it in the path field to go on from.
    fn render_folders(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let typed = self.path_input.read(cx).value().to_string();
        // The filter does not depend on where an untyped path starts.
        let (_, filter) = browse_target(&typed, "");
        let mut rows = Vec::new();
        let mut shown_directory = None;
        if let Some(listing) = &self.folders.listing {
            let directory = with_separator(&listing.directory.to_string_lossy());
            if let Some(parent) = &listing.parent {
                rows.push(self.folder_row(
                    "project-folder-parent".into(),
                    IconName::CornerLeftUp,
                    "..".into(),
                    with_separator(&parent.to_string_lossy()),
                    cx,
                ));
            }
            let separator = separator_of(&directory);
            for name in matching_folders(&listing.entries, &filter) {
                rows.push(self.folder_row(
                    SharedString::from(format!("project-folder-{name}")),
                    IconName::Folder,
                    name.clone(),
                    format!("{directory}{name}{separator}"),
                    cx,
                ));
            }
            shown_directory = Some(directory);
        }
        let viewport_height = gpui_base::motion::transition(
            "project-folder-viewport",
            px((rows.len() as f32 * FOLDER_ROW_HEIGHT).min(folder_viewport_cap(window))),
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .text_caption(cx)
                            .child(crate::tr!("sidebar.folders")),
                    )
                    .when_some(shown_directory, |row, directory| {
                        row.child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_secondary(false, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(directory)),
                        )
                    }),
            )
            .when_some(self.folders.error.clone(), |column, error| {
                column.child(
                    div()
                        .text_secondary(false, cx)
                        .text_color(cx.theme().danger)
                        .child(crate::tr!("sidebar.folders_failed", reason = error).into_owned()),
                )
            })
            .child(
                div()
                    .id("project-folder-list")
                    .role(gpui::Role::List)
                    .aria_label(crate::tr!("sidebar.folders").into_owned())
                    .w_full()
                    .h(viewport_height)
                    .overflow_y_scrollbar()
                    .child(v_flex().w_full().children(rows)),
            )
            .into_any_element()
    }

    fn folder_row(
        &self,
        id: SharedString,
        icon: IconName,
        name: String,
        target: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        crate::material::menu_item_frame(false, crate::material::ItemFill::Hover, cx)
            .id(id)
            .role(gpui::Role::Button)
            .aria_label(target.clone())
            .w_full()
            .h(px(FOLDER_ROW_HEIGHT))
            .flex_none()
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(move |dialog, _, window, cx| {
                dialog.enter_folder(target.clone(), window, cx);
            }))
            .child(Icon::new(icon).text_color(cx.theme().muted_foreground))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_body(false, cx)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(name)),
            )
            .into_any_element()
    }

    fn render_recent(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = crate::motion::CrossFade::new(
            "recent-directories-state",
            match &self.recent {
                RecentState::Loading => 0,
                RecentState::Failed(_) => 1,
                RecentState::Ready(recent) if recent.is_empty() => 2,
                RecentState::Ready(_) => 3,
            },
            4,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        if state.shows(0) {
            layers.push((
                0,
                v_flex()
                    .child(crate::material::empty_row(
                        crate::tr!("sidebar.recent_loading"),
                        cx,
                    ))
                    .child(Progress::new("recent-directories-loading").loading(true))
                    .into_any_element(),
            ));
        }
        if let RecentState::Failed(error) = &self.recent {
            layers.push((
                1,
                crate::material::empty_row(
                    crate::tr!("sidebar.recent_failed", reason = error.clone()).into_owned(),
                    cx,
                )
                .text_color(cx.theme().danger)
                .into_any_element(),
            ));
        }
        if state.shows(2) {
            layers.push((
                2,
                crate::material::empty_row(crate::tr!("sidebar.recent_empty"), cx)
                    .into_any_element(),
            ));
        }
        if let RecentState::Ready(recent) = &self.recent
            && !recent.is_empty()
        {
            layers.push((3, {
                // Keep the viewport and the flex column separate. Putting the
                // max height on the column itself lets flexbox shrink every row
                // until there is no overflow left for the wheel to scroll.
                // A directory that arrives or goes folds in or out where it
                // is, the others gliding to their places.
                let items = recent
                    .iter()
                    .take(RECENT_LIMIT)
                    .enumerate()
                    .map(|(index, recent)| {
                        (
                            SharedString::from(recent.path.display().to_string()),
                            (index, recent.clone()),
                        )
                    });
                let rows = crate::motion::keyed(
                    "recent-directory-rows",
                    items,
                    ROWS,
                    window,
                    cx,
                    |_, (index, recent), _, cx| {
                        let index = *index;
                        let selected = recent.clone();
                        let name = directory_name(&recent.path);
                        let accessible_name =
                            crate::tr!("sidebar.open_recent", name = name.clone()).into_owned();
                        let path = recent.path.display().to_string();
                        let ago =
                            humanize_ago(now_secs().saturating_sub(recent.last_active_ms / 1000));
                        let counts = tool_counts(&recent.threads);
                        crate::material::panel_row(
                            format!("recent-directory-{index}"),
                            accessible_name.into(),
                            false,
                            cx,
                        )
                        .flex_none()
                        .cursor_pointer()
                        .on_click(cx.listener(move |dialog, _, window, cx| {
                            dialog.choose_recent(selected.clone(), window, cx);
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .justify_between()
                                        .gap_3()
                                        .child(
                                            div()
                                                .min_w_0()
                                                .text_body(false, cx)
                                                .font_bold()
                                                .overflow_hidden()
                                                .child(crate::widgets::fade_text(name)),
                                        )
                                        .child(
                                            div().flex_none().text_secondary(false, cx).child(ago),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_secondary(false, cx)
                                        .overflow_hidden()
                                        .child(crate::widgets::fade_text(path)),
                                )
                                .child(div().text_secondary(false, cx).child(counts)),
                        )
                        .into_any_element()
                    },
                )
                .render(v_flex().w_full().gap_1());
                let visible_rows = recent.len().min(RECENT_LIMIT) as f32;
                // The viewport grows and shrinks to its rows, gliding.
                let viewport_height = gpui_base::motion::transition(
                    "recent-directory-viewport",
                    fit_viewport(
                        (visible_rows * RECENT_ROW_HEIGHT_ESTIMATE).min(RECENT_VIEWPORT_MAX_HEIGHT),
                        window.viewport_size().height
                            - px(RECENT_VIEWPORT_CHROME
                                + FOLDER_CAPTION
                                + folder_viewport_cap(window)),
                    ),
                    gpui_base::motion::Transition::new(GLIDE),
                    window,
                    cx,
                );
                div()
                    .id("recent-directory-list")
                    .w_full()
                    .h(viewport_height)
                    .overflow_y_scrollbar()
                    .child(div().size_full().child(rows))
                    .into_any_element()
            }));
        }
        state.render(layers).w_full().into_any_element()
    }
}

impl Render for AddProjectDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.store.read(cx).remote_host_name().map(str::to_owned);
        let recent_label = match &host {
            Some(host) => crate::tr!("sidebar.recent_activity_host", host = host).into_owned(),
            None => crate::tr!("sidebar.recent_activity").into_owned(),
        };
        let path_hint = host
            .as_ref()
            .map(|host| crate::tr!("sidebar.host_path_hint", host = host).into_owned());
        let destination = format!(
            "~/TcodeProjects/{}",
            self.name_input.read(cx).value().trim()
        );
        let destination_hint = match &host {
            Some(host) => crate::tr!(
                "sidebar.new_project_destination_host",
                host = host,
                path = destination
            )
            .into_owned(),
            None => crate::tr!("sidebar.new_project_destination", path = destination).into_owned(),
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("sidebar.create_new_project")),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(Input::new(&self.name_input).flex_1())
                            .child(
                                Button::new("create-new-project")
                                    .label(crate::tr!("sidebar.create"))
                                    .on_click(cx.listener(|dialog, _, window, cx| {
                                        dialog.create_new_project(window, cx)
                                    })),
                            ),
                    )
                    .child(div().text_secondary(false, cx).child(destination_hint)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(Input::new(&self.path_input).w_full())
                    .when_some(path_hint, |column, hint| {
                        column.child(div().text_secondary(false, cx).child(hint))
                    })
                    .child(div().pt_1().child(self.render_folders(window, cx)))
                    .children(
                        crate::motion::shown(
                            "add-project-error",
                            self.error.clone(),
                            SHOW,
                            window,
                            cx,
                        )
                        .map(|error| {
                            error.fold(
                                div()
                                    .text_secondary(false, cx)
                                    .text_color(cx.theme().danger)
                                    .child(error.value.clone()),
                            )
                        }),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_caption(cx).child(recent_label))
                            .child(
                                div()
                                    .text_secondary(false, cx)
                                    .child(crate::tr!("sidebar.recent_import_hint")),
                            ),
                    )
                    .child(self.render_recent(window, cx)),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        // The summary cross-fades over the last "n of N" line.
        let outcome = crate::motion::CrossFade::new(
            "external-import-outcome",
            usize::from(summary.is_some()),
            2,
            CHANGE,
            window,
            cx,
        );
        let running = crate::motion::shown("external-import-running", running, SHOW, window, cx)
            .map(|running| running.value)
            .filter(|_| outcome.shows(0));
        let mut layers = Vec::new();
        if let Some((done, total, tool)) = running {
            layers.push((
                0,
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!(
                        "sidebar.import_progress",
                        done = done,
                        total = total,
                        tool = tool
                    ))
                    .into_any_element(),
            ));
        }
        if let Some((imported, skipped)) = summary {
            layers.push((
                1,
                v_flex()
                    .gap_3()
                    .child(div().text_title(cx).child(crate::tr!(
                        "sidebar.import_summary",
                        imported = imported,
                        skipped = skipped
                    )))
                    .child(
                        h_flex().w_full().justify_end().child(
                            Button::new("external-import-ok")
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
                    .into_any_element(),
            ));
        }
        v_flex()
            .gap_3()
            .py_2()
            .child(Progress::new("external-import-progress").value(percent))
            .when(!layers.is_empty(), |column| {
                column.child(outcome.render(layers).w_full())
            })
    }
}

fn render_add_footer(dialog: &Entity<AddProjectDialog>) -> AnyElement {
    let open = dialog.clone();
    DialogActions::new()
        .child(
            Button::new("add-project-cancel")
                .label(crate::tr!("sidebar.cancel"))
                .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("add-project-open")
                .primary()
                .label(crate::tr!("sidebar.open"))
                .on_click(move |_, window, cx| {
                    open.update(cx, |dialog, cx| dialog.open_typed_path(window, cx));
                }),
        )
        .into_any_element()
}

/// The directory to list for `typed`, and the start of the folder names to
/// show from it. The directory is everything up to the last separator; with
/// none, the typed text narrows the `start` directory instead. A path is the
/// host's, so both separators count: a host may be Windows.
fn browse_target(typed: &str, start: &str) -> (String, String) {
    let typed = typed.trim_start();
    if typed.is_empty() {
        return (start.to_owned(), String::new());
    }
    if typed == "~" {
        return ("~".to_owned(), String::new());
    }
    match typed.rfind(['/', '\\']) {
        Some(at) => (typed[..=at].to_owned(), typed[at + 1..].to_owned()),
        None => (start.to_owned(), typed.to_owned()),
    }
}

/// The folders whose names start with `filter`, ignoring case. Hidden ones
/// show only once a `.` is typed.
fn matching_folders<'a>(entries: &'a [String], filter: &str) -> impl Iterator<Item = &'a String> {
    let lower = filter.to_lowercase();
    let hidden = filter.starts_with('.');
    entries.iter().filter(move |name| {
        (hidden || !name.starts_with('.')) && name.to_lowercase().starts_with(&lower)
    })
}

/// The separator a host path is written with: `\` for a Windows host's.
fn separator_of(path: &str) -> char {
    if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    }
}

/// `path` ending in its separator, to go on typing a folder name after.
fn with_separator(path: &str) -> String {
    let separator = separator_of(path);
    let mut path = path.to_owned();
    if !path.ends_with(separator) {
        path.push(separator);
    }
    path
}

/// The typed path without the separators a folder chosen from the list leaves
/// at its end, unless that is all there is (a root).
fn trim_trailing_separators(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        path
    } else {
        trimmed
    }
}

fn directory_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
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
        counts.get(&source).map(|&count| {
            let tool = source.display_name();
            if count == 1 {
                crate::tr!("sidebar.recent_tool_threads_one", tool = tool).into_owned()
            } else {
                crate::tr!("sidebar.recent_tool_threads", tool = tool, count = count).into_owned()
            }
        })
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

#[cfg(test)]
mod tests {
    use std::future::Future as _;

    use gpui::{TestAppContext, VisualTestContext};
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    use super::*;
    use crate::store::WorkspaceAttachment;

    /// A remote client types a path that means nothing on its own machine. The
    /// answer — accept or reject, and why — comes from the host's filesystem,
    /// and the client shows the host's words rather than judging the string
    /// against its own path rules.
    #[gpui::test]
    fn a_remote_project_root_is_judged_by_the_host(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("add-project-remote");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let host = spawn_host(
            SessionStore::open_at(root.join("data")).unwrap(),
            HostServices::default(),
        )
        .expect("spawn add-project test host");
        let link = tcode_client::HostLink::new(host.to_host.clone(), host.from_host.clone());
        let mut pump = std::pin::pin!(link.pump());
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Remote {
                    host_id: "build-box".into(),
                    host_name: "build-box".into(),
                },
                None,
                None,
                cx,
            )
        });
        crate::store::tests::place_shown_threads(&store, cx);
        fn wait_until(
            cx: &mut TestAppContext,
            store: &Entity<WorkspaceStore>,
            mut pump: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
            description: &str,
            mut ready: impl FnMut(&TestAppContext) -> bool,
        ) {
            let deadline = crate::store::tests::host_deadline();
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                // Host replies arrive on an OS thread. Decode them here so ACKs
                // wake GPUI tasks on the test scheduler's own thread.
                let _ = pump.as_mut().poll(&mut task_cx);
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                cx.run_until_parked();
                if ready(cx) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiting for {description}"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        wait_until(cx, &store, pump.as_mut(), "host baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        // The dialog closes itself on success, which routes through the
        // window's overlay root — so mount it the way the app does.
        let built = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = built.clone();
        let store_for_view = store.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let dialog = cx.new(|cx| AddProjectDialog::new(store_for_view.clone(), window, cx));
            *capture.borrow_mut() = Some(dialog.clone());
            gpui_base::Root::new(dialog, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let dialog = built.borrow().clone().expect("dialog was built");

        let type_and_open = |cx: &mut VisualTestContext, path: &str| {
            let path = path.to_owned();
            cx.update(|window, cx| {
                dialog.update(cx, |dialog, cx| {
                    dialog
                        .path_input
                        .update(cx, |input, cx| input.set_value(path.clone(), window, cx));
                    dialog.open_typed_path(window, cx);
                });
            });
        };

        // A Windows-style path on a Unix host (or a nonexistent one on Windows):
        // rejected there, not here.
        type_and_open(cx, r"C:\Users\dev\src");
        wait_until(cx, &store, pump.as_mut(), "host path refusal", |cx| {
            dialog.read_with(cx, |dialog, _| dialog.error.is_some())
        });
        let error = dialog.read_with(cx, |dialog, _| dialog.error.clone());
        assert!(
            error.is_some_and(|error| error.contains(r"C:\Users\dev\src")),
            "the dialog must show the host's reason for refusing the path"
        );
        assert_eq!(
            smol::block_on(host.update_state_for_test(|state, _| state.projects.len()))
                .expect("read projects"),
            0,
            "a refused root must not create a project"
        );

        // The folder list under the field is the host's too: typing a host
        // directory and the start of a name lists the matching folders there.
        let typed = format!("{}/work", root.to_str().unwrap());
        cx.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog
                    .path_input
                    .update(cx, |input, cx| input.replace_all(typed.clone(), window, cx));
            });
        });
        wait_until(cx, &store, pump.as_mut(), "host folder listing", |cx| {
            dialog.read_with(cx, |dialog, _| {
                dialog
                    .folders
                    .listing
                    .as_ref()
                    .is_some_and(|listing| listing.directory == root)
            })
        });
        let shown = dialog.read_with(cx, |dialog, _| {
            let listing = dialog.folders.listing.as_ref().unwrap();
            matching_folders(&listing.entries, "work")
                .cloned()
                .collect::<Vec<_>>()
        });
        assert_eq!(shown, ["workspace"]);

        // A directory that exists on the host is accepted even though this
        // client never looked at it.
        type_and_open(cx, workspace.to_str().unwrap());
        wait_until(cx, &store, pump.as_mut(), "host project draft", |cx| {
            store.read_with(cx, |store, _| store.sole_thread().is_some())
        });
        assert_eq!(dialog.read_with(cx, |dialog, _| dialog.error.clone()), None);
        assert_eq!(
            smol::block_on(host.update_state_for_test(|state, _| {
                state
                    .projects
                    .iter()
                    .map(|project| project.root.clone())
                    .collect::<Vec<_>>()
            }))
            .expect("read projects"),
            vec![workspace.clone()]
        );

        let mut shutdown = std::pin::pin!(link.shutdown());
        wait_until(cx, &store, pump.as_mut(), "host shutdown", |_| {
            let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
            match shutdown.as_mut().poll(&mut task_cx) {
                std::task::Poll::Ready(result) => {
                    result.expect("stop host");
                    true
                }
                std::task::Poll::Pending => false,
            }
        });
        host.stopped.recv_blocking().expect("host stopped");
        let _ = std::fs::remove_dir_all(root);
    }
}
