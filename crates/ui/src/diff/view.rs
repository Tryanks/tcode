//! The right-side diff panel view: scope controls, virtualized unified/split
//! lists, expandable gaps, and line-anchored review comments.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::highlight::HighlightTheme;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::Popover;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::tabs::{TabBar, TabItem};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::FileChange;
use gpui::{
    Action, AnyElement, App, AppContext as _, AvailableSpace, Context, Entity, HighlightStyle,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Role,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_base::{PopoverState, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;

use super::list::{
    DiffList, DiffListHost, DiffSelectionMenu, FileMenu, LineSelection, file_header_index,
    render_list, render_notice,
};
use super::model::{
    DiffColors, ExpandDir, FileDiffInput, RenderedFile, build_file, reconstruct_from_text,
};
use super::parse::{RowKind, is_binary_patch};
use crate::agents_panel::{Agents, AgentsPanel};
use crate::plan_panel::PlanPanel;
use crate::store::{StoreChange, WorkspaceStore};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_caption;
use crate::window_state::WindowState;
use crate::workspace_walk::relativize_to_workspace;
use crate::{highlight, material};
use tcode_core::{
    session::{ReviewComment, ReviewSide},
    ui::RightTab,
};
use tcode_protocol::{GitDiffResult, GitDiffScope, GitFileText};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// The widest the chip naming an agent whose panel is shown grows.
const SUBJECT_WIDEST: f32 = 180.;

/// A diff view toggle. The compact toolbar reaches these through its overflow
/// menu, which addresses items by action rather than by callback.
#[derive(Action, Clone, Copy, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_diff, no_json)]
enum DiffViewOption {
    Split,
    Wrap,
    Whitespace,
    Invisibles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DiffScope {
    Turn(usize),
    WorkingTree,
    Branch,
}

#[derive(Clone, Copy)]
struct DiffOptions {
    ignore_ws: bool,
    show_invisibles: bool,
}

struct RenderFileContext<'a> {
    cwd: &'a Path,
    options: DiffOptions,
    theme: &'a HighlightTheme,
    colors: &'a DiffColors,
    whitespace_style: &'a HighlightStyle,
}

fn render_file(
    change: &FileChange,
    texts: Option<&GitFileText>,
    fallback_new_text: Option<&str>,
    context: RenderFileContext<'_>,
) -> RenderedFile {
    let needs_reconstruction = texts.is_none_or(|texts| texts.new.is_none());
    let reconstructed = needs_reconstruction
        .then(|| {
            change.diff.as_deref().and_then(|patch| {
                fallback_new_text
                    .map(str::to_string)
                    .and_then(|text| reconstruct_from_text(text, patch))
            })
        })
        .flatten();
    let old_text = texts
        .and_then(|texts| texts.old.as_deref())
        .or_else(|| reconstructed.as_ref().map(|(old, _)| old.as_str()));
    let new_text = texts
        .and_then(|texts| texts.new.as_deref())
        .or_else(|| reconstructed.as_ref().map(|(_, new)| new.as_str()));

    build_file(
        &FileDiffInput {
            path: &change.path,
            kind: change.kind,
            old_text,
            new_text,
            patch: change.diff.as_deref(),
            ignore_whitespace: context.options.ignore_ws,
            show_invisibles: context.options.show_invisibles,
        },
        relativize_to_workspace(&change.path, context.cwd),
        highlight::language_name_for_path(&change.path),
        context.theme,
        context.colors,
        context.whitespace_style,
    )
}

/// Cache of rendered files, invalidated when the session, selected turn, or
/// theme brightness changes (highlight colors are theme-resolved).
struct DiffCache {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
    ignore_ws: bool,
    show_invisibles: bool,
    list: DiffList,
    fallback_texts: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
    ignore_ws: bool,
    show_invisibles: bool,
}

struct RenderAppearance {
    theme: Arc<HighlightTheme>,
    colors: DiffColors,
    whitespace_style: HighlightStyle,
}

impl RenderAppearance {
    /// The colours a rendered file bakes into its text, from the shown
    /// mode's palette at rest: the file is kept until the mode changes
    /// again, so colours sampled while the theme glides would stay.
    fn of(cx: &App) -> Self {
        let theme = crate::theme::palette(cx.theme().mode, cx);
        Self {
            theme: theme.highlight_theme.clone(),
            colors: DiffColors {
                added_word_bg: theme.success.opacity(0.30),
                removed_word_bg: theme.danger.opacity(0.28),
            },
            whitespace_style: HighlightStyle {
                color: Some(theme.muted_foreground),
                ..Default::default()
            },
        }
    }
}

struct GitPreview {
    session: String,
    scope: DiffScope,
    base: Option<String>,
    revision: u64,
    ignore_ws: bool,
    result: GitDiffResult,
}

#[derive(Clone, Copy)]
struct GitPreviewOptions {
    revision: u64,
    ignore_ws: bool,
}

pub struct DiffPanel {
    workspace_store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// The thread the drawer shows, which the pane tells it.
    session_id: Option<String>,
    /// The thread the pane shows, where the panel shows an agent beside it.
    lead: Option<String>,
    /// Whether the panel's pane reaches the window's top right corner.
    at_window_corner: bool,
    /// The Plan/Tasks tab content (the other tab in this right panel).
    plan: Entity<PlanPanel>,
    pull_requests: Entity<crate::pull_requests::PullRequestsPanel>,
    agents: Entity<AgentsPanel>,
    ignore_ws: bool,
    show_invisibles: bool,
    scopes: HashMap<String, DiffScope>,
    bases: HashMap<String, String>,
    cache: Option<DiffCache>,
    git_preview: Option<GitPreview>,
    loading_key: Option<(String, DiffScope, Option<String>, u64, bool)>,
    render_loading_key: Option<RenderKey>,
    comment_input: Option<Entity<InputState>>,
    observed_review_comments: Vec<ReviewComment>,
    /// The panel's size-class motion: the window draws the panel as a wide
    /// tab or a compact page, under different parents.
    motion: crate::motion::owned::Owned,
    /// Whether the tab strip last measured too narrow for its labels; only
    /// its layout knows, and its prepaint writes it here.
    tab_icons: Rc<std::cell::Cell<Option<bool>>>,
    _subscriptions: Vec<Subscription>,
}

impl DiffPanel {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        session_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pull_requests = cx.new(|cx| {
            crate::pull_requests::PullRequestsPanel::new(
                workspace_store.clone(),
                window_state.clone(),
                session_id.clone(),
                cx,
            )
        });
        let plan = cx.new(|_| PlanPanel::new(workspace_store.clone(), session_id.clone()));
        let agents = cx.new(|cx| AgentsPanel::new(workspace_store.clone(), session_id.clone(), cx));
        let subscriptions = vec![
            cx.observe(&workspace_store, |this, _, cx| {
                this.sync_agents_shown(cx);
                this.sync_review_comments(cx);
                cx.notify();
            }),
            // A status replaced notifies only its thread's readers, and the
            // drafts it carries are compared here, not as the panel renders.
            cx.subscribe(&workspace_store, |this, _, change: &StoreChange, cx| {
                if change.topic == crate::store::TopicKind::SessionStatus {
                    this.sync_review_comments(cx);
                }
            }),
        ];
        let mut panel = Self {
            workspace_store,
            window_state,
            session_id,
            lead: None,
            at_window_corner: false,
            plan,
            pull_requests,
            agents,
            ignore_ws: false,
            show_invisibles: false,
            scopes: HashMap::new(),
            bases: HashMap::new(),
            cache: None,
            git_preview: None,
            loading_key: None,
            render_loading_key: None,
            comment_input: None,
            observed_review_comments: Vec::new(),
            motion: Default::default(),
            tab_icons: Default::default(),
            _subscriptions: subscriptions,
        };
        panel.sync_agents_shown(cx);
        panel.sync_review_comments(cx);
        panel
    }

    /// Hold the thread's review comments, which every row the panel draws
    /// reads, and lay the rows out again when they change.
    fn sync_review_comments(&mut self, cx: &App) {
        let comments = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .map_or(&[][..], |status| &status.review_comment_drafts);
        if self.observed_review_comments != comments {
            self.observed_review_comments = comments.to_vec();
            self.remeasure_lists();
        }
    }

    /// Show the tabs of `session_id`'s thread, or none.
    pub(crate) fn set_session(this: &Entity<Self>, session_id: Option<String>, cx: &mut App) {
        if this.read(cx).session_id == session_id {
            return;
        }
        this.update(cx, |panel, cx| {
            panel.session_id = session_id.clone();
            panel.sync_review_comments(cx);
            panel
                .plan
                .update(cx, |plan, cx| plan.set_session(session_id.clone(), cx));
            panel.pull_requests.update(cx, |pull_requests, cx| {
                pull_requests.set_session(session_id.clone(), cx)
            });
            panel
                .agents
                .update(cx, |agents, cx| agents.set_session(session_id, cx));
            panel.sync_agents_shown(cx);
            cx.notify();
        });
    }

    pub(crate) fn set_lead(this: &Entity<Self>, lead: Option<String>, cx: &mut App) {
        if this.read(cx).lead != lead {
            this.update(cx, |panel, cx| {
                panel.lead = lead;
                cx.notify();
            });
        }
    }

    pub(crate) fn set_at_window_corner(this: &Entity<Self>, at_corner: bool, cx: &mut App) {
        if this.read(cx).at_window_corner != at_corner {
            this.update(cx, |panel, cx| {
                panel.at_window_corner = at_corner;
                cx.notify();
            });
        }
    }

    fn sync_agents_shown(&mut self, cx: &mut Context<Self>) {
        let panel = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        self.agents.update(cx, |agents, cx| {
            agents.set_shown(
                panel.right_panel_open && panel.right_tab == RightTab::Agents,
                cx,
            )
        });
    }

    /// Keep the panel's motion in `scene`, the right panel's: what it shows
    /// is what the tabs show.
    pub(crate) fn share_scene(&mut self, scene: &crate::motion::Epoch, cx: &mut Context<Self>) {
        self.motion = crate::motion::owned::Owned::in_scene(scene);
        self.pull_requests
            .update(cx, |pull_requests, cx| pull_requests.share_scene(scene, cx));
    }

    /// The held turn `turn`'s number in the whole thread: the turns of its
    /// log before the held ones count too.
    fn turn_number(&self, session: &str, turn: usize, cx: &App) -> usize {
        self.workspace_store
            .read(cx)
            .absolute_turn_for(Some(session), turn)
            + 1
    }

    /// Let go of what was built to draw the diffs — the host's answer, the
    /// rendered and highlighted files — and of the pull requests' rendered
    /// pages, once the right panel has taken the panel down. The scope, base
    /// and options chosen, and a comment being written, stay; the next draw
    /// reads and renders again. A read or render under way lands nowhere:
    /// its key is no longer the one awaited.
    pub(crate) fn release(&mut self, cx: &mut Context<Self>) {
        self.cache = None;
        self.git_preview = None;
        self.loading_key = None;
        self.render_loading_key = None;
        self.pull_requests
            .update(cx, |pull_requests, cx| pull_requests.release(cx));
    }

    fn selected_scope(&self, session: &str, cx: &App) -> Option<DiffScope> {
        self.scopes
            .get(session)
            .copied()
            .or_else(|| {
                self.workspace_store
                    .read(cx)
                    .diff_selected_turn_for(self.session_id.as_deref(), cx)
                    .map(DiffScope::Turn)
            })
            .or(Some(DiffScope::WorkingTree))
    }

    fn remeasure_lists(&self) {
        if let Some(cache) = &self.cache {
            cache.list.remeasure();
        }
    }

    fn apply_pending_file_focus(
        &mut self,
        session: &str,
        scope: DiffScope,
        cx: &mut Context<Self>,
    ) {
        let DiffScope::Turn(turn) = scope else {
            return;
        };
        let (request, cwd) = {
            let store = self.workspace_store.read(cx);
            let request = store
                .pending_diff_focus_for(self.session_id.as_deref())
                .filter(|request| request.session == session && request.turn == turn);
            let cwd = store
                .diff_active_state_for(self.session_id.as_deref(), cx)
                .filter(|active| active.session == session)
                .map(|active| active.cwd);
            (request, cwd)
        };
        let (Some(request), Some(cwd)) = (request, cwd) else {
            return;
        };
        let Some(cache) = self
            .cache
            .as_ref()
            .filter(|cache| cache.session == session && cache.scope == scope)
        else {
            return;
        };
        let list = &cache.list;
        if let Some(path) = &request.path {
            if let Some(index) = file_header_index(&list.files, &list.unified_items, path, &cwd) {
                list.glide_to(false, index, cx);
            }
            if let Some(index) = file_header_index(&list.files, &list.split_items, path, &cwd) {
                list.glide_to(true, index, cx);
            }
        }
        self.workspace_store.update(cx, |store, _cx| {
            store.take_diff_focus_for(self.session_id.as_deref(), session, turn);
        });
    }

    fn request_git_preview(
        &mut self,
        session: String,
        cwd: PathBuf,
        scope: DiffScope,
        base: Option<String>,
        options: GitPreviewOptions,
        cx: &mut Context<Self>,
    ) {
        let runtime_scope = match scope {
            DiffScope::WorkingTree => GitDiffScope::WorkingTree,
            DiffScope::Branch => GitDiffScope::Branch,
            DiffScope::Turn(_) => return,
        };
        let key = (
            session.clone(),
            scope,
            base.clone(),
            options.revision,
            options.ignore_ws,
        );
        if self.loading_key.as_ref() == Some(&key)
            || self.git_preview.as_ref().is_some_and(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == options.revision
                    && preview.ignore_ws == options.ignore_ws
            })
        {
            return;
        }
        self.loading_key = Some(key.clone());
        let workspace_store = self.workspace_store.clone();
        cx.spawn(async move |this, cx| {
            let result = workspace_store
                .update(cx, |store, cx| {
                    store.load_git_diff(&cwd, runtime_scope, base.as_deref(), options.ignore_ws, cx)
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.loading_key.as_ref() == Some(&key) {
                    panel.git_preview = Some(GitPreview {
                        session,
                        scope,
                        base: key.2.clone(),
                        revision: options.revision,
                        ignore_ws: options.ignore_ws,
                        result,
                    });
                    panel.loading_key = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn request_rendered_files(
        &mut self,
        key: RenderKey,
        changes: Vec<FileChange>,
        texts: Vec<GitFileText>,
        cwd: PathBuf,
        appearance: RenderAppearance,
        cx: &mut Context<Self>,
    ) {
        if self.render_loading_key.as_ref() == Some(&key) {
            return;
        }
        let reusable = self
            .cache
            .as_ref()
            .filter(|cache| {
                cache.session == key.session
                    && cache.scope == key.scope
                    && cache.revision == key.revision
            })
            .map(|cache| cache.fallback_texts.clone())
            .unwrap_or_default();
        self.render_loading_key = Some(key.clone());
        let workspace_store = self.workspace_store.clone();
        cx.spawn(async move |this, cx| {
            let mut fallback_texts = changes
                .iter()
                .map(|change| reusable.get(&change.path).cloned().flatten())
                .collect::<Vec<_>>();
            for first in (0..changes.len()).step_by(4) {
                if !this
                    .update(cx, |panel, _cx| {
                        panel.render_loading_key.as_ref() == Some(&key)
                    })
                    .unwrap_or(false)
                {
                    return;
                }
                let reads = changes
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(4)
                    .filter_map(|(index, change)| {
                        if change.diff.as_deref().is_none_or(is_binary_patch)
                            || reusable.contains_key(&change.path)
                            || texts.get(index).is_some_and(|text| text.new.is_some())
                        {
                            return None;
                        }
                        let path = PathBuf::from(&change.path);
                        let path = if path.is_absolute() {
                            path
                        } else {
                            cwd.join(path)
                        };
                        let task = workspace_store
                            .update(cx, |store, cx| store.read_file_preview(path, cx));
                        Some((index, task))
                    })
                    .collect::<Vec<_>>();
                for (index, task) in reads {
                    if let Ok(bytes) = task.await {
                        fallback_texts[index] = String::from_utf8(bytes).ok();
                    }
                }
            }
            if !this
                .update(cx, |panel, _cx| {
                    panel.render_loading_key.as_ref() == Some(&key)
                })
                .unwrap_or(false)
            {
                return;
            }
            let sources = changes
                .iter()
                .zip(&fallback_texts)
                .map(|(change, text)| (change.path.clone(), text.clone()))
                .collect();
            let files = cx
                .background_executor()
                .spawn(async move {
                    changes
                        .iter()
                        .enumerate()
                        .map(|(index, change)| {
                            render_file(
                                change,
                                texts.get(index),
                                fallback_texts[index].as_deref(),
                                RenderFileContext {
                                    cwd: &cwd,
                                    options: DiffOptions {
                                        ignore_ws: key.ignore_ws,
                                        show_invisibles: key.show_invisibles,
                                    },
                                    theme: &appearance.theme,
                                    colors: &appearance.colors,
                                    whitespace_style: &appearance.whitespace_style,
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.render_loading_key.as_ref() == Some(&key) {
                    let previous = panel
                        .cache
                        .take()
                        .filter(|cache| cache.session == key.session && cache.scope == key.scope);
                    let list = match previous {
                        Some(DiffCache { mut list, .. }) => {
                            list.replace_files(files);
                            list
                        }
                        None => DiffList::new(files, Vec::new(), cx),
                    };
                    panel.cache = Some(DiffCache {
                        session: key.session.clone(),
                        scope: key.scope,
                        revision: key.revision,
                        dark: key.dark,
                        ignore_ws: key.ignore_ws,
                        show_invisibles: key.show_invisibles,
                        list,
                        fallback_texts: sources,
                    });
                    panel.render_loading_key = None;
                    panel.apply_pending_file_focus(&key.session, key.scope, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Rebuild the rendered-file cache when its key (session / turn / theme)
    /// changed. Returns whether there is anything to show.
    fn ensure_cache(&mut self, cx: &mut Context<Self>) -> bool {
        let pending_focus = self
            .workspace_store
            .read(cx)
            .pending_diff_focus_for(self.session_id.as_deref());
        if let Some(request) = pending_focus {
            if self.session_id.as_deref() == Some(request.session.as_str()) {
                self.scopes
                    .insert(request.session.clone(), DiffScope::Turn(request.turn));
            } else {
                self.workspace_store.update(cx, |store, cx| {
                    store.discard_diff_focus_for(self.session_id.as_deref(), cx);
                });
            }
        }
        let dark = cx.theme().mode.is_dark();
        let (session, scope, revision, cwd) = {
            let store = self.workspace_store.read(cx);
            let Some(active) = store.diff_active_state_for(self.session_id.as_deref(), cx) else {
                self.cache = None;
                return false;
            };
            let session = active.session;
            let Some(scope) = self.selected_scope(&session, cx) else {
                self.cache = None;
                return false;
            };
            (
                session,
                scope,
                store.diff_refresh_generation_for(self.session_id.as_deref()),
                active.cwd,
            )
        };
        // Another view's files and build are dropped; this view's stay on screen while a
        // newer build is pending, so a refresh never blanks the panel.
        if self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.session != session || cache.scope != scope)
        {
            self.cache = None;
        }
        if self
            .render_loading_key
            .as_ref()
            .is_some_and(|key| key.session != session || key.scope != scope)
        {
            self.render_loading_key = None;
        }
        if matches!(scope, DiffScope::WorkingTree | DiffScope::Branch) {
            let base = (scope == DiffScope::Branch)
                .then(|| self.bases.get(&session).cloned())
                .flatten();
            self.request_git_preview(
                session.clone(),
                cwd.clone(),
                scope,
                base.clone(),
                GitPreviewOptions {
                    revision,
                    ignore_ws: self.ignore_ws,
                },
                cx,
            );
            let Some(_) = self.git_preview.as_ref().filter(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == revision
                    && preview.ignore_ws == self.ignore_ws
            }) else {
                return self
                    .cache
                    .as_ref()
                    .is_some_and(|c| !c.list.files.is_empty());
            };
        }

        let fresh = self.cache.as_ref().is_none_or(|c| {
            c.session != session
                || c.scope != scope
                || c.revision != revision
                || c.dark != dark
                || c.ignore_ws != self.ignore_ws
                || c.show_invisibles != self.show_invisibles
        });
        if fresh {
            let appearance = RenderAppearance::of(cx);
            let (changes, texts) = match scope {
                DiffScope::Turn(turn) => {
                    let changes = self
                        .workspace_store
                        .read(cx)
                        .with_diff_turn_changes_for(
                            self.session_id.as_deref(),
                            turn,
                            |changes, _| changes.to_vec(),
                            cx,
                        )
                        .unwrap_or_default();
                    (changes, Vec::new())
                }
                DiffScope::WorkingTree | DiffScope::Branch => {
                    let preview = self
                        .git_preview
                        .as_ref()
                        .expect("matching git preview checked above");
                    (preview.result.changes.clone(), preview.result.texts.clone())
                }
            };
            self.request_rendered_files(
                RenderKey {
                    session,
                    scope,
                    revision,
                    dark,
                    ignore_ws: self.ignore_ws,
                    show_invisibles: self.show_invisibles,
                },
                changes,
                texts,
                cwd,
                appearance,
                cx,
            );
            return false;
        }
        self.apply_pending_file_focus(&session, scope, cx);
        self.cache
            .as_ref()
            .is_some_and(|c| !c.list.files.is_empty())
    }

    fn render_tab_strip(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let chrome = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        let panel_open = chrome.right_panel_open;
        let expanded = chrome.right_panel_expanded;
        let active = chrome.right_tab;
        // Windows: the open Diff/Plan panel is the rightmost column, so this
        // strip hosts the caption buttons. It is shorter than the 52px shell
        // header, so grow it to match — the buttons must reach the window top,
        // and a taller strip keeps the tabs aligned with the chat header.
        let hosts_caption = self.at_window_corner
            && window_caption::hosts_caption_for_state(
                window_caption::CaptionSurface::RightPanel,
                self.window_state.read(cx).route(),
                panel_open,
                active,
            );
        let store = self.workspace_store.clone();
        let store_close = self.workspace_store.clone();
        let session_expand = self.session_id.clone();
        let session_close = self.session_id.clone();
        let select_tab = {
            let store = self.workspace_store.clone();
            let session_id = self.session_id.clone();
            move |tab, cx: &mut App| {
                store.update(cx, |store, cx| {
                    store.set_right_tab_for(session_id.as_deref(), tab, cx)
                });
            }
        };
        let agents = {
            let store = self.workspace_store.read(cx);
            self.session_id.as_deref().and_then(|id| {
                let agents = Agents::of(store, id, cx);
                (!agents.is_empty() || active == RightTab::Agents).then(|| agents.outstanding())
            })
        };
        let count = self.session_id.as_deref().map_or(0, |id| {
            self.workspace_store
                .read(cx)
                .pull_requests(id)
                .iter()
                .filter(|link| link.visible())
                .count()
        });
        let build_tabs = move |icon_only| {
            let select_diff = select_tab.clone();
            let select_plan = select_tab.clone();
            let select_pr = select_tab.clone();
            let select_agents = select_tab.clone();
            let tabs = TabBar::new("right-panel-tabs")
                .underline()
                .hairline(false)
                .aria_label(crate::tr!("diff.panel_tabs"))
                .icon_only(icon_only)
                .w_full()
                .item(
                    TabItem::new("diff-tab", crate::tr!("diff.title").into_owned())
                        .icon(IconName::File)
                        .selected(active == RightTab::Diff)
                        .on_click(move |_, _, cx| select_diff(RightTab::Diff, cx)),
                )
                .item(
                    TabItem::new("plan-tab", crate::tr!("tasks.tab").into_owned())
                        .icon(IconName::Map)
                        .selected(active == RightTab::Plan)
                        .on_click(move |_, _, cx| select_plan(RightTab::Plan, cx)),
                );
            let tabs = if let Some(outstanding) = agents {
                tabs.item(
                    TabItem::new("agents-tab", crate::tr!("agents.title").into_owned())
                        .icon(IconName::Bot)
                        .selected(active == RightTab::Agents)
                        .count(outstanding)
                        .on_click(move |_, _, cx| select_agents(RightTab::Agents, cx)),
                )
            } else {
                tabs
            };
            tabs.item(
                TabItem::new(
                    "pull-requests-tab",
                    crate::tr!("pull_requests.title").into_owned(),
                )
                .icon(IconName::GitPullRequest)
                .selected(active == RightTab::PullRequests)
                .count(count)
                .on_click(move |_, _, cx| select_pr(RightTab::PullRequests, cx)),
            )
        };
        // Measure the labelled bar even when icons are shown, so growing the
        // panel can restore labels without alternating between the two widths.
        // The labels dropping or coming back cross-fades the two bars: the
        // width that decides it is measured as the strip lays out, so the
        // frame after it changes starts the fade, on the panel's clock.
        let measured = self.tab_icons.get();
        let icons = measured.map(|icons| {
            self.motion.transition(
                "tab-icons",
                if icons { 1f32 } else { 0. },
                GLIDE,
                window,
                cx,
            )
        });
        let tab_icons = self.tab_icons.clone();
        let panel = cx.entity_id();
        // Whose panel it is, where it shows an agent's.
        let subject = {
            let store = self.workspace_store.read(cx);
            self.lead
                .as_deref()
                .zip(self.session_id.as_deref())
                .map(|(lead, agent)| {
                    let title = Agents::of(store, lead, cx).title(store, agent);
                    crate::tr!("diff.agent_subject", title = title.as_str()).into_owned()
                })
        };
        let subject = self
            .motion
            .shown("diff-subject", subject, SHOW, window, cx)
            .map(|shown| {
                let label = shown.value.clone();
                shown.fold_across(
                    material::chip_surface(cx.theme().secondary, cx.theme().foreground, cx)
                        .debug_selector(|| "diff-subject".into())
                        .max_w(px(SUBJECT_WIDEST))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(label)),
                        ),
                )
            });
        let tabs = canvas(
            move |bounds, window, cx| {
                let labelled = build_tabs(false).into_any_element().layout_as_root(
                    size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                    window,
                    cx,
                );
                let narrow = labelled.width > bounds.size.width;
                if tab_icons.replace(Some(narrow)) != Some(narrow) {
                    window.on_next_frame(move |_, cx| cx.notify(panel));
                }
                let shown = measured.unwrap_or(narrow);
                // The bar leaving is drawn first, under the one coming in,
                // and takes no clicks.
                let layers = match icons {
                    Some(icons) if shown => vec![(false, 1. - icons), (true, icons)],
                    Some(icons) => vec![(true, icons), (false, 1. - icons)],
                    None => vec![(shown, 1.)],
                };
                layers
                    .into_iter()
                    .filter(|(_, opacity)| *opacity > 0.)
                    .map(|(icon_only, opacity)| {
                        let mut tabs = div()
                            .id(if icon_only {
                                "tabs-icons"
                            } else {
                                "tabs-labelled"
                            })
                            .relative()
                            .size_full()
                            .opacity(opacity)
                            .child(build_tabs(icon_only))
                            .when(icon_only != shown, |tabs| {
                                tabs.child(crate::motion::shield())
                            })
                            .into_any_element();
                        tabs.layout_as_root(
                            size(
                                AvailableSpace::Definite(bounds.size.width),
                                AvailableSpace::Definite(bounds.size.height),
                            ),
                            window,
                            cx,
                        );
                        tabs.prepaint_at(bounds.origin, window, cx);
                        tabs
                    })
                    .collect::<Vec<_>>()
            },
            |_, layers, window, cx| {
                for mut tabs in layers {
                    tabs.paint(window, cx);
                }
            },
        )
        .flex_1()
        .min_w_0()
        .h(px(material::bar_height(material::Bar::Toolbar, false)))
        .mb(px(-1.))
        .self_end();
        let kind = if hosts_caption {
            material::Bar::Strip
        } else {
            material::Bar::Toolbar
        };
        material::bar_divider_below(material::bar("right-panel-header", kind, false, cx), cx)
            .when(hosts_caption, |strip| strip.pr_0())
            .child(tabs)
            .children(subject)
            // The gap between the tabs and the icon cluster holds nothing, so
            // it doubles as the window's drag handle: `window_drag_area` for the
            // app-owned move (macOS), `drag_region` for native HTCAPTION
            // (Windows). `h_full` is load-bearing: the strip centers its
            // children, so without it the drag hitbox collapses to zero height.
            .child(window_caption::drag_region(crate::window_drag_area(
                "right-panel-tabs-drag",
                div().flex_none().w(cx.theme().tokens.spacing.sm).h_full(),
                window,
                cx,
            )))
            // Right icon cluster: expand toggle, a layout no-op, close.
            .child(
                material::toolbar_icon_button(
                    "diff-expand",
                    if expanded {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    },
                    if expanded {
                        crate::tr!("diff.restore_width")
                    } else {
                        crate::tr!("diff.expand_width")
                    },
                    false,
                )
                .debug_selector(|| "diff-expand".into())
                .flex_none()
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| {
                        store.toggle_diff_expanded_for(session_expand.as_deref(), cx);
                    });
                }),
            )
            .child(
                material::toolbar_icon_button(
                    "diff-close",
                    IconName::Close,
                    crate::tr!("diff.close"),
                    false,
                )
                .debug_selector(|| "diff-close".into())
                .flex_none()
                .on_click(move |_, _, cx| {
                    store_close.update(cx, |store, cx| {
                        store.close_right_panel_for(session_close.as_deref(), cx);
                    });
                }),
            )
            // Last child: the panel's own actions keep their places to its left.
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
            .into_any_element()
    }

    /// Whether this panel is drawn as a compact page rather than the wide
    /// layout's right column.
    fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    fn on_view_option(&mut self, option: &DiffViewOption, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_view_option(*option, cx);
    }

    fn on_selection_menu(
        &mut self,
        action: &DiffSelectionMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(list) = self.cache.as_ref().map(|cache| &cache.list) else {
            return;
        };
        if list.selection.is_none() {
            return;
        }
        match action {
            DiffSelectionMenu::CopyLines => {
                let text = list.selected_lines();
                if !text.is_empty() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            }
            DiffSelectionMenu::AddComment => self.start_comment(window, cx),
        }
    }

    fn start_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comment_input = Some(cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("diff.comment_placeholder"))
        }));
        self.remeasure_lists();
        cx.notify();
    }

    /// The file's absolute path: the rendered path is relative to the
    /// workspace for display.
    fn absolute_file_path(&self, file: &RenderedFile, cx: &App) -> String {
        let path = Path::new(&file.path);
        if path.is_absolute() {
            return file.path.clone();
        }
        match self
            .workspace_store
            .read(cx)
            .diff_active_state_for(self.session_id.as_deref(), cx)
        {
            Some(active) => active.cwd.join(path).to_string_lossy().into_owned(),
            None => file.path.clone(),
        }
    }

    fn apply_view_option(&mut self, option: DiffViewOption, cx: &mut Context<Self>) {
        match option {
            DiffViewOption::Split => {
                self.workspace_store.update(cx, |store, cx| {
                    let split = store.diff_split_for(self.session_id.as_deref());
                    store.set_diff_split_for(self.session_id.as_deref(), !split, cx);
                });
                self.remeasure_lists();
            }
            DiffViewOption::Wrap => {
                self.workspace_store.update(cx, |store, cx| {
                    store.toggle_diff_wrap_for(self.session_id.as_deref(), cx)
                });
                self.remeasure_lists();
            }
            DiffViewOption::Whitespace => {
                self.ignore_ws = !self.ignore_ws;
                self.git_preview = None;
            }
            DiffViewOption::Invisibles => {
                self.show_invisibles = !self.show_invisibles;
            }
        }
        cx.notify();
    }

    fn render_toolbar(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let active_state = self
            .workspace_store
            .read(cx)
            .diff_active_state_for(self.session_id.as_deref(), cx);
        let session = active_state
            .as_ref()
            .map(|active| active.session.clone())
            .unwrap_or_default();
        let selected_scope = self.selected_scope(&session, cx);
        let turns: Rc<[usize]> = self
            .workspace_store
            .read(cx)
            .diff_turns_for(self.session_id.as_deref(), cx)
            .into_iter()
            .rev()
            .collect();
        let label = match selected_scope {
            Some(DiffScope::Turn(turn)) => {
                crate::tr!("diff.turn", count = self.turn_number(&session, turn, cx)).into_owned()
            }
            Some(DiffScope::WorkingTree) => crate::tr!("diff.working_tree").into_owned(),
            Some(DiffScope::Branch) => crate::tr!("diff.branch_changes").into_owned(),
            None => crate::tr!("diff.no_changes").into_owned(),
        };
        let muted = cx.theme().muted_foreground;
        let panel = cx.entity();
        let session_selector = session.clone();

        let trigger = Button::new("diff-turn-select")
            .ghost()
            .outline()
            .compact()
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.sm)
                    .items_center()
                    .text_body(compact, cx)
                    .font_medium()
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).text_color(muted)),
            );

        let selector =
            Popover::new("diff-turn-popover")
                .trigger(trigger)
                .content(move |_, _, cx| {
                    let panel_for = panel.clone();
                    let session_for = session_selector.clone();
                    let scope_row =
                        |id: &'static str,
                         label: gpui::SharedString,
                         scope: DiffScope,
                         cx: &mut gpui::Context<gpui_base::PopoverState>| {
                            let panel = panel_for.clone();
                            let session = session_for.clone();
                            menu_row(
                                id.into(),
                                label.clone(),
                                selected_scope == Some(scope),
                                compact,
                                cx,
                            )
                            .child(div().flex_1().child(label))
                            .when(selected_scope == Some(scope), |row| {
                                row.child(Icon::new(IconName::Check))
                            })
                            .on_click({
                                let popover = cx.entity();
                                move |_, window, cx| {
                                    panel.update(cx, |this, cx| {
                                        this.scopes.insert(session.clone(), scope);
                                        this.cache = None;
                                        this.workspace_store.update(cx, |store, cx| {
                                            store.discard_diff_focus_for(
                                                this.session_id.as_deref(),
                                                cx,
                                            );
                                        });
                                        cx.notify();
                                    });
                                    popover.update(cx, |state, cx| state.dismiss(window, cx));
                                }
                            })
                        };
                    let turn_rows = (!turns.is_empty()).then(|| {
                        let turns = turns.clone();
                        let panel = panel.clone();
                        let session = session_selector.clone();
                        let popover = cx.entity();
                        crate::scroll::VirtualList::uniform(
                            "diff-turn-items",
                            turns.len(),
                            move |range, _, cx| {
                                range
                                    .map(|index| {
                                        let turn = turns[index];
                                        div().pb_0p5().child(turn_row(
                                            turn,
                                            selected_scope == Some(DiffScope::Turn(turn)),
                                            compact,
                                            &panel,
                                            &session,
                                            &popover,
                                            cx,
                                        ))
                                    })
                                    .collect()
                            },
                        )
                        .w_full()
                        .min_h_0()
                    });
                    v_flex()
                        .id("diff-turn-list")
                        .role(Role::Menu)
                        .aria_label(crate::tr!("diff.scope_menu"))
                        // A popover sizes to its content; the turn list is virtual,
                        // so the menu's extent is fixed here and it scrolls past it.
                        .min_w(px(190.))
                        .max_h(px(320.))
                        .pt_1()
                        .px_1()
                        // The last turn row carries its gap.
                        .pb(if turn_rows.is_some() {
                            cx.theme().tokens.spacing.xxs
                        } else {
                            cx.theme().tokens.spacing.xs
                        })
                        .gap_0p5()
                        .child(scope_row(
                            "diff-scope-working",
                            crate::tr!("diff.working_tree").into_owned().into(),
                            DiffScope::WorkingTree,
                            cx,
                        ))
                        .child(scope_row(
                            "diff-scope-branch",
                            crate::tr!("diff.branch_changes").into_owned().into(),
                            DiffScope::Branch,
                            cx,
                        ))
                        .child(
                            div()
                                .flex_none()
                                .px_2()
                                .pt_2()
                                .pb_1()
                                .text_caption(cx)
                                .child(crate::tr!("diff.turns")),
                        )
                        .children(turn_rows)
                });

        let branch_scope = selected_scope == Some(DiffScope::Branch);
        let base_selector = {
            let mut branches: Rc<[String]> = self
                .git_preview
                .as_ref()
                .map(|preview| preview.result.branches.as_slice().into())
                .unwrap_or_default();
            if branches.is_empty() {
                branches = active_state
                    .as_ref()
                    .map(|active| active.branches.as_slice().into())
                    .unwrap_or_default();
            }
            let current = self
                .bases
                .get(&session)
                .cloned()
                .or_else(|| {
                    self.git_preview
                        .as_ref()
                        .and_then(|p| p.result.base.clone())
                })
                .unwrap_or_default();
            let panel = cx.entity();
            let session_base = session.clone();
            let current_label = current.clone();
            let current: Rc<str> = current.into();
            let trigger = Button::new("diff-base-select")
                .ghost()
                .outline()
                .compact()
                .label(current_label)
                .icon(IconName::ChevronDown);
            Popover::new("diff-base-popover")
                .trigger(trigger)
                .content(move |_, _, cx| {
                    let branches = branches.clone();
                    let current = current.clone();
                    let panel = panel.clone();
                    let session = session_base.clone();
                    let popover = cx.entity();
                    let branch_count = branches.len();
                    let widest = branches
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, branch)| branch.chars().count())
                        .map_or(0, |(index, _)| index);
                    div()
                        .id("diff-base-list")
                        .role(Role::Menu)
                        .aria_label(crate::tr!("diff.base_branches"))
                        .child(
                            crate::scroll::VirtualList::uniform(
                                "diff-base-items",
                                branch_count,
                                move |range, _, cx| {
                                    range
                                        .map(|index| {
                                            let branch = &branches[index];
                                            let panel = panel.clone();
                                            let session = session.clone();
                                            let popover = popover.clone();
                                            div().pb_0p5().child(base_row(
                                                index,
                                                branch,
                                                *branch == *current,
                                                compact,
                                                move |chosen, window, cx| {
                                                    panel.update(cx, |this, cx| {
                                                        this.bases.insert(session.clone(), chosen);
                                                        this.cache = None;
                                                        this.git_preview = None;
                                                        cx.notify();
                                                    });
                                                    popover.update(cx, |state, cx| {
                                                        state.dismiss(window, cx)
                                                    });
                                                },
                                                cx,
                                            ))
                                        })
                                        .collect()
                                },
                            )
                            .width_from_row(widest)
                            // A popover sizes to its content; the branch list
                            // is virtual, so its extent is fixed here.
                            .min_w(px(180.))
                            .max_h(px(280.))
                            .pt_1()
                            .px_1()
                            // The last row carries its gap.
                            .pb(if branch_count == 0 {
                                cx.theme().tokens.spacing.xs
                            } else {
                                cx.theme().tokens.spacing.xxs
                            }),
                        )
                })
                .into_any_element()
        };
        // The base picker fades in beside the scope picker when Branch is chosen, and out.
        let base_selector = crate::motion::Sampled::new(move |window, cx| {
            match crate::motion::shown(
                "diff-base-selector",
                branch_scope.then_some(()),
                SHOW,
                window,
                cx,
            ) {
                Some(shown) => shown.fade(base_selector),
                // Out of flow, so an absent picker takes no gap in the toolbar.
                None => div().absolute().into_any_element(),
            }
        });

        let store = self.workspace_store.read(cx);
        let wrap_on = store.diff_word_wrap_for(self.session_id.as_deref());
        let split_on = store.diff_split_for(self.session_id.as_deref());
        // One description of the view options, rendered as four dense toggles on
        // the desktop and as one overflow menu on a page with no room for them.
        let options = [
            (
                DiffViewOption::Split,
                "diff-view-split",
                IconName::PanelLeft,
                split_on,
                if split_on {
                    crate::tr!("diff.unified_view")
                } else {
                    crate::tr!("diff.split_view")
                }
                .into_owned(),
            ),
            (
                DiffViewOption::Wrap,
                "diff-wrap",
                IconName::Menu,
                wrap_on,
                crate::tr!("diff.toggle_wrap").into_owned(),
            ),
            (
                DiffViewOption::Whitespace,
                "diff-whitespace",
                IconName::Eye,
                self.ignore_ws,
                crate::tr!("diff.toggle_whitespace").into_owned(),
            ),
            (
                DiffViewOption::Invisibles,
                "diff-invisibles",
                IconName::CaseSensitive,
                self.show_invisibles,
                crate::tr!("diff.toggle_invisibles").into_owned(),
            ),
        ];
        let mut toolbar =
            material::bar("diff-toolbar", material::Bar::Toolbar, compact, cx).child(selector);
        if compact {
            // The scope and base pickers are the row; everything else is one
            // overflow menu, so a 393pt page never has to choose what to clip.
            toolbar = toolbar.child(base_selector).child(div().flex_1()).child(
                material::toolbar_icon_button(
                    "diff-view-options",
                    IconName::Ellipsis,
                    crate::tr!("mobile.more_actions"),
                    true,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for (option, _, _, on, label) in &options {
                        menu = menu.menu_with_check(label.clone(), *on, Box::new(*option));
                    }
                    menu
                }),
            );
        } else {
            toolbar = toolbar.child(div().flex_1());
            for (option, id, icon, on, label) in options {
                toolbar = toolbar.child(
                    material::toolbar_icon_button(id, icon, label, false)
                        .selected(on)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.apply_view_option(option, cx)),
                        ),
                );
            }
            toolbar = toolbar.child(base_selector);
        }
        toolbar.into_any_element()
    }

    fn review_excerpt(&self, selection: &LineSelection) -> String {
        let Some(file) = self.cache.as_ref().and_then(|cache| {
            cache
                .list
                .files
                .iter()
                .find(|file| file.path == selection.file)
        }) else {
            return String::new();
        };
        let start = selection.row_start.min(selection.row_end);
        let end = selection.row_start.max(selection.row_end);
        let selected = file
            .all_rows
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= start && *index <= end)
            .map(|(_, row)| (row.kind, row.old, row.new, row.source(selection.side)))
            .collect::<Vec<_>>();
        let old_start = selected.iter().find_map(|(_, old, _, _)| *old).unwrap_or(0);
        let new_start = selected.iter().find_map(|(_, _, new, _)| *new).unwrap_or(0);
        let old_count = selected
            .iter()
            .filter(|(kind, ..)| *kind != RowKind::Added)
            .count();
        let new_count = selected
            .iter()
            .filter(|(kind, ..)| *kind != RowKind::Removed)
            .count();
        let mut lines = vec![format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        )];
        lines.extend(selected.into_iter().map(|(kind, _, _, text)| {
            let marker = match kind {
                RowKind::Added => '+',
                RowKind::Removed => '-',
                RowKind::Context => ' ',
            };
            format!("{marker}{text}")
        }));
        lines.join("\n")
    }

    fn submit_comment(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.list.selection.clone())
        else {
            return;
        };
        let Some(input) = self.comment_input.as_ref() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let (section_id, section_title) = match self
            .cache
            .as_ref()
            .map(|cache| (cache.scope, cache.session.as_str()))
        {
            Some((DiffScope::Turn(turn), session)) => {
                let number = self.turn_number(session, turn, cx);
                (format!("turn:{number}"), format!("Turn {number}"))
            }
            Some((DiffScope::WorkingTree, _)) => {
                ("unstaged".to_string(), "Working tree".to_string())
            }
            Some((DiffScope::Branch, _)) => ("branch".to_string(), "Branch changes".to_string()),
            None => ("diff".to_string(), "Review".to_string()),
        };
        let comment = ReviewComment::new(
            selection.file.clone(),
            selection.line_start,
            selection.line_end,
            selection.side,
            text,
            self.review_excerpt(&selection),
            section_id,
            section_title,
            selection.start_index,
            selection.end_index,
        );
        let session_id = self.session_id.clone().unwrap_or_default();
        self.workspace_store.update(cx, |store, _cx| {
            store.add_review_comment_for(session_id, comment);
        });
        if let Some(cache) = self.cache.as_mut() {
            cache.list.selection = None;
        }
        self.comment_input = None;
        self.remeasure_lists();
        cx.notify();
    }

    /// The diff, cross-fading over the Loading line it replaces.
    fn render_body(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let loading = self.cache.is_none()
            && (self.loading_key.is_some() || self.render_loading_key.is_some());
        let fade = crate::motion::CrossFade::new(
            "diff-loading",
            usize::from(!loading),
            2,
            CHANGE,
            window,
            cx,
        )
        .fill();
        let column = |body: AnyElement| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(body)
                .into_any_element()
        };
        let mut layers = Vec::new();
        if fade.shows(0) {
            layers.push((
                0,
                column(self.render_status(crate::tr!("diff.loading").into_owned(), cx)),
            ));
        }
        if !loading {
            layers.push((1, column(self.render_loaded(window, cx))));
        }
        fade.render(layers)
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element()
    }

    fn render_loaded(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            if self.loading_key.is_some() || self.render_loading_key.is_some() {
                return self.render_status(crate::tr!("diff.loading").into_owned(), cx);
            }
            return self.render_empty(cx);
        };
        let viewport = if cache.list.files.is_empty() {
            self.render_empty(cx)
        } else {
            let store = self.workspace_store.read(cx);
            let split = store.diff_split_for(self.session_id.as_deref());
            let wrap = store.diff_word_wrap_for(self.session_id.as_deref());
            render_list(self, "diff-body", split, wrap, window, cx)
        };
        // A compact page holds its content clear of the window edges; the code
        // itself still scrolls sideways *inside* that inset rather than running
        // off the page.
        let mut content = v_flex()
            .size_full()
            .min_h_0()
            .px(self.motion.transition(
                "page-inset",
                if self.compact(cx) {
                    px(material::COMPACT_PAGE_INSET)
                } else {
                    px(0.)
                },
                GLIDE,
                window,
                cx,
            ))
            .child(viewport);

        let preview = self
            .git_preview
            .as_ref()
            .filter(|preview| preview.session == cache.session && preview.scope == cache.scope);
        // The truncated and error notices fold in under the diff and out when they clear.
        let truncated = preview
            .is_some_and(|preview| preview.result.truncated)
            .then(|| crate::tr!("diff.truncated").into_owned());
        let error = preview.and_then(|preview| preview.result.error.clone());
        // A turn's changes as the provider attributed them; commands may have written others.
        let partial = match cache.scope {
            DiffScope::Turn(turn) => self
                .workspace_store
                .read(cx)
                .with_diff_turn_changes_for(
                    self.session_id.as_deref(),
                    turn,
                    |_, completeness| completeness == agent::ChangeCompleteness::Partial,
                    cx,
                )
                .unwrap_or_default(),
            DiffScope::WorkingTree | DiffScope::Branch => false,
        }
        .then(|| crate::tr!("diff.turn_partial").into_owned());
        for (id, notice) in [
            ("diff-turn-partial", partial),
            ("diff-truncated", truncated),
            ("diff-error", error),
        ] {
            if let Some(shown) = crate::motion::shown(id, notice, SHOW, window, cx) {
                content = content.child(shown.fold(render_notice(shown.value.clone(), cx)));
            }
        }

        content.into_any_element()
    }

    fn render_status(&self, message: String, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
            .into_any_element()
    }

    fn render_comment_ui(
        &self,
        file: &str,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = self
            .observed_review_comments
            .iter()
            .filter(|comment| {
                comment.file == file
                    && match comment.side {
                        ReviewSide::Old => old,
                        ReviewSide::New => new,
                    } == Some(comment.line_end)
            })
            .map(|comment| {
                material::card(cx)
                    .flex_row()
                    .items_center()
                    .min_w_full()
                    .relative()
                    .font_family(cx.theme().font_family.clone())
                    .text_token(&cx.theme().tokens.typography.xs)
                    // The comment's accent rail; no owner draws an accent edge
                    // on a card.
                    .child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(6.))
                            .bottom(px(6.))
                            .w(px(2.))
                            .rounded_full()
                            .bg(cx.theme().primary),
                    )
                    .child(Icon::empty().path("icons/pencil.svg"))
                    .child(comment.text.clone())
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let selection = self
            .cache
            .as_ref()
            .and_then(|cache| cache.list.selection.as_ref())
            .filter(|selection| {
                selection.file == file
                    && match selection.side {
                        ReviewSide::Old => old,
                        ReviewSide::New => new,
                    } == Some(selection.line_end)
            });
        if selection.is_some() {
            if let Some(input) = &self.comment_input {
                rows.push(
                    material::card(cx)
                        .min_w_full()
                        .font_family(cx.theme().font_family.clone())
                        .child(Input::new(input).appearance(false))
                        .child(
                            h_flex().justify_end().child(
                                Button::new("diff-submit-comment")
                                    .primary()
                                    .small()
                                    .label(crate::tr!("diff.submit_comment"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_comment(cx);
                                    })),
                            ),
                        )
                        .into_any_element(),
                );
            } else {
                rows.push(super::list::selection_row(
                    "diff-add-comment",
                    crate::tr!("diff.add_comment").into(),
                    None,
                    cx.listener(|this, _, window, cx| {
                        this.start_comment(window, cx);
                    }),
                    cx,
                ));
            }
        }
        rows
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .debug_selector(|| "diff-empty".into())
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_body(self.compact(cx), cx)
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("diff.empty")),
            )
            .into_any_element()
    }
}

/// One item of the scope and base pickers: a menu item that marks the chosen
/// scope. These pickers are virtual lists inside a `Popover`, which
/// `PopupMenu` does not host, so the item is drawn on the menu's frame.
fn menu_row(
    id: gpui::ElementId,
    label: gpui::SharedString,
    selected: bool,
    compact: bool,
    cx: &App,
) -> material::Clickable<gpui::Div> {
    material::accessible_clickable(
        material::menu_item_frame(
            compact,
            material::ItemFill::highlight(selected, cx.theme().muted),
            cx,
        ),
        id,
        Role::MenuItem,
        label,
        cx,
    )
    .aria_selected(selected)
    .flex_none()
    .w_full()
    .items_center()
    .text_body(compact, cx)
    .cursor_pointer()
}

fn turn_row(
    turn: usize,
    selected: bool,
    compact: bool,
    panel: &Entity<DiffPanel>,
    session: &str,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let panel = panel.clone();
    let session = session.to_string();
    let popover = popover.clone();
    let number = panel.read(cx).turn_number(&session, turn, cx);
    let label: gpui::SharedString = crate::tr!("diff.turn", count = number).into_owned().into();
    menu_row(
        ("diff-turn-item", turn).into(),
        label.clone(),
        selected,
        compact,
        cx,
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(crate::widgets::fade_text(label)),
    )
    .when(selected, |this| this.child(Icon::new(IconName::Check)))
    .on_click(move |_, window, cx| {
        panel.update(cx, |this, cx| {
            this.scopes.insert(session.clone(), DiffScope::Turn(turn));
            this.cache = None;
            this.workspace_store.update(cx, |store, cx| {
                store.select_diff_turn_for(this.session_id.as_deref(), turn, cx);
            });
            cx.notify();
        });
        popover.update(cx, |st, cx| st.dismiss(window, cx));
    })
    .into_any_element()
}

fn base_row(
    index: usize,
    branch: &str,
    selected: bool,
    compact: bool,
    on_pick: impl Fn(String, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let chosen = branch.to_string();
    menu_row(
        ("diff-base-item", index).into(),
        crate::tr!("diff.base_branch", branch = branch)
            .into_owned()
            .into(),
        selected,
        compact,
        cx,
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(crate::widgets::fade_text(branch.to_string())),
    )
    .when(selected, |row| row.child(Icon::new(IconName::Check)))
    .on_click(move |_, window, cx| on_pick(chosen.clone(), window, cx))
    .into_any_element()
}

impl DiffListHost for DiffPanel {
    fn diff_list(&self) -> Option<&DiffList> {
        self.cache.as_ref().map(|cache| &cache.list)
    }

    fn diff_list_mut(&mut self) -> Option<&mut DiffList> {
        self.cache.as_mut().map(|cache| &mut cache.list)
    }

    fn expand_gap(
        &mut self,
        file: usize,
        lines: Range<u32>,
        direction: ExpandDir,
        cx: &mut Context<Self>,
    ) {
        if let Some(list) = self.diff_list_mut() {
            list.expand(file, lines, direction);
        }
        self.comment_input = None;
        cx.notify();
    }

    fn file_menu(&self, file: usize, cx: &App) -> FileMenu {
        let path = self
            .diff_list()
            .and_then(|list| list.files.get(file))
            .map(|file| self.absolute_file_path(file, cx))
            .unwrap_or_default();
        let cwd = self
            .workspace_store
            .read(cx)
            .diff_active_state_for(self.session_id.as_deref(), cx)
            .map(|active| active.cwd);
        Rc::new(move |menu, _, _| menu.path_items(&path, cwd.as_deref()))
    }

    fn row_extras(
        &self,
        file: usize,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(path) = self
            .diff_list()
            .and_then(|list| list.files.get(file))
            .map(|file| file.path.clone())
        else {
            return Vec::new();
        };
        self.render_comment_ui(&path, old, new, cx)
    }

    fn review_comment_menu(&self, _cx: &App) -> Option<(bool, gpui::SharedString)> {
        Some((
            self.comment_input.is_none(),
            crate::tr!("diff.add_comment").into(),
        ))
    }
}

impl Render for DiffPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_cache(cx);
        let tab = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref())
            .right_tab;
        // Compact: the Panel page's segmented control is the only selector, and
        // Back is the only way off the page. This panel's own tab row and its
        // expand / split / close cluster are wide-layout affordances, so the
        // whole strip stays unbuilt rather than being drawn and ignored.
        let compact = self.compact(cx);
        // The strip folds away as the window turns compact and back as it
        // turns wide.
        let strip = self
            .motion
            .swap("tab-strip", usize::from(!compact), 2, CHANGE, window, cx);
        let strip_layer = strip
            .shows(1)
            .then(|| (1, self.render_tab_strip(window, cx)));
        let root = v_flex()
            .size_full()
            .min_w_0()
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_view_option))
            .on_action(cx.listener(Self::on_selection_menu))
            .child(div().flex_none().child(strip.render(strip_layer)));
        // Tab bodies cross-fade, each in its own layer, built only while it shows.
        let layer = match tab {
            RightTab::Diff | RightTab::Preview => 0,
            RightTab::PullRequests => 1,
            RightTab::Plan => 2,
            RightTab::Agents => 3,
        };
        let fade =
            crate::motion::CrossFade::new("right-tab-body", layer, 4, CHANGE, window, cx).fill();
        let body = || v_flex().size_full().min_h_0();
        let mut layers = Vec::new();
        if fade.shows(0) {
            layers.push((
                0,
                body()
                    .debug_selector(|| "diff-tab-body".into())
                    .child({
                        // The overflow menu and the four toggles trade places
                        // as the window turns.
                        let toolbar = self.motion.swap(
                            "toolbar",
                            usize::from(compact),
                            2,
                            CHANGE,
                            window,
                            cx,
                        );
                        let layers = [(0, false), (1, true)]
                            .into_iter()
                            .filter(|(layer, _)| toolbar.shows(*layer))
                            .map(|(layer, compact)| (layer, self.render_toolbar(compact, cx)))
                            .collect::<Vec<_>>();
                        toolbar.render(layers)
                    })
                    .child(self.render_body(window, cx))
                    .into_any_element(),
            ));
        }
        for (index, view) in [
            (1, self.pull_requests.clone().into_any_element()),
            (2, self.plan.clone().into_any_element()),
            (3, self.agents.clone().into_any_element()),
        ] {
            if fade.shows(index) {
                layers.push((index, body().child(view).into_any_element()));
            }
        }
        root.child(fade.render(layers).flex_1().min_h_0())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::list::build_list_items_with;
    use agent::FileChangeKind;

    #[gpui::test]
    fn supplied_added_text_and_binary_changes_render_without_fallback_reads(
        cx: &mut gpui::TestAppContext,
    ) {
        let (to_host, requests) = async_channel::unbounded();
        let (_replies, from_host) = async_channel::unbounded();
        let host = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::default());
        let panel = cx.new(|cx| DiffPanel::new(store, window_state, None, cx));
        panel.update(cx, |panel, cx| {
            panel.request_rendered_files(
                RenderKey {
                    session: "thread".into(),
                    scope: DiffScope::WorkingTree,
                    revision: 1,
                    dark: true,
                    ignore_ws: false,
                    show_invisibles: false,
                },
                vec![
                    FileChange {
                        path: "/workspace/added.rs".into(),
                        kind: FileChangeKind::Create,
                        diff: Some("@@ -0,0 +1 @@\n+fn added() {}\n".into()),
                    },
                    FileChange {
                        path: "/workspace/image.png".into(),
                        kind: FileChangeKind::Modify,
                        diff: None,
                    },
                    FileChange {
                        path: "/workspace/binary.dat".into(),
                        kind: FileChangeKind::Modify,
                        diff: Some("diff --git a/binary.dat b/binary.dat\nindex abc..def 100644\nBinary files a/binary.dat and b/binary.dat differ\n".into()),
                    },
                ],
                vec![
                    GitFileText {
                        old: None,
                        new: Some("fn added() {}\n".into()),
                    },
                    GitFileText::default(),
                    GitFileText::default(),
                ],
                PathBuf::from("/workspace"),
                RenderAppearance {
                    theme: HighlightTheme::default_dark(),
                    colors: DiffColors {
                        added_word_bg: gpui::hsla(0.3, 0.8, 0.5, 0.3),
                        removed_word_bg: gpui::hsla(0., 0.8, 0.5, 0.28),
                    },
                    whitespace_style: HighlightStyle::default(),
                },
                cx,
            );
        });
        cx.run_until_parked();
        while let Ok(line) = requests.try_recv() {
            let request = tcode_protocol::decode_client_line(&line).unwrap();
            assert!(
                !matches!(
                    request.payload,
                    tcode_protocol::ClientPayload::Query(
                        tcode_protocol::Query::ReadFilePreview { .. }
                    )
                ),
                "supplied added-file text and binary changes need no fallback file read"
            );
        }
        panel.read_with(cx, |panel, _| {
            let cache = panel
                .cache
                .as_ref()
                .expect("files render without host file replies");
            assert_eq!(cache.list.files.len(), 3);
            assert_eq!(
                cache.list.files[2].notice,
                Some(super::super::model::FileNotice::Binary)
            );
            assert!(cache.list.files[2].all_rows.is_empty());
            assert_eq!(cache.list.files[0].added, Some(1));
        });
    }

    #[gpui::test]
    fn an_oversized_fallback_preview_keeps_the_stored_patch(cx: &mut gpui::TestAppContext) {
        use tcode_protocol::{
            ClientPayload, HostMessage, ProtocolError, Query, QueryResponse, decode_client_line,
            encode_line,
        };
        cx.update(crate::test_client::init);
        let (to_host, requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::default());
        let panel = cx.new(|cx| DiffPanel::new(store, window_state, None, cx));
        panel.update(cx, |panel, cx| {
            panel.request_rendered_files(
                RenderKey {
                    session: "thread".into(),
                    scope: DiffScope::WorkingTree,
                    revision: 1,
                    dark: true,
                    ignore_ws: false,
                    show_invisibles: false,
                },
                (0..2)
                    .map(|index| FileChange {
                        path: format!("file-{index}.rs"),
                        kind: FileChangeKind::Modify,
                        diff: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
                    })
                    .collect(),
                Vec::new(),
                PathBuf::from("/workspace"),
                RenderAppearance {
                    theme: HighlightTheme::default_dark(),
                    colors: DiffColors {
                        added_word_bg: gpui::green(),
                        removed_word_bg: gpui::red(),
                    },
                    whitespace_style: HighlightStyle::default(),
                },
                cx,
            )
        });
        loop {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| panel.cache.is_some()) {
                break;
            }
            let reads = std::iter::from_fn(|| requests.try_recv().ok())
                .filter_map(|line| {
                    let request = decode_client_line(&line).unwrap();
                    match request.payload {
                        ClientPayload::Query(Query::ReadFilePreview { path }) => {
                            Some((request.id, path))
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            assert!(
                !reads.is_empty(),
                "loading waits for outstanding host previews"
            );
            for (id, path) in reads {
                let result = if path.ends_with("file-1.rs") {
                    Err(ProtocolError {
                        code: "file_too_large".into(),
                        message: "preview exceeds the host limit".into(),
                    })
                } else {
                    Ok(QueryResponse::FileBytes(b"new\n".to_vec()))
                };
                incoming
                    .try_send(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
                    .unwrap();
            }
        }
        panel.read_with(cx, |panel, _| {
            let files = &panel.cache.as_ref().unwrap().list.files;
            let oversized = &files[1];
            assert_eq!((oversized.added, oversized.removed), (Some(1), Some(1)));
            assert!(
                !oversized.expandable,
                "an oversized preview never invents unseen context"
            );
        });
    }

    #[test]
    fn out_of_workspace_turn_change_renders_from_stored_diff_without_file_text() {
        let path = "/tmp/tcode-outside-workspace.rs";
        let change = FileChange {
            path: path.into(),
            kind: FileChangeKind::Modify,
            diff: Some("@@ -1 +1 @@\n-fn old_value() {}\n+fn new_value() {}\n".into()),
        };
        let colors = DiffColors {
            added_word_bg: gpui::hsla(0.3, 0.8, 0.5, 0.3),
            removed_word_bg: gpui::hsla(0., 0.8, 0.5, 0.28),
        };

        let file = render_file(
            &change,
            None,
            None,
            RenderFileContext {
                cwd: Path::new("/workspace/repository"),
                options: DiffOptions {
                    ignore_ws: false,
                    show_invisibles: false,
                },
                theme: &HighlightTheme::default_dark(),
                colors: &colors,
                whitespace_style: &HighlightStyle::default(),
            },
        );

        assert_eq!(file.path, path);
        assert_eq!(file.added, Some(1));
        assert_eq!(file.removed, Some(1));
        assert_eq!(file.all_rows.len(), 2);
        let items = build_list_items_with(std::slice::from_ref(&file), &[]);
        for rows in [&items.unified, &items.split] {
            let content = rows
                .iter()
                .filter_map(|row| match row {
                    super::super::rows::DisplayRow::Code { left, right, .. } => Some((
                        left.map(|ix| file.all_rows[ix].source_text.as_str()),
                        right.map(|ix| file.all_rows[ix].source_text.as_str()),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                content
                    .iter()
                    .any(|(old, _)| *old == Some("fn old_value() {}"))
            );
            assert!(
                content
                    .iter()
                    .any(|(_, new)| *new == Some("fn new_value() {}"))
            );
        }
    }
}
