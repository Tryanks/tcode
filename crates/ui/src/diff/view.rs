//! The right-side diff panel view: scope controls, virtualized unified/split
//! lists, expandable gaps, and line-anchored review comments.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::highlight::HighlightTheme;
use crate::theme::ActiveTheme as _;
use crate::widgets::Popover;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::tooltip::Tooltip;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::FileChange;
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, HighlightStyle,
    InteractiveElement as _, IntoElement, ListOffset, ParentElement as _, Render, Role,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{ElementExt as _, PopoverState, StyledExt as _, h_flex, v_flex};
use serde::Deserialize;

use super::list::{
    DiffList, DiffListHost, DiffSelectionMenu, FileMenu, LineSelection, file_header_index,
    render_list, render_notice,
};
use super::model::{
    DiffColors, ExpandDir, FileDiffInput, RenderedFile, build_file, reconstruct_from_text,
};
use super::parse::RowKind;
use crate::agents_panel::{Agents, AgentsPanel};
use crate::plan_panel::PlanPanel;
use crate::store::WorkspaceStore;
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

/// Below this tab strip width (caption buttons excluded) the tabs show their
/// icons alone. All four tabs labelled, with two-digit counts, beside the
/// panel's own controls fit in this much in English, the widest shipped
/// locale, so no label is ever cut off; the default panel width is above it.
const TAB_LABELS_MIN_WIDTH: f32 = 540.;

/// Whether a strip last laid out `width` wide labels its tabs. Before the
/// first layout it does.
fn tab_labels_fit(width: Option<f32>, caption_width: f32) -> bool {
    width.is_none_or(|width| width - caption_width >= TAB_LABELS_MIN_WIDTH)
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
    let needs_reconstruction = texts.is_none_or(|texts| texts.old.is_none() || texts.new.is_none());
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
    /// The tab strip's laid-out width on the last frame, which decides whether
    /// the next one labels its tabs.
    strip_width: Rc<Cell<Option<f32>>>,
    _subscriptions: Vec<Subscription>,
}

impl DiffPanel {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let pull_requests = cx.new(|cx| {
            crate::pull_requests::PullRequestsPanel::new(
                workspace_store.clone(),
                window_state.clone(),
                cx,
            )
        });
        let plan = cx.new(|cx| PlanPanel::new(workspace_store.clone(), cx));
        let agents = cx.new(|cx| AgentsPanel::new(workspace_store.clone(), cx));
        let subscriptions = vec![cx.observe(&workspace_store, |this, store, cx| {
            let comments = store.read(cx).review_comments();
            if this.observed_review_comments != comments {
                this.observed_review_comments = comments;
                this.remeasure_lists();
            }
            cx.notify();
        })];
        Self {
            workspace_store,
            window_state,
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
            strip_width: Rc::new(Cell::new(None)),
            _subscriptions: subscriptions,
        }
    }

    fn selected_scope(&self, session: &str, cx: &App) -> Option<DiffScope> {
        self.scopes
            .get(session)
            .copied()
            .or_else(|| {
                self.workspace_store
                    .read(cx)
                    .diff_selected_turn()
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
                .pending_diff_focus()
                .filter(|request| request.session == session && request.turn == turn);
            let cwd = store
                .diff_active_state()
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
        if let Some(index) =
            file_header_index(&list.files, &list.unified_items, &request.path, &cwd)
        {
            list.unified_list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        if let Some(index) = file_header_index(&list.files, &list.split_items, &request.path, &cwd)
        {
            list.split_list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        self.workspace_store.update(cx, |store, _cx| {
            store.take_diff_focus(session, turn);
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
                    panel.cache = None;
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
        self.cache = None;
        self.render_loading_key = Some(key.clone());
        let workspace_store = self.workspace_store.clone();
        cx.spawn(async move |this, cx| {
            let mut fallback_texts = vec![None; changes.len()];
            for (index, change) in changes.iter().enumerate() {
                if texts
                    .get(index)
                    .is_some_and(|text| text.old.is_some() && text.new.is_some())
                {
                    continue;
                }
                let path = PathBuf::from(&change.path);
                let path = if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                };
                let task = workspace_store.update(cx, |store, cx| store.read_file_bytes(path, cx));
                let Ok(bytes) = task.await else {
                    continue;
                };
                if bytes.len() <= 512 * 1024
                    && let Ok(text) = String::from_utf8(bytes)
                {
                    fallback_texts[index] = Some(text);
                }
            }
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
                    panel.cache = Some(DiffCache {
                        session: key.session.clone(),
                        scope: key.scope,
                        revision: key.revision,
                        dark: key.dark,
                        ignore_ws: key.ignore_ws,
                        show_invisibles: key.show_invisibles,
                        list: DiffList::new(files, Vec::new()),
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
        let pending_focus = self.workspace_store.read(cx).pending_diff_focus();
        if let Some(request) = pending_focus {
            let is_active = self
                .workspace_store
                .read(cx)
                .active_session_id()
                .is_some_and(|session| session == request.session);
            if is_active {
                self.scopes
                    .insert(request.session.clone(), DiffScope::Turn(request.turn));
            } else {
                self.workspace_store.update(cx, |store, cx| {
                    store.discard_diff_focus(cx);
                });
            }
        }
        let dark = cx.theme().mode.is_dark();
        let (session, scope, revision, cwd) = {
            let store = self.workspace_store.read(cx);
            let Some(active) = store.diff_active_state() else {
                self.cache = None;
                return false;
            };
            let session = active.session;
            let Some(scope) = self.selected_scope(&session, cx) else {
                self.cache = None;
                return false;
            };
            (session, scope, store.diff_refresh_generation(), active.cwd)
        };
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
                self.cache = None;
                return false;
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
            let appearance = RenderAppearance {
                theme: cx.theme().highlight_theme.clone(),
                colors: DiffColors {
                    added_word_bg: cx.theme().success.opacity(0.30),
                    removed_word_bg: cx.theme().danger.opacity(0.28),
                },
                whitespace_style: HighlightStyle {
                    color: Some(cx.theme().muted_foreground),
                    ..Default::default()
                },
            };
            let (changes, texts) = match scope {
                DiffScope::Turn(turn) => {
                    let changes = self
                        .workspace_store
                        .read(cx)
                        .with_diff_turn_changes(turn, |changes, completeness| {
                            let _completeness = completeness;
                            changes.to_vec()
                        })
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
        let chrome = self.workspace_store.read(cx).panel_state();
        let panel_open = chrome.right_panel_open;
        let expanded = chrome.right_panel_expanded;
        let active = chrome.right_tab;
        // Windows: the open Diff/Plan panel is the rightmost column, so this
        // strip hosts the caption buttons. It is shorter than the 52px shell
        // header, so grow it to match — the buttons must reach the window top,
        // and a taller strip keeps the tabs aligned with the chat header.
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::RightPanel,
            self.window_state.read(cx).route(),
            panel_open,
            active,
        );
        let store = self.workspace_store.clone();
        let store_close = self.workspace_store.clone();
        let store_diff = self.workspace_store.clone();
        let store_plan = self.workspace_store.clone();
        let store_pr = self.workspace_store.clone();
        let store_agents = self.workspace_store.clone();
        let agents = {
            let store = self.workspace_store.read(cx);
            store.active_session_id().and_then(|id| {
                let agents = Agents::of(store, &id);
                (!agents.is_empty() || active == RightTab::Agents).then(|| agents.outstanding())
            })
        };
        let muted = cx.theme().muted_foreground;
        let tab_active = cx.theme().tab_active;
        let caption_width = if hosts_caption {
            window_caption::CAPTION_CLUSTER_WIDTH
        } else {
            0.
        };
        let show_labels = tab_labels_fit(self.strip_width.get(), caption_width);

        // Without its label a tab is its icon, named by its tooltip and its
        // accessibility label.
        let labelled = |id: &'static str,
                        icon: IconName,
                        label: gpui::SharedString,
                        content: AnyElement,
                        is_active: bool,
                        cx: &mut Context<Self>|
         -> gpui_base::Tab {
            material::tab(id, label.clone(), is_active, cx)
                .debug_selector(move || id.into())
                .flex_none()
                .h(px(28.))
                .when(show_labels, |s| s.px_2p5().gap_1p5())
                .when(!show_labels, |s| s.w(px(28.)))
                .rounded(material::radius_button(cx))
                .text_size(px(13.))
                .font_medium()
                .when(is_active, |s| s.bg(tab_active))
                .when(!is_active, |s| {
                    s.text_color(muted).hover(|s| s.bg(cx.theme().muted))
                })
                .child(Icon::new(icon).xsmall().text_color(muted))
                .map(|s| {
                    if show_labels {
                        s.child(content)
                    } else {
                        s.tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
                    }
                })
        };
        let tab = |id, icon, label: gpui::SharedString, is_active, cx: &mut Context<Self>| {
            labelled(
                id,
                icon,
                label.clone(),
                label.into_any_element(),
                is_active,
                cx,
            )
        };

        let tabs = gpui_base::Tabs::new("right-panel-tabs")
            .flex()
            .items_center()
            .debug_selector(|| "right-panel-tabs".into())
            .aria_label(crate::tr!("diff.panel_tabs"))
            // The tabs give way before the window controls do, so the close
            // button stays on screen at any panel width. The 2px inset keeps
            // the focus ring of the first tab inside the clip.
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .px(px(2.))
            .ml(px(-2.))
            .gap_1()
            .child(
                tab(
                    "diff-tab",
                    IconName::File,
                    crate::tr!("diff.title").into_owned().into(),
                    active == RightTab::Diff,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    store_diff.update(cx, |store, cx| {
                        store.set_right_tab(RightTab::Diff, cx);
                    });
                }),
            )
            .child(
                tab(
                    "plan-tab",
                    IconName::Map,
                    crate::tr!("tasks.tab").into_owned().into(),
                    active == RightTab::Plan,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    store_plan.update(cx, |store, cx| {
                        store.set_right_tab(RightTab::Plan, cx);
                    });
                }),
            )
            .when_some(agents, |strip, outstanding| {
                let is_active = active == RightTab::Agents;
                let tab = if outstanding == 0 {
                    tab(
                        "agents-tab",
                        IconName::Bot,
                        crate::tr!("agents.title").into_owned().into(),
                        is_active,
                        cx,
                    )
                } else {
                    // The localized pattern places the count; only the count is muted.
                    let pattern = crate::tr!("agents.tab_count", count = "\u{0}").into_owned();
                    let (before, after) = pattern.split_once('\u{0}').unwrap_or((&pattern, ""));
                    let content = h_flex()
                        .gap_1()
                        .children((!before.trim().is_empty()).then(|| before.trim().to_owned()))
                        .child(div().text_color(muted).child(outstanding.to_string()))
                        .children((!after.trim().is_empty()).then(|| after.trim().to_owned()))
                        .into_any_element();
                    labelled(
                        "agents-tab",
                        IconName::Bot,
                        crate::tr!("agents.tab_count", count = outstanding.to_string())
                            .into_owned()
                            .into(),
                        content,
                        is_active,
                        cx,
                    )
                };
                strip.child(tab.on_click(move |_, _, cx| {
                    store_agents.update(cx, |store, cx| {
                        store.set_right_tab(RightTab::Agents, cx);
                    });
                }))
            })
            .child({
                let count = self
                    .workspace_store
                    .read(cx)
                    .active_session_id()
                    .map_or(0, |id| {
                        self.workspace_store
                            .read(cx)
                            .pull_requests(&id)
                            .iter()
                            .filter(|link| link.visible())
                            .count()
                    });
                let title = crate::tr!("pull_requests.title").into_owned();
                let is_active = active == RightTab::PullRequests;
                if count == 0 {
                    tab(
                        "pull-requests-tab",
                        IconName::GitPullRequest,
                        title.into(),
                        is_active,
                        cx,
                    )
                } else {
                    // The localized pattern places the count; only the count is muted.
                    let pattern =
                        crate::tr!("pull_requests.tab_count", count = "\u{0}").into_owned();
                    let (before, after) = pattern.split_once('\u{0}').unwrap_or((&pattern, ""));
                    let content = h_flex()
                        .gap_1()
                        .children((!before.trim().is_empty()).then(|| before.trim().to_owned()))
                        .child(div().text_color(muted).child(count.to_string()))
                        .children((!after.trim().is_empty()).then(|| after.trim().to_owned()))
                        .into_any_element();
                    labelled(
                        "pull-requests-tab",
                        IconName::GitPullRequest,
                        crate::tr!("pull_requests.tab_count", count = count.to_string())
                            .into_owned()
                            .into(),
                        content,
                        is_active,
                        cx,
                    )
                }
                .on_click(move |_, _, cx| {
                    store_pr.update(cx, |store, cx| {
                        store.set_right_tab(RightTab::PullRequests, cx)
                    })
                })
            });

        let strip_width = self.strip_width.clone();
        h_flex()
            .flex_none()
            .h(px(if hosts_caption {
                window_caption::CAPTION_STRIP_HEIGHT
            } else {
                40.
            }))
            .w_full()
            .px_2()
            .when(hosts_caption, |strip| strip.pr_0())
            .gap_1()
            .on_prepaint(move |bounds, window, _| {
                let width = Some(f32::from(bounds.size.width));
                let previous = strip_width.replace(width);
                if tab_labels_fit(previous, caption_width) != tab_labels_fit(width, caption_width) {
                    window.request_animation_frame();
                }
            })
            .child(tabs)
            // The gap between the tabs and the icon cluster holds nothing, so
            // it doubles as the window's drag handle: `window_drag_area` for the
            // app-owned move (macOS), `drag_region` for native HTCAPTION
            // (Windows). `h_full` is load-bearing: the strip centers its
            // children, so without it the drag hitbox collapses to zero height.
            .child(window_caption::drag_region(crate::window_drag_area(
                "right-panel-tabs-drag",
                div().flex_1().h_full(),
                window,
                cx,
            )))
            // Right icon cluster: expand toggle, a layout no-op, close.
            .child(
                Button::new("diff-expand")
                    .ghost()
                    .small()
                    .compact()
                    .icon(if expanded {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    })
                    .tooltip(if expanded {
                        crate::tr!("diff.restore_width")
                    } else {
                        crate::tr!("diff.expand_width")
                    })
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            store.toggle_diff_expanded(cx);
                        });
                    }),
            )
            .child(
                Button::new("diff-layout")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelRight)
                    .tooltip(crate::tr!("diff.layout_soon")),
            )
            .child(
                Button::new("diff-close")
                    .debug_selector(|| "diff-close".into())
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(crate::tr!("diff.close"))
                    .on_click(move |_, _, cx| {
                        store_close.update(cx, |store, cx| {
                            store.close_diff_panel(cx);
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
        match self.workspace_store.read(cx).diff_active_state() {
            Some(active) => active.cwd.join(path).to_string_lossy().into_owned(),
            None => file.path.clone(),
        }
    }

    fn apply_view_option(&mut self, option: DiffViewOption, cx: &mut Context<Self>) {
        match option {
            DiffViewOption::Split => {
                let split = self.workspace_store.read(cx).diff_split();
                self.workspace_store
                    .update(cx, |store, cx| store.set_diff_split(!split, cx));
                self.remeasure_lists();
            }
            DiffViewOption::Wrap => {
                self.workspace_store
                    .update(cx, |store, cx| store.toggle_diff_wrap(cx));
                self.remeasure_lists();
            }
            DiffViewOption::Whitespace => {
                self.ignore_ws = !self.ignore_ws;
                self.git_preview = None;
                self.cache = None;
            }
            DiffViewOption::Invisibles => {
                self.show_invisibles = !self.show_invisibles;
                self.cache = None;
            }
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let active_state = self.workspace_store.read(cx).diff_active_state();
        let session = active_state
            .as_ref()
            .map(|active| active.session.clone())
            .unwrap_or_default();
        let selected_scope = self.selected_scope(&session, cx);
        let turns: Rc<[usize]> = self
            .workspace_store
            .read(cx)
            .diff_turns()
            .into_iter()
            .rev()
            .collect();
        let label = match selected_scope {
            Some(DiffScope::Turn(turn)) => crate::tr!("diff.turn", count = turn + 1).into_owned(),
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
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .font_medium()
                    .child(label)
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );

        let selector = Popover::new("diff-turn-popover")
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
                        material::accessible_clickable(
                            h_flex(),
                            id,
                            Role::MenuItem,
                            label.clone(),
                            cx,
                        )
                        .aria_selected(selected_scope == Some(scope))
                        .flex_none()
                        .w_full()
                        .px_2()
                        .py_1()
                        .items_center()
                        .rounded(cx.theme().tokens.radius.sm)
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|row| row.bg(cx.theme().list_hover))
                        .when(selected_scope == Some(scope), |row| {
                            row.bg(cx.theme().list_active)
                        })
                        .child(div().flex_1().child(label))
                        .when(selected_scope == Some(scope), |row| {
                            row.child(Icon::new(IconName::Check).xsmall())
                        })
                        .on_click({
                            let popover = cx.entity();
                            move |_, window, cx| {
                                panel.update(cx, |this, cx| {
                                    this.scopes.insert(session.clone(), scope);
                                    this.cache = None;
                                    this.workspace_store.update(cx, |store, cx| {
                                        store.discard_diff_focus(cx);
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
                    .min_w(px(190.))
                    .max_h(px(320.))
                    .pt_1()
                    .px_1()
                    // The last turn row carries its gap.
                    .pb(px(if turn_rows.is_some() { 2. } else { 4. }))
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
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("diff.turns")),
                    )
                    .children(turn_rows)
            })
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .rounded(material::radius_overlay(cx));

        let base_selector =
            (selected_scope == Some(DiffScope::Branch)).then(|| {
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
                            .and_then(|p| p.result.default_base.clone())
                    })
                    .unwrap_or_else(|| "HEAD".to_string());
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
                                                div().pb_0p5().child(base_row(
                                                    index,
                                                    branch,
                                                    *branch == *current,
                                                    &panel,
                                                    &session,
                                                    &popover,
                                                    cx,
                                                ))
                                            })
                                            .collect()
                                    },
                                )
                                .width_from_row(widest)
                                .min_w(px(180.))
                                .max_h(px(280.))
                                .pt_1()
                                .px_1()
                                // The last row carries its gap.
                                .pb(px(if branch_count == 0 { 4. } else { 2. })),
                            )
                    })
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_xl()
                    .rounded(material::radius_overlay(cx))
                    .into_any_element()
            });

        let wrap_on = self.workspace_store.read(cx).diff_word_wrap();
        let split_on = self.workspace_store.read(cx).diff_split();
        let compact = self.compact(cx);
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
        let mut toolbar = h_flex()
            .flex_none()
            .h(px(if compact { material::TOUCH_TARGET } else { 40. }))
            .w_full()
            .px(px(if compact {
                material::COMPACT_PAGE_INSET
            } else {
                8.
            }))
            .gap_1()
            .items_center()
            .child(selector);
        if compact {
            // The scope and base pickers are the row; everything else is one
            // overflow menu, so a 393pt page never has to choose what to clip.
            toolbar = toolbar.children(base_selector).child(div().flex_1()).child(
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
                    Button::new(id)
                        .ghost()
                        .small()
                        .compact()
                        .icon(icon)
                        .selected(on)
                        .tooltip(label)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.apply_view_option(option, cx)),
                        ),
                );
            }
            toolbar = toolbar.children(base_selector);
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
            .map(|(_, row)| (row.kind, row.old, row.new, &row.text))
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
        let (section_id, section_title) = match self.cache.as_ref().map(|cache| cache.scope) {
            Some(DiffScope::Turn(turn)) => (format!("turn:{turn}"), format!("Turn {}", turn + 1)),
            Some(DiffScope::WorkingTree) => ("unstaged".to_string(), "Working tree".to_string()),
            Some(DiffScope::Branch) => ("branch".to_string(), "Branch changes".to_string()),
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
        self.workspace_store.update(cx, |store, _cx| {
            store.add_review_comment(comment);
        });
        if let Some(cache) = self.cache.as_mut() {
            cache.list.selection = None;
        }
        self.comment_input = None;
        self.remeasure_lists();
        cx.notify();
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            if self.loading_key.is_some() || self.render_loading_key.is_some() {
                return self.render_status(crate::tr!("diff.loading").into_owned(), cx);
            }
            return self.render_empty(cx);
        };
        let viewport = if cache.list.files.is_empty() {
            self.render_empty(cx)
        } else {
            let split = self.workspace_store.read(cx).diff_split();
            let wrap = self.workspace_store.read(cx).diff_word_wrap();
            render_list(self, "diff-body", split, wrap, cx)
        };
        // A compact page holds its content clear of the window edges; the code
        // itself still scrolls sideways *inside* that inset rather than running
        // off the page.
        let mut content = v_flex()
            .size_full()
            .min_h_0()
            .when(self.compact(cx), |body| {
                body.px(px(material::COMPACT_PAGE_INSET))
            })
            .child(viewport);

        if let Some(preview) = self
            .git_preview
            .as_ref()
            .filter(|preview| preview.session == cache.session && preview.scope == cache.scope)
        {
            if preview.result.truncated {
                content =
                    content.child(render_notice(crate::tr!("diff.truncated").into_owned(), cx));
            }
            if let Some(error) = &preview.result.error {
                content = content.child(render_notice(error.clone(), cx));
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
            .workspace_store
            .read(cx)
            .review_comments()
            .iter()
            .filter(|comment| {
                comment.file == file
                    && match comment.side {
                        ReviewSide::Old => old,
                        ReviewSide::New => new,
                    } == Some(comment.line_end)
            })
            .map(|comment| {
                h_flex()
                    .min_w_full()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .relative()
                    .rounded(material::radius_card(cx))
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().font_family.clone())
                    .text_size(px(11.))
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
                    .child(Icon::empty().path("icons/pencil.svg").xsmall())
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
                    v_flex()
                        .min_w_full()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .bg(cx.theme().muted)
                        .rounded(material::radius_card(cx))
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
                    .text_size(px(15.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("diff.empty")),
            )
            .into_any_element()
    }
}

fn turn_row(
    turn: usize,
    selected: bool,
    panel: &Entity<DiffPanel>,
    session: &str,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let panel = panel.clone();
    let session = session.to_string();
    let popover = popover.clone();
    let label: gpui::SharedString = crate::tr!("diff.turn", count = turn + 1)
        .into_owned()
        .into();
    material::accessible_clickable(
        h_flex(),
        ("diff-turn-item", turn),
        Role::MenuItem,
        label.clone(),
        cx,
    )
    .aria_selected(selected)
    .flex_none()
    .w_full()
    .px_2()
    .py_1()
    .gap_2()
    .items_center()
    .rounded(cx.theme().tokens.radius.sm)
    .text_size(px(13.))
    .cursor_pointer()
    .hover(|s| s.bg(cx.theme().list_hover))
    .when(selected, |this| this.bg(cx.theme().list_active))
    .child(div().flex_1().min_w_0().truncate().child(label))
    .when(selected, |this| {
        this.child(Icon::new(IconName::Check).xsmall())
    })
    .on_click(move |_, window, cx| {
        panel.update(cx, |this, cx| {
            this.scopes.insert(session.clone(), DiffScope::Turn(turn));
            this.cache = None;
            this.workspace_store.update(cx, |store, cx| {
                store.select_diff_turn(turn, cx);
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
    panel: &Entity<DiffPanel>,
    session: &str,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let panel = panel.clone();
    let session = session.to_string();
    let popover = popover.clone();
    let chosen = branch.to_string();
    material::accessible_clickable(
        h_flex(),
        ("diff-base-item", index),
        Role::MenuItem,
        crate::tr!("diff.base_branch", branch = branch).into_owned(),
        cx,
    )
    .aria_selected(selected)
    .flex_none()
    .w_full()
    .px_2()
    .py_1()
    .rounded(cx.theme().tokens.radius.sm)
    .cursor_pointer()
    .hover(|row| row.bg(cx.theme().list_hover))
    .when(selected, |row| row.bg(cx.theme().list_active))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(branch.to_string()),
    )
    .when(selected, |row| {
        row.child(Icon::new(IconName::Check).xsmall())
    })
    .on_click(move |_, window, cx| {
        panel.update(cx, |this, cx| {
            this.bases.insert(session.clone(), chosen.clone());
            this.cache = None;
            this.git_preview = None;
            cx.notify();
        });
        popover.update(cx, |state, cx| state.dismiss(window, cx));
    })
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
            .diff_active_state()
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
        let tab = self.workspace_store.read(cx).panel_state().right_tab;
        // Compact: the Panel page's segmented control is the only selector, and
        // Back is the only way off the page. This panel's own tab row and its
        // expand / split / close cluster are wide-layout affordances, so the
        // whole strip stays unbuilt rather than being drawn and ignored.
        let compact = self.compact(cx);
        let mut root = v_flex()
            .size_full()
            .min_w_0()
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_view_option))
            .on_action(cx.listener(Self::on_selection_menu))
            .when(!compact, |root| {
                root.child(self.render_tab_strip(window, cx))
            });
        root = match tab {
            // AppShell mounts Preview separately; this container handles Diff/Plan.
            RightTab::Diff | RightTab::Preview => root
                .child(self.render_toolbar(cx))
                .child(self.render_body(cx)),
            RightTab::PullRequests => {
                root.child(div().flex_1().min_h_0().child(self.pull_requests.clone()))
            }
            RightTab::Plan => root.child(div().flex_1().min_h_0().child(self.plan.clone())),
            RightTab::Agents => root.child(div().flex_1().min_h_0().child(self.agents.clone())),
        };
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::list::build_list_items_with;
    use agent::FileChangeKind;

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
        assert_eq!(file.added, 1);
        assert_eq!(file.removed, 1);
        assert_eq!(file.all_rows.len(), 2);
        let items = build_list_items_with(std::slice::from_ref(&file), &[]);
        assert_eq!(items.unified.len(), 3, "header plus two visible rows");
        assert_eq!(items.split.len(), 2, "header plus one paired row");
    }
}
