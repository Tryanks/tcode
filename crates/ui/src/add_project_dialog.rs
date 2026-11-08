//! Add Project: pick, create or bring in a directory on the *host* as a
//! project, in one field.
//!
//! The field takes a host path; the list under it shows the folders of the
//! directory the path is in, narrowed by what follows its last separator. With
//! nothing typed it also offers a new project and the directories other tools
//! were recently active in, whose threads come along. One action is current at
//! a time — the highlighted row's, or else the typed path's — and the footer
//! says what it is.
//!
//! Everything here is host state reached over the pipe — the recents scan, the
//! folder listings, the project root and the import run. Nothing browses *this*
//! machine, so a local and a remote host pick a folder the same way.

use std::collections::HashMap;
use std::path::PathBuf;

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
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::time::{humanize_ago, now_secs};
use tcode_core::settings::ProjectStart;
use tcode_protocol::{
    CommandResponse, DirectoryListing, ExternalImportState, ExternalThread, RecentDir, SourceTool,
};

gpui::actions!(add_project, [CompleteFolder]);

/// The key context the field and list share, so Tab completes there.
const CONTEXT: &str = "AddProject";

/// Tab completes the field to a folder. The binding is in the dialog's own
/// context, so it is matched before the root's Tab focus step.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("tab", CompleteFolder, Some(CONTEXT))]);
}

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

const RECENT_LIMIT: usize = 15;
const ROW_HEIGHT: f32 = 32.;
/// The list keeps one height while it is filtered, so the dialog does not jump.
const LIST_HEIGHT: f32 = 340.;
/// What the dialog draws around the list: title, field, hints and footer.
const LIST_CHROME: f32 = 300.;

/// The host folders under the field: those of the directory the typed path is
/// in. With nothing typed, those of the start folder Settings choose.
#[derive(Default)]
struct FolderBrowser {
    /// The directory text last asked of the host.
    requested: Option<String>,
    /// The directory text `listing` answers. It differs from `requested` while
    /// a new listing is on its way, and when the start folder fell back to the
    /// home.
    answered: Option<String>,
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

/// One row of the list.
#[derive(Clone)]
enum Row {
    /// A directory other tools were active in; adding it imports its threads.
    Recent(RecentDir),
    /// The listed directory's parent, as text to go on typing in.
    Up(String),
    Folder {
        name: String,
        /// The folder as text to go on typing in, ending in a separator.
        target: String,
        project: bool,
    },
    /// Make the folder `name` inside `parent` and add it. `in_new_dir` when
    /// `parent` is where Create puts new projects.
    Create {
        parent: String,
        name: String,
        in_new_dir: bool,
    },
}

/// The rows, and where the list's captions go.
struct Rows {
    rows: Vec<Row>,
    /// The directory the folder rows are in, as the host writes it.
    directory: Option<String>,
    /// The first folder row, under the Folders caption.
    folders_from: usize,
    /// The first recent row, under the Recent caption.
    recents_from: Option<usize>,
    /// The typed name, the part after the last separator.
    filter: String,
    /// The listed directory is the new-project folder, which does not exist yet.
    new_folder_missing: bool,
}

/// What the primary button and Ctrl+Enter do.
enum Primary {
    Add {
        root: String,
        project: bool,
    },
    Recent(RecentDir),
    /// `parent: None` is where Create puts new projects, as the host knows it.
    Create {
        parent: Option<String>,
        name: String,
    },
    Nothing,
}

pub(super) struct AddProjectDialog {
    store: Entity<WorkspaceStore>,
    path_input: Entity<InputState>,
    recent: RecentState,
    /// The last failure to show under the field. Host-authored where the host
    /// produced it, so the user reads the host's own reason.
    error: Option<String>,
    pending_created_project: Option<String>,
    folders: FolderBrowser,
    /// The row the arrow keys or a click put the highlight on. `None` while the
    /// highlight follows what is typed.
    highlight: Option<usize>,
    list_scroll: ScrollHandle,
    _subscriptions: Vec<gpui::Subscription>,
}

pub(super) fn open(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| AddProjectDialog::new(store, window, cx));
    dialog.update(cx, |dialog, cx| dialog.scan(cx));
    let content = dialog.clone();
    window.open_dialog(cx, move |builder, _, cx| {
        let dialog_content = content.clone();
        let confirm = content.clone();
        builder
            .wide()
            .title(crate::tr!("sidebar.add_project").into_owned())
            .content(move |content_el, _, _| content_el.child(dialog_content.clone()))
            .footer(render_add_footer(&content, cx))
            .on_ok(move |_, window, cx| {
                let secondary = window.modifiers().secondary(cx);
                confirm.update(cx, |dialog, cx| dialog.confirm(secondary, window, cx));
                // The host result owns dismissal, including validation failures.
                false
            })
    });
}

impl AddProjectDialog {
    fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = match store.read(cx).remote_host_name() {
            Some(host) => {
                crate::tr!("sidebar.add_project_placeholder_host", host = host).into_owned()
            }
            None => crate::tr!("sidebar.add_project_placeholder").into_owned(),
        };
        let path_input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscriptions = vec![
            cx.subscribe(&path_input, |dialog, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    // The highlight follows the typing again.
                    dialog.highlight = None;
                    dialog.error = None;
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
            pending_created_project: None,
            folders: FolderBrowser::default(),
            highlight: None,
            list_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
            store,
            path_input,
            recent: RecentState::Loading,
            error: None,
        };
        dialog.refresh_folders(cx);
        dialog
    }

    fn typed(&self, cx: &App) -> String {
        self.path_input.read(cx).value().to_string()
    }

    /// The typed path as the host will read it: a relative one is inside the
    /// folder Create puts new projects in, so a bare name finds or makes a
    /// project there.
    fn path(&self, cx: &App) -> String {
        let typed = self.typed(cx);
        let trimmed = typed.trim_start();
        if trimmed.is_empty() || is_absolute(trimmed) {
            return typed;
        }
        let new_dir = self.store.read(cx).settings_ref().new_project_dir();
        format!("{}{trimmed}", with_separator(new_dir))
    }

    /// Where the folder list starts when nothing is typed, as Settings choose.
    fn start_directory(&self, cx: &App) -> String {
        self.store
            .read(cx)
            .settings_ref()
            .project_start_dir()
            .to_owned()
    }

    /// Whether `directory` is where Create puts new projects, which exists
    /// only once one has been created there.
    fn is_new_project_dir(&self, directory: &str, cx: &App) -> bool {
        let new_dir = self.store.read(cx).settings_ref().new_project_dir();
        trim_trailing_separators(directory) == trim_trailing_separators(new_dir)
    }

    /// Ask the host for the folders of the directory the typed path is in,
    /// unless that is the directory already asked for.
    fn refresh_folders(&mut self, cx: &mut Context<Self>) {
        let typed = self.typed(cx);
        let (directory, _) = browse_target(&self.path(cx), &self.start_directory(cx));
        if self.folders.requested.as_ref() == Some(&directory) {
            return;
        }
        self.folders.requested = Some(directory.clone());
        self.folders.generation += 1;
        // A start folder that is the missing new-project folder starts at the
        // home instead.
        let fall_back_home = typed.trim().is_empty()
            && self.store.read(cx).settings_ref().project_start == ProjectStart::NewProjectDir
            && self.is_new_project_dir(&directory, cx);
        self.list_folders(directory, fall_back_home, cx);
    }

    fn list_folders(&mut self, directory: String, fall_back_home: bool, cx: &mut Context<Self>) {
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
                        dialog.folders.answered = Some(directory);
                        dialog.folders.error = None;
                    }
                    Err(_) if fall_back_home => dialog.list_folders("~".to_owned(), false, cx),
                    Err(error) => {
                        dialog.folders.listing = None;
                        dialog.folders.answered = None;
                        dialog.folders.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Put `path` in the field, as a directory to go on typing in.
    fn enter_folder(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.highlight = None;
        self.path_input.update(cx, |input, cx| {
            input.replace_all(path, window, cx);
            input.focus(window, cx);
        });
        self.refresh_folders(cx);
        self.list_scroll.scroll_to_item(0);
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

    /// The list for what is typed now.
    fn rows(&self, cx: &App) -> Rows {
        let typed = self.typed(cx);
        let (directory, filter) = browse_target(&self.path(cx), &self.start_directory(cx));
        let mut rows = Vec::new();
        let mut recents_from = None;
        if typed.trim().is_empty()
            && let RecentState::Ready(recent) = &self.recent
            && !recent.is_empty()
        {
            recents_from = Some(rows.len());
            rows.extend(recent.iter().take(RECENT_LIMIT).cloned().map(Row::Recent));
        }
        let folders_from = rows.len();
        let current = self.folders.answered.as_deref() == Some(directory.as_str());
        let mut exact = false;
        let mut shown = None;
        // A listing on its way leaves the last one up, so the list does not
        // blank between keystrokes.
        if let Some(listing) = &self.folders.listing {
            let listed = with_separator(&listing.directory.to_string_lossy());
            let separator = separator_of(&listed);
            let projects = self.store.read(cx).projects();
            if let Some(parent) = &listing.parent {
                rows.push(Row::Up(with_separator(&parent.to_string_lossy())));
            }
            for name in matching_folders(&listing.entries, &filter) {
                let target = format!("{listed}{name}{separator}");
                let root = PathBuf::from(trim_trailing_separators(&target));
                rows.push(Row::Folder {
                    name: name.clone(),
                    project: projects.iter().any(|project| project.root == root),
                    target,
                });
            }
            exact = current && listing.entries.iter().any(|name| *name == filter.trim());
            shown = Some(listed);
        }
        let new_folder_missing =
            self.folders.error.is_some() && self.is_new_project_dir(&directory, cx);
        let name = filter.trim();
        if is_folder_name(name) && ((current && !exact) || new_folder_missing) {
            rows.push(Row::Create {
                in_new_dir: self.is_new_project_dir(&directory, cx),
                parent: directory.clone(),
                name: name.to_owned(),
            });
        }
        Rows {
            rows,
            directory: shown.or(new_folder_missing.then_some(directory)),
            folders_from,
            recents_from,
            filter,
            new_folder_missing,
        }
    }

    /// The highlighted row: the one the user moved to, or, while a name is
    /// typed, the first folder it matches, else the folder it would create.
    fn highlighted(&self, rows: &Rows) -> Option<usize> {
        if let Some(index) = self.highlight {
            return (index < rows.rows.len()).then_some(index);
        }
        if rows.filter.trim().is_empty() {
            return None;
        }
        rows.rows
            .iter()
            .position(|row| matches!(row, Row::Folder { .. }))
            .or_else(|| {
                rows.rows
                    .iter()
                    .position(|row| matches!(row, Row::Create { .. }))
            })
    }

    fn primary(&self, rows: &Rows, highlighted: Option<usize>, cx: &App) -> Primary {
        match highlighted.and_then(|index| rows.rows.get(index)) {
            Some(Row::Recent(recent)) => return Primary::Recent(recent.clone()),
            Some(Row::Create {
                parent,
                name,
                in_new_dir,
            }) => {
                return Primary::Create {
                    parent: (!in_new_dir).then(|| parent.clone()),
                    name: name.clone(),
                };
            }
            Some(Row::Folder {
                target, project, ..
            }) => {
                return Primary::Add {
                    root: trim_trailing_separators(target).to_owned(),
                    project: *project,
                };
            }
            Some(Row::Up(_)) | None => {}
        }
        // Otherwise the typed path, whatever this client makes of it: whether
        // it is a directory is the host's to say.
        let typed = self.path(cx);
        let typed = typed.trim();
        if typed.is_empty() {
            return Primary::Nothing;
        }
        let project = rows.filter.is_empty()
            && self
                .folders
                .listing
                .as_ref()
                .filter(|_| {
                    self.folders.answered.as_deref() == Some(browse_target(typed, "").0.as_str())
                })
                .is_some_and(|listing| {
                    let root = &listing.directory;
                    self.store
                        .read(cx)
                        .projects()
                        .iter()
                        .any(|project| project.root == *root)
                });
        Primary::Add {
            root: trim_trailing_separators(typed).to_owned(),
            project,
        }
    }

    /// Enter: go into the highlighted folder, or do what the footer says.
    /// With the secondary modifier, always do what the footer says.
    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let highlighted = self.highlighted(&rows);
        if !secondary {
            match highlighted.and_then(|index| rows.rows.get(index)) {
                Some(Row::Up(target)) | Some(Row::Folder { target, .. }) => {
                    let target = target.clone();
                    self.enter_folder(target, window, cx);
                    return;
                }
                _ => {}
            }
        }
        let primary = self.primary(&rows, highlighted, cx);
        self.run(primary, window, cx);
    }

    fn run(&mut self, primary: Primary, window: &mut Window, cx: &mut Context<Self>) {
        match primary {
            Primary::Add { root, .. } => self.create_draft(PathBuf::from(root), window, cx),
            Primary::Recent(recent) => self.choose_recent(recent, window, cx),
            Primary::Create { parent, name } => self.create_new_project(name, parent, window, cx),
            Primary::Nothing => {}
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let total = rows.rows.len();
        let current = self.highlighted(&rows);
        match event.keystroke.key.as_str() {
            "down" if total > 0 => {
                let next = current.map_or(0, |index| (index + 1).min(total - 1));
                self.move_highlight(next, &rows, cx);
            }
            "up" => {
                if let Some(index) = current.filter(|index| *index > 0) {
                    self.move_highlight(index - 1, &rows, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Tab: complete to the highlighted folder, or the first one. With no
    /// folder to complete to, Tab moves focus on as everywhere else.
    fn complete_folder(&mut self, _: &CompleteFolder, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let target = self
            .highlighted(&rows)
            .and_then(|index| rows.rows.get(index))
            .filter(|row| matches!(row, Row::Folder { .. }))
            .or_else(|| {
                rows.rows
                    .iter()
                    .find(|row| matches!(row, Row::Folder { .. }))
            });
        if let Some(Row::Folder { target, .. }) = target {
            let target = target.clone();
            self.enter_folder(target, window, cx);
        } else if let Ok(step) = cx.build_action("root::Tab", None) {
            window.dispatch_action(step, cx);
        }
    }

    fn move_highlight(&mut self, index: usize, rows: &Rows, cx: &mut Context<Self>) {
        self.highlight = Some(index);
        self.list_scroll.scroll_to_item(child_index(rows, index));
        cx.notify();
    }

    fn click_row(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.rows(cx);
        let Some(row) = rows.rows.get(index).cloned() else {
            return;
        };
        match row {
            Row::Up(target) | Row::Folder { target, .. } => self.enter_folder(target, window, cx),
            Row::Create { .. } => {
                let primary = self.primary(&rows, Some(index), cx);
                self.run(primary, window, cx);
            }
            // Adding one imports its threads too, weightier than a move: a
            // click picks it and the footer says what it will do; a double
            // click does it.
            Row::Recent(_) => {
                self.highlight = Some(index);
                if event.click_count() >= 2 {
                    let primary = self.primary(&rows, Some(index), cx);
                    self.run(primary, window, cx);
                }
                cx.notify();
            }
        }
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

    fn create_new_project(
        &mut self,
        name: String,
        parent: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let create = self
            .store
            .update(cx, |store, cx| store.create_new_project(name, parent, cx));
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

    /// Send the typed text to the host as-is. Whether it is absolute and whether
    /// it is a directory are facts about the host's filesystem and path rules,
    /// so the host decides and its answer is what the user reads.
    #[cfg(test)]
    fn open_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.path(cx);
        let typed = trim_trailing_separators(typed.trim()).to_owned();
        if !typed.is_empty() {
            self.create_draft(PathBuf::from(typed), window, cx);
        }
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

    fn render_row(
        &self,
        index: usize,
        row: &Row,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (icon, label, trailing, accent): (IconName, AnyElement, Option<AnyElement>, bool) =
            match row {
                Row::Recent(recent) => {
                    let name = recent
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| recent.path.display().to_string());
                    let ago = humanize_ago(now_secs().saturating_sub(recent.last_active_ms / 1000));
                    let count = recent.threads.len();
                    let tag = if count == 1 {
                        crate::tr!("sidebar.recent_import_count_one").into_owned()
                    } else {
                        crate::tr!("sidebar.recent_import_count", count = count).into_owned()
                    };
                    (
                        IconName::History,
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(div().flex_none().font_bold().child(name))
                            .child(
                                div()
                                    .min_w_0()
                                    .text_secondary(false, cx)
                                    .overflow_hidden()
                                    .child(crate::widgets::fade_text(
                                        recent.path.display().to_string(),
                                    )),
                            )
                            .into_any_element(),
                        Some(
                            h_flex()
                                .gap_2()
                                .child(tag_chip(tag, cx))
                                .child(div().text_secondary(false, cx).child(ago))
                                .into_any_element(),
                        ),
                        false,
                    )
                }
                Row::Up(_) => (
                    IconName::CornerLeftUp,
                    div().child("..").into_any_element(),
                    None,
                    false,
                ),
                Row::Folder { name, project, .. } => (
                    IconName::Folder,
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(name.clone()))
                        .into_any_element(),
                    project.then(|| {
                        tag_chip(crate::tr!("sidebar.already_project").into_owned(), cx)
                            .into_any_element()
                    }),
                    false,
                ),
                Row::Create {
                    parent,
                    name,
                    in_new_dir,
                } => {
                    let label = if *in_new_dir {
                        crate::tr!(
                            "sidebar.create_in_folder_row",
                            path = trim_trailing_separators(parent).to_owned(),
                            name = name.clone()
                        )
                    } else {
                        crate::tr!("sidebar.create_here_row", name = name.clone())
                    };
                    (
                        IconName::Plus,
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(label.into_owned()))
                            .into_any_element(),
                        None,
                        true,
                    )
                }
            };
        let fill = if highlighted {
            crate::material::ItemFill::Highlight(cx.theme().list_active)
        } else {
            crate::material::ItemFill::Hover
        };
        crate::material::menu_item_frame(false, fill, cx)
            .id(("add-project-row", index))
            .role(gpui::Role::ListBoxOption)
            .aria_selected(highlighted)
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_none()
            .items_center()
            .cursor_pointer()
            .on_click(cx.listener(move |dialog, event, window, cx| {
                dialog.click_row(index, event, window, cx);
            }))
            .child(Icon::new(icon).text_color(if accent { cx.theme().primary } else { muted }))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_body(false, cx)
                    .overflow_hidden()
                    .when(accent, |label| label.text_color(cx.theme().primary))
                    .child(label),
            )
            .children(trailing.map(|trailing| div().flex_none().child(trailing)))
            .into_any_element()
    }
}

impl Render for AddProjectDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.store.read(cx).remote_host_name().map(str::to_owned);
        let rows = self.rows(cx);
        let highlighted = self.highlighted(&rows);
        let typed = self.typed(cx);
        let mut children: Vec<AnyElement> = Vec::new();
        for (index, row) in rows.rows.iter().enumerate() {
            if rows.recents_from == Some(index) {
                let label = match &host {
                    Some(host) => {
                        crate::tr!("sidebar.recent_activity_host", host = host).into_owned()
                    }
                    None => crate::tr!("sidebar.recent_activity").into_owned(),
                };
                children.push(caption(
                    label,
                    Some(crate::tr!("sidebar.recent_section_hint").into_owned()),
                    cx,
                ));
            }
            if index == rows.folders_from {
                children.push(caption(
                    crate::tr!("sidebar.folders").into_owned(),
                    rows.directory.clone(),
                    cx,
                ));
            }
            children.push(self.render_row(index, row, highlighted == Some(index), cx));
        }
        if rows.folders_from == rows.rows.len() {
            children.push(caption(
                crate::tr!("sidebar.folders").into_owned(),
                rows.directory.clone(),
                cx,
            ));
        }
        // Notes the rows cannot carry.
        let note = if rows.new_folder_missing {
            Some((crate::tr!("sidebar.new_folder_missing").into_owned(), false))
        } else if let Some(error) = &self.folders.error {
            Some((
                crate::tr!("sidebar.folders_failed", reason = error.clone()).into_owned(),
                true,
            ))
        } else if typed.trim().is_empty() {
            match &self.recent {
                RecentState::Failed(error) => Some((
                    crate::tr!("sidebar.recent_failed", reason = error.clone()).into_owned(),
                    true,
                )),
                _ => None,
            }
        } else {
            None
        };
        if let Some((text, danger)) = note {
            children.push(
                div()
                    .px_2()
                    .py_1()
                    .text_secondary(false, cx)
                    .when(danger, |note| note.text_color(cx.theme().danger))
                    .child(text)
                    .into_any_element(),
            );
        }
        let loading_recents =
            typed.trim().is_empty() && matches!(self.recent, RecentState::Loading);
        let list_height =
            fit_viewport(LIST_HEIGHT, window.viewport_size().height - px(LIST_CHROME));
        let path_hint = host
            .as_ref()
            .map(|host| crate::tr!("sidebar.host_path_hint", host = host).into_owned());
        let modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        v_flex()
            .id("add-project")
            .gap_2()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::complete_folder))
            // The arrows move the highlight before the field sees them.
            .capture_key_down(cx.listener(Self::on_key_down))
            .child(Input::new(&self.path_input).w_full())
            .when_some(path_hint, |column, hint| {
                column.child(div().text_secondary(false, cx).child(hint))
            })
            .children(
                crate::motion::shown("add-project-error", self.error.clone(), SHOW, window, cx)
                    .map(|error| {
                        error.fold(
                            div()
                                .text_secondary(false, cx)
                                .text_color(cx.theme().danger)
                                .child(error.value.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .id("add-project-list")
                    .role(gpui::Role::ListBox)
                    .aria_label(crate::tr!("sidebar.add_project").into_owned())
                    .w_full()
                    .h(list_height)
                    .overflow_y_scroll_area()
                    .track_scroll(&self.list_scroll)
                    .children(children),
            )
            .when(loading_recents, |column| {
                column.child(Progress::new("recent-directories-loading").loading(true))
            })
            .child(
                div().text_secondary(false, cx).child(
                    crate::tr!("sidebar.add_project_keys", modifier = modifier).into_owned(),
                ),
            )
    }
}

/// The list child a row is drawn as: captions come before some rows.
fn child_index(rows: &Rows, index: usize) -> usize {
    let mut child = index;
    if rows.recents_from.is_some_and(|from| index >= from) {
        child += 1;
    }
    if index >= rows.folders_from {
        child += 1;
    }
    child
}

fn caption(label: String, detail: Option<String>, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .flex_none()
        .gap_2()
        .px_2()
        .pt_2()
        .pb_1()
        .child(div().flex_none().text_caption(cx).child(label))
        .when_some(detail, |row, detail| {
            row.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_secondary(false, cx)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(detail)),
            )
        })
        .into_any_element()
}

fn tag_chip(text: String, cx: &App) -> gpui::Div {
    div()
        .px_2()
        .rounded(crate::material::radius_chip(cx))
        .bg(cx.theme().primary.opacity(0.1))
        .text_color(cx.theme().primary)
        .text_secondary(false, cx)
        .child(text)
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

fn render_add_footer(dialog: &Entity<AddProjectDialog>, cx: &App) -> AnyElement {
    let this = dialog.read(cx);
    let rows = this.rows(cx);
    let highlighted = this.highlighted(&rows);
    let primary = this.primary(&rows, highlighted, cx);
    let (summary, label, enabled): (String, SharedString, bool) = match &primary {
        Primary::Add {
            root,
            project: true,
        } => (
            crate::tr!("sidebar.summary_open", path = root.clone()).into_owned(),
            crate::tr!("sidebar.open").into_owned().into(),
            true,
        ),
        Primary::Add {
            root,
            project: false,
        } => (
            crate::tr!("sidebar.summary_add", path = root.clone()).into_owned(),
            crate::tr!("sidebar.action_add").into_owned().into(),
            true,
        ),
        Primary::Recent(recent) => (
            crate::tr!(
                "sidebar.summary_recent",
                path = recent.path.display().to_string(),
                count = recent.threads.len(),
                tools = tool_counts(&recent.threads)
            )
            .into_owned(),
            crate::tr!("sidebar.action_recent").into_owned().into(),
            true,
        ),
        Primary::Create { parent, name } => (
            crate::tr!(
                "sidebar.summary_create",
                path = format!(
                    "{}{name}",
                    with_separator(parent.as_deref().unwrap_or_else(|| {
                        this.store.read(cx).settings_ref().new_project_dir()
                    }))
                )
            )
            .into_owned(),
            crate::tr!("sidebar.action_create").into_owned().into(),
            true,
        ),
        Primary::Nothing => (
            crate::tr!("sidebar.summary_empty").into_owned(),
            crate::tr!("sidebar.action_add").into_owned().into(),
            false,
        ),
    };
    let go = dialog.clone();
    h_flex()
        .w_full()
        .gap_3()
        .items_center()
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_secondary(false, cx)
                .overflow_hidden()
                .child(crate::widgets::fade_text(summary)),
        )
        .child(
            DialogActions::new()
                .child(
                    Button::new("add-project-cancel")
                        .label(crate::tr!("sidebar.cancel"))
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                        }),
                )
                .child(
                    Button::new("add-project-go")
                        .primary()
                        .label(label)
                        .disabled(!enabled)
                        .on_click(move |_, window, cx| {
                            go.update(cx, |dialog, cx| {
                                let rows = dialog.rows(cx);
                                let highlighted = dialog.highlighted(&rows);
                                let primary = dialog.primary(&rows, highlighted, cx);
                                dialog.run(primary, window, cx);
                            });
                        }),
                ),
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

/// Whether `path` names a place on its own rather than inside the folder Create
/// puts new projects in: from a root, the home (`~`), or a Windows drive.
fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(['/', '\\', '~'])
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// Whether `name` can be one new folder: not empty, no separator, not `.` or `..`.
fn is_folder_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
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
