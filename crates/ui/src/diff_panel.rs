//! The right-side diff panel: a resizable pane that renders the file changes
//! of a selected turn as a syntax-highlighted unified diff (see docs/DESIGN.md
//! "Diff panel").
//!
//! The unified-diff parser ([`parse_unified_diff`]) is hand-written (no deps).
//! It accepts both proper unified diffs (with `@@` hunk headers, `+++`/`---`
//! file headers, space-prefixed context, and `\ No newline at end of file`
//! markers) and the header-less `+`/`-` line diffs that the Claude provider
//! emits for Write/Edit tool calls. Line rows carry both old and new line
//! numbers; the count of unmodified lines skipped between hunks drives the
//! "N unmodified lines" separator rows.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::{FileChange, FileChangeKind};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, HighlightStyle, InteractiveElement as _,
    IntoElement, ListAlignment, ListOffset, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _, StyledText,
    Subscription, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputState},
    popover::Popover,
    v_flex,
};

use crate::diff::model::{
    DiffColors, FileDiffInput, PairedRow, RenderedFile, RenderedRow, VisibleItem, VisibleSplitItem,
    build_file, diff_content_widths, reconstruct_from_disk, visible_split, visible_unified,
};
pub use crate::diff::parse::RowKind;
use crate::plan_panel::PlanPanel;
use crate::window_caption;
use crate::{highlight, material};
use tcode_core::session::{ReviewComment, ReviewSide};
use tcode_runtime::app::{AppState, RightTab};
use tcode_runtime::ui_facade::{
    GitDiffResult, GitDiffScope, GitFileText, load_git_diff, relativize_to_workspace,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DiffScope {
    Turn(usize),
    WorkingTree,
    Branch,
}

fn render_file(
    change: &FileChange,
    texts: Option<&GitFileText>,
    cwd: &Path,
    theme: &HighlightTheme,
    colors: &DiffColors,
    whitespace_style: &HighlightStyle,
) -> RenderedFile {
    let needs_reconstruction = texts.is_none_or(|texts| texts.old.is_none() || texts.new.is_none());
    let reconstructed = needs_reconstruction
        .then(|| {
            change
                .diff
                .as_deref()
                .and_then(|patch| reconstruct_from_disk(Path::new(&change.path), patch))
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
            ignore_whitespace: false,
            show_invisibles: false,
        },
        relativize_to_workspace(&change.path, cwd),
        highlight::language_name_for_path(&change.path),
        theme,
        colors,
        whitespace_style,
    )
}

// ---------------------------------------------------------------------------
// Panel entity
// ---------------------------------------------------------------------------

/// Cache of rendered files, invalidated when the session, selected turn, or
/// theme brightness changes (highlight colors are theme-resolved).
struct DiffCache {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
    files: Vec<RenderedFile>,
    unified_visible: Vec<Vec<VisibleItem>>,
    split_visible: Vec<Vec<VisibleSplitItem>>,
    unified_items: Vec<DiffListItem>,
    split_items: Vec<DiffListItem>,
    unified_content_width: f32,
    split_content_width: f32,
    unified_list: ListState,
    split_list: ListState,
}

#[derive(Debug, Clone, Copy)]
enum DiffListItem {
    Header(usize),
    UnifiedRow { file: usize, row: usize },
    SplitRow { file: usize, row: usize },
}

fn build_list_items(files: &[RenderedFile]) -> BuiltListItems {
    let unified_visible = files.iter().map(visible_unified).collect::<Vec<_>>();
    let split_visible = files.iter().map(visible_split).collect::<Vec<_>>();
    let unified_capacity = files.len() + unified_visible.iter().map(Vec::len).sum::<usize>();
    let split_capacity = files.len() + split_visible.iter().map(Vec::len).sum::<usize>();
    let mut unified = Vec::with_capacity(unified_capacity);
    let mut split = Vec::with_capacity(split_capacity);
    for (file_index, _) in files.iter().enumerate() {
        unified.push(DiffListItem::Header(file_index));
        split.push(DiffListItem::Header(file_index));
        unified.extend((0..unified_visible[file_index].len()).map(|row| {
            DiffListItem::UnifiedRow {
                file: file_index,
                row,
            }
        }));
        split.extend(
            (0..split_visible[file_index].len()).map(|row| DiffListItem::SplitRow {
                file: file_index,
                row,
            }),
        );
    }
    BuiltListItems {
        unified_visible,
        split_visible,
        unified,
        split,
    }
}

struct BuiltListItems {
    unified_visible: Vec<Vec<VisibleItem>>,
    split_visible: Vec<Vec<VisibleSplitItem>>,
    unified: Vec<DiffListItem>,
    split: Vec<DiffListItem>,
}

fn file_header_index(files: &[RenderedFile], items: &[DiffListItem], path: &str) -> Option<usize> {
    let file_index = files.iter().position(|file| file.path == path)?;
    items
        .iter()
        .position(|item| matches!(item, DiffListItem::Header(index) if *index == file_index))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    session: String,
    scope: DiffScope,
    revision: u64,
    dark: bool,
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
    result: GitDiffResult,
}

#[derive(Clone)]
struct CommentSelection {
    file: String,
    row_start: usize,
    row_end: usize,
    line_start: u32,
    line_end: u32,
    side: ReviewSide,
    start_index: usize,
    end_index: usize,
}

pub struct DiffPanel {
    app_state: Entity<AppState>,
    /// The Plan/Tasks tab content (the other tab in this right panel).
    plan: Entity<PlanPanel>,
    /// Soft-wrap toggle for long code lines (the one real toolbar button).
    wrap: bool,
    scopes: HashMap<String, DiffScope>,
    split: HashMap<String, bool>,
    bases: HashMap<String, String>,
    cache: Option<DiffCache>,
    git_preview: Option<GitPreview>,
    loading_key: Option<(String, DiffScope, Option<String>, u64)>,
    render_loading_key: Option<RenderKey>,
    selection: Option<CommentSelection>,
    comment_input: Option<Entity<InputState>>,
    observed_review_comments: Vec<ReviewComment>,
    _subscriptions: Vec<Subscription>,
}

impl DiffPanel {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Soft-wrap defaults to the user's "Word wrap in diffs" setting.
        let wrap = app_state.read(cx).settings.word_wrap_diffs;
        let plan = cx.new(|cx| PlanPanel::new(app_state.clone(), cx));
        let subscriptions = vec![cx.observe(&app_state, |this, state, cx| {
            let comments = state.read(cx).review_comments();
            if this.observed_review_comments != comments {
                this.observed_review_comments = comments.to_vec();
                this.remeasure_lists();
            }
            cx.notify();
        })];
        Self {
            app_state,
            plan,
            wrap,
            scopes: HashMap::new(),
            split: HashMap::new(),
            bases: HashMap::new(),
            cache: None,
            git_preview: None,
            loading_key: None,
            render_loading_key: None,
            selection: None,
            comment_input: None,
            observed_review_comments: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    fn selected_scope(&self, state: &AppState, session: &str) -> Option<DiffScope> {
        self.scopes
            .get(session)
            .copied()
            .or_else(|| state.diff_selected_turn().map(DiffScope::Turn))
            .or(Some(DiffScope::WorkingTree))
    }

    fn remeasure_lists(&self) {
        if let Some(cache) = &self.cache {
            cache.unified_list.remeasure();
            cache.split_list.remeasure();
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
        let request = self
            .app_state
            .read(cx)
            .pending_diff_focus()
            .filter(|request| request.session == session && request.turn == turn)
            .cloned();
        let Some(request) = request else {
            return;
        };
        let Some(cache) = self
            .cache
            .as_ref()
            .filter(|cache| cache.session == session && cache.scope == scope)
        else {
            return;
        };
        if let Some(index) = file_header_index(&cache.files, &cache.unified_items, &request.path) {
            cache.unified_list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        if let Some(index) = file_header_index(&cache.files, &cache.split_items, &request.path) {
            cache.split_list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
        self.app_state.update(cx, |state, _| {
            state.take_diff_focus(session, turn);
        });
    }

    fn request_git_preview(
        &mut self,
        session: String,
        cwd: PathBuf,
        scope: DiffScope,
        base: Option<String>,
        revision: u64,
        cx: &mut Context<Self>,
    ) {
        let runtime_scope = match scope {
            DiffScope::WorkingTree => GitDiffScope::WorkingTree,
            DiffScope::Branch => GitDiffScope::Branch,
            DiffScope::Turn(_) => return,
        };
        let key = (session.clone(), scope, base.clone(), revision);
        if self.loading_key.as_ref() == Some(&key)
            || self.git_preview.as_ref().is_some_and(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == revision
            })
        {
            return;
        }
        self.loading_key = Some(key.clone());
        cx.spawn(async move |this, cx| {
            let result = tcode_runtime::blocking::unblock(cx.background_executor(), move || {
                load_git_diff(&cwd, runtime_scope, base.as_deref())
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.loading_key.as_ref() == Some(&key) {
                    panel.git_preview = Some(GitPreview {
                        session,
                        scope,
                        base: key.2.clone(),
                        revision,
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
        cx.spawn(async move |this, cx| {
            let (
                files,
                unified_visible,
                split_visible,
                unified_items,
                split_items,
                unified_content_width,
                split_content_width,
            ) = tcode_runtime::blocking::unblock(cx.background_executor(), move || {
                let files = changes
                    .iter()
                    .enumerate()
                    .map(|(index, change)| {
                        render_file(
                            change,
                            texts.get(index),
                            &cwd,
                            &appearance.theme,
                            &appearance.colors,
                            &appearance.whitespace_style,
                        )
                    })
                    .collect::<Vec<_>>();
                let items = build_list_items(&files);
                let (unified_content_width, split_content_width) = diff_content_widths(&files);
                (
                    files,
                    items.unified_visible,
                    items.split_visible,
                    items.unified,
                    items.split,
                    unified_content_width,
                    split_content_width,
                )
            })
            .await;
            let _ = this.update(cx, |panel, cx| {
                if panel.render_loading_key.as_ref() == Some(&key) {
                    let unified_list =
                        ListState::new(unified_items.len(), ListAlignment::Top, px(180.));
                    let split_list =
                        ListState::new(split_items.len(), ListAlignment::Top, px(180.));
                    panel.cache = Some(DiffCache {
                        session: key.session.clone(),
                        scope: key.scope,
                        revision: key.revision,
                        dark: key.dark,
                        files,
                        unified_visible,
                        split_visible,
                        unified_items,
                        split_items,
                        unified_content_width,
                        split_content_width,
                        unified_list,
                        split_list,
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
        let pending_focus = self.app_state.read(cx).pending_diff_focus().cloned();
        if let Some(request) = pending_focus {
            let is_active = self
                .app_state
                .read(cx)
                .active_session_id()
                .is_some_and(|session| session == request.session);
            if is_active {
                self.scopes
                    .insert(request.session.clone(), DiffScope::Turn(request.turn));
            } else {
                self.app_state
                    .update(cx, |state, _| state.discard_diff_focus());
            }
        }
        let debug = {
            let state = self.app_state.read(cx);
            state.active.as_ref().map(|active| {
                (
                    active.meta.id.clone(),
                    state.debug_diff_scope.clone(),
                    state.debug_diff_split,
                )
            })
        };
        if let Some((session, scope, split)) = debug {
            if !self.scopes.contains_key(&session) {
                match scope.as_deref() {
                    Some("working") | Some("working-tree") => {
                        self.scopes.insert(session.clone(), DiffScope::WorkingTree);
                    }
                    Some("branch") => {
                        self.scopes.insert(session.clone(), DiffScope::Branch);
                    }
                    _ => {}
                }
            }
            if split {
                self.split.insert(session, true);
            }
        }
        let dark = cx.theme().mode.is_dark();
        let (session, scope, revision, mut changes, mut texts, cwd) = {
            let state = self.app_state.read(cx);
            let Some(active) = state.active.as_ref() else {
                self.cache = None;
                return false;
            };
            let session = active.meta.id.clone();
            let Some(scope) = self.selected_scope(state, &session) else {
                self.cache = None;
                return false;
            };
            let changes = match scope {
                DiffScope::Turn(turn) => active
                    .timeline
                    .turns
                    .get(turn)
                    .and_then(|turn| turn.changes.as_ref())
                    .map(|changes| changes.changes.clone())
                    .unwrap_or_default(),
                DiffScope::WorkingTree | DiffScope::Branch => Vec::new(),
            };
            (
                session,
                scope,
                state.diff_refresh_generation,
                changes,
                Vec::new(),
                active.meta.cwd.clone(),
            )
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
                revision,
                cx,
            );
            let Some(preview) = self.git_preview.as_ref().filter(|preview| {
                preview.session == session
                    && preview.scope == scope
                    && preview.base == base
                    && preview.revision == revision
            }) else {
                self.cache = None;
                return false;
            };
            changes = preview.result.changes.clone();
            texts = preview.result.texts.clone();
        }

        let fresh = self.cache.as_ref().is_none_or(|c| {
            c.session != session || c.scope != scope || c.revision != revision || c.dark != dark
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
            self.request_rendered_files(
                RenderKey {
                    session,
                    scope,
                    revision,
                    dark,
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
        let debug_comment = self.app_state.read(cx).debug_review_comment
            && self.app_state.read(cx).review_comments().is_empty();
        if debug_comment
            && let Some((scope, file, row_index, line, side, text)) =
                self.cache.as_ref().and_then(|cache| {
                    cache.files.iter().find_map(|file| {
                        file.all_rows
                            .iter()
                            .enumerate()
                            .find_map(|(index, row)| match row {
                                RenderedRow::Code {
                                    kind,
                                    old,
                                    new,
                                    text,
                                    ..
                                } => {
                                    let (line, side) = match kind {
                                        RowKind::Removed => ((*old)?, ReviewSide::Old),
                                        RowKind::Added | RowKind::Context => {
                                            ((*new)?, ReviewSide::New)
                                        }
                                    };
                                    Some((
                                        cache.scope,
                                        file.path.clone(),
                                        index,
                                        line,
                                        side,
                                        text.clone(),
                                    ))
                                }
                                RenderedRow::Gap(_) => None,
                            })
                    })
                })
        {
            let (section_id, section_title) = match scope {
                DiffScope::Turn(turn) => (format!("turn:{turn}"), format!("Turn {}", turn + 1)),
                DiffScope::WorkingTree => ("unstaged".into(), "Working tree".into()),
                DiffScope::Branch => ("branch".into(), "Branch changes".into()),
            };
            let marker = if side == ReviewSide::Old { '-' } else { '+' };
            let excerpt = format!("@@ -{line},1 +{line},1 @@\n{marker}{text}");
            let comment = ReviewComment::new(
                file,
                line,
                line,
                side,
                "Please review this line before sending.".into(),
                excerpt,
                section_id,
                section_title,
                row_index,
                row_index,
            );
            self.app_state.update(cx, |state, cx| {
                state.debug_review_comment = false;
                state.add_review_comment(comment, cx);
            });
        }
        self.cache.as_ref().is_some_and(|c| !c.files.is_empty())
    }

    // -- top strip (tab look + right icon cluster) --------------------------

    fn render_tab_strip(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let expanded = state.diff_panel_expanded();
        let active = state.right_tab();
        // Windows: the open Diff/Plan panel is the rightmost column, so this
        // strip hosts the caption buttons. It is shorter than the 52px shell
        // header, so grow it to match — the buttons must reach the window top,
        // and a taller strip keeps the tabs aligned with the chat header.
        let hosts_caption =
            window_caption::hosts_caption(window_caption::CaptionSurface::RightPanel, state);
        // The second tab is "Plan" when a plan exists or the session is in Plan
        // mode, else "Tasks" (S1 §6).
        let plan_label = if state.plan_tab_active_label() {
            tcode_i18n::tr!("plan.tab_plan")
        } else {
            tcode_i18n::tr!("plan.tab_tasks")
        };
        let app = self.app_state.clone();
        let app2 = self.app_state.clone();
        let app_diff = self.app_state.clone();
        let app_plan = self.app_state.clone();
        let muted = cx.theme().muted_foreground;
        let tab_active = cx.theme().tab_active;

        let tab = |id: &'static str,
                   icon: IconName,
                   label: gpui::SharedString,
                   is_active: bool,
                   cx: &mut Context<Self>|
         -> gpui::Stateful<gpui::Div> {
            material::accessible_clickable(h_flex(), id, Role::Tab, label.clone(), cx)
                .aria_selected(is_active)
                .h(px(28.))
                .px_2p5()
                .gap_1p5()
                .items_center()
                .rounded(material::radius_button())
                .cursor_pointer()
                .text_size(px(13.))
                .font_medium()
                .when(is_active, |s| s.bg(tab_active))
                .when(!is_active, |s| {
                    s.text_color(muted).hover(|s| s.bg(cx.theme().muted))
                })
                .child(Icon::new(icon).xsmall().text_color(muted))
                .child(label)
        };

        h_flex()
            .id("right-panel-tabs")
            .role(Role::TabList)
            .aria_label(tcode_i18n::tr!("diff.panel_tabs"))
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
            .items_center()
            .child(
                tab(
                    "diff-tab",
                    IconName::File,
                    tcode_i18n::tr!("diff.title").into_owned().into(),
                    active == RightTab::Diff,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    app_diff.update(cx, |state, cx| state.set_right_tab(RightTab::Diff, cx));
                }),
            )
            .child(
                tab(
                    "plan-tab",
                    IconName::Map,
                    plan_label.into_owned().into(),
                    active == RightTab::Plan,
                    cx,
                )
                .on_click(move |_, _, cx| {
                    app_plan.update(cx, |state, cx| state.set_right_tab(RightTab::Plan, cx));
                }),
            )
            // The gap between the tabs and the icon cluster holds nothing, so
            // it doubles as the window's native drag handle where one is needed.
            // `h_full` is load-bearing: the strip centers its children, so
            // without it the drag hitbox collapses to zero height.
            .child(window_caption::drag_region(div().flex_1().h_full()))
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
                        tcode_i18n::tr!("diff.restore_width")
                    } else {
                        tcode_i18n::tr!("diff.expand_width")
                    })
                    .on_click(move |_, _, cx| {
                        app.update(cx, |state, cx| state.toggle_diff_expanded(cx));
                    }),
            )
            .child(
                Button::new("diff-layout")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelRight)
                    .tooltip(tcode_i18n::tr!("diff.layout_soon")),
            )
            .child(
                Button::new("diff-close")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(tcode_i18n::tr!("diff.close"))
                    .on_click(move |_, _, cx| {
                        app2.update(cx, |state, cx| state.close_diff_panel(cx));
                    }),
            )
            // Last child: the panel's own actions keep their places to its left.
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)))
            .into_any_element()
    }

    // -- toolbar (turn selector + view controls) ----------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let session = state
            .active
            .as_ref()
            .map(|active| active.meta.id.clone())
            .unwrap_or_default();
        let selected_scope = self.selected_scope(state, &session);
        let turns = state.diff_turns();
        let label = match selected_scope {
            Some(DiffScope::Turn(turn)) => {
                tcode_i18n::tr!("diff.turn", count = turn + 1).into_owned()
            }
            Some(DiffScope::WorkingTree) => tcode_i18n::tr!("diff.working_tree").into_owned(),
            Some(DiffScope::Branch) => tcode_i18n::tr!("diff.branch_changes").into_owned(),
            None => tcode_i18n::tr!("diff.no_changes").into_owned(),
        };
        let muted = cx.theme().muted_foreground;
        let panel = cx.entity();
        let session_selector = session.clone();

        let trigger = Button::new("diff-turn-select").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .font_medium()
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let selector = Popover::new("diff-turn-popover")
            .default_open(state.debug_diff_scope_menu)
            .trigger(trigger)
            .content(move |_, _, cx| {
                let panel_for = panel.clone();
                let session_for = session_selector.clone();
                let scope_row = |id: &'static str,
                                     label: gpui::SharedString,
                                     scope: DiffScope,
                                     cx: &mut gpui::Context<
                        gpui_component::popover::PopoverState,
                    >| {
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
                            .rounded(px(6.))
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
                                        this.selection = None;
                                        this.app_state
                                            .update(cx, |state, _| state.discard_diff_focus());
                                        cx.notify();
                                    });
                                    popover.update(cx, |state, cx| state.dismiss(window, cx));
                                }
                            })
                    };
                let mut list = v_flex()
                    .w_full()
                    .p_1()
                    .gap_0p5()
                    .child(scope_row(
                        "diff-scope-working",
                        tcode_i18n::tr!("diff.working_tree").into_owned().into(),
                        DiffScope::WorkingTree,
                        cx,
                    ))
                    .child(scope_row(
                        "diff-scope-branch",
                        tcode_i18n::tr!("diff.branch_changes").into_owned().into(),
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
                            .child(tcode_i18n::tr!("diff.turns")),
                    );
                let mut items = turns.clone();
                items.reverse();
                for turn in items {
                    let panel = panel.clone();
                    let session = session_selector.clone();
                    let is_sel = selected_scope == Some(DiffScope::Turn(turn));
                    let turn_label: gpui::SharedString =
                        tcode_i18n::tr!("diff.turn", count = turn + 1)
                            .into_owned()
                            .into();
                    list = list.child(
                        material::accessible_clickable(
                            h_flex(),
                            ("diff-turn-item", turn),
                            Role::MenuItem,
                            turn_label.clone(),
                            cx,
                        )
                        .aria_selected(is_sel)
                        .flex_none()
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .text_size(px(13.))
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().list_hover))
                        .when(is_sel, |this| this.bg(cx.theme().list_active))
                        .child(div().flex_1().child(turn_label))
                        .when(is_sel, |this| {
                            this.child(Icon::new(IconName::Check).xsmall())
                        })
                        .on_click({
                            let popover = cx.entity();
                            move |_, window, cx| {
                                panel.update(cx, |this, cx| {
                                    this.scopes.insert(session.clone(), DiffScope::Turn(turn));
                                    this.cache = None;
                                    this.selection = None;
                                    this.app_state
                                        .update(cx, |state, _| state.discard_diff_focus());
                                    cx.notify();
                                });
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                            }
                        }),
                    );
                }
                div()
                    .id("diff-turn-list")
                    .role(Role::Menu)
                    .aria_label(tcode_i18n::tr!("diff.scope_menu"))
                    .min_w(px(190.))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .child(list)
            })
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_xl()
            .rounded(material::radius_overlay());

        let wrap_on = self.wrap;
        let split_on = self.split.get(&session).copied().unwrap_or(false);
        let panel_split = cx.entity();
        let session_split = session.clone();
        let mut toolbar = h_flex()
            .flex_none()
            .h(px(40.))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .child(selector)
            .child(div().flex_1())
            .child(
                Button::new("diff-view-split")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::PanelLeft)
                    .selected(split_on)
                    .tooltip(if split_on {
                        tcode_i18n::tr!("diff.unified_view")
                    } else {
                        tcode_i18n::tr!("diff.split_view")
                    })
                    .on_click(move |_, _, cx| {
                        panel_split.update(cx, |this, cx| {
                            this.split.insert(session_split.clone(), !split_on);
                            this.remeasure_lists();
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new("diff-wrap")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Menu)
                    .selected(wrap_on)
                    .tooltip(tcode_i18n::tr!("diff.toggle_wrap"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.wrap = !this.wrap;
                        this.remeasure_lists();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("diff-whitespace")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Eye)
                    .tooltip(tcode_i18n::tr!("diff.whitespace_soon")),
            )
            .child(
                Button::new("diff-invisibles")
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::CaseSensitive)
                    .tooltip(tcode_i18n::tr!("diff.invisibles_soon")),
            );

        if selected_scope == Some(DiffScope::Branch) {
            let branches = self
                .git_preview
                .as_ref()
                .map(|preview| preview.result.branches.clone())
                .unwrap_or_default();
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
            let trigger = Button::new("diff-base-select")
                .ghost()
                .compact()
                .label(current.clone())
                .icon(IconName::ChevronDown);
            toolbar = toolbar.child(
                Popover::new("diff-base-popover")
                    .trigger(trigger)
                    .content(move |_, _, cx| {
                        let mut list = v_flex().w_full().p_1().gap_0p5();
                        for (branch_index, branch) in branches.clone().into_iter().enumerate() {
                            let panel = panel.clone();
                            let session = session_base.clone();
                            let chosen = branch.clone();
                            let selected = branch == current;
                            let accessible_label =
                                tcode_i18n::tr!("diff.base_branch", branch = branch.clone())
                                    .into_owned();
                            list = list.child(
                                material::accessible_clickable(
                                    h_flex(),
                                    ("diff-base-item", branch_index),
                                    Role::MenuItem,
                                    accessible_label,
                                    cx,
                                )
                                .aria_selected(selected)
                                .flex_none()
                                .w_full()
                                .px_2()
                                .py_1()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .hover(|row| row.bg(cx.theme().list_hover))
                                .when(selected, |row| row.bg(cx.theme().list_active))
                                .child(div().flex_1().child(branch))
                                .when(selected, |row| {
                                    row.child(Icon::new(IconName::Check).xsmall())
                                })
                                .on_click({
                                    let popover = cx.entity();
                                    move |_, window, cx| {
                                        panel.update(cx, |this, cx| {
                                            this.bases.insert(session.clone(), chosen.clone());
                                            this.cache = None;
                                            this.git_preview = None;
                                            cx.notify();
                                        });
                                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                                    }
                                }),
                            );
                        }
                        div()
                            .id("diff-base-list")
                            .role(Role::Menu)
                            .aria_label(tcode_i18n::tr!("diff.base_branches"))
                            .min_w(px(180.))
                            .max_h(px(280.))
                            .overflow_y_scroll()
                            .child(list)
                    })
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_xl()
                    .rounded(material::radius_overlay()),
            );
        }
        toolbar.into_any_element()
    }

    // -- body ---------------------------------------------------------------

    fn select_line(&mut self, file: String, row: usize, line: u32, side: ReviewSide, drag: bool) {
        if drag
            && let Some(selection) = self.selection.as_mut()
            && selection.file == file
            && selection.side == side
        {
            selection.row_end = row;
            selection.line_end = line;
            selection.end_index = row;
        } else {
            self.selection = Some(CommentSelection {
                file,
                row_start: row,
                row_end: row,
                line_start: line,
                line_end: line,
                side,
                start_index: row,
                end_index: row,
            });
            self.comment_input = None;
        }
        self.remeasure_lists();
    }

    fn review_excerpt(&self, selection: &CommentSelection) -> String {
        let Some(file) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.files.iter().find(|file| file.path == selection.file))
        else {
            return String::new();
        };
        let start = selection.row_start.min(selection.row_end);
        let end = selection.row_start.max(selection.row_end);
        let selected = file
            .all_rows
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= start && *index <= end)
            .filter_map(|(_, row)| match row {
                RenderedRow::Code {
                    kind,
                    old,
                    new,
                    text,
                    ..
                } => Some((*kind, *old, *new, text)),
                RenderedRow::Gap(_) => None,
            })
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
        let Some(selection) = self.selection.clone() else {
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
        self.app_state
            .update(cx, |state, cx| state.add_review_comment(comment, cx));
        self.selection = None;
        self.comment_input = None;
        self.remeasure_lists();
        cx.notify();
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            if self.loading_key.is_some() || self.render_loading_key.is_some() {
                return self.render_status(tcode_i18n::tr!("diff.loading").into_owned(), cx);
            }
            return self.render_empty(cx);
        };
        let split = self.split.get(&cache.session).copied().unwrap_or(false);
        let list_state = if split {
            cache.split_list.clone()
        } else {
            cache.unified_list.clone()
        };
        let content_width = if split {
            cache.split_content_width
        } else {
            cache.unified_content_width
        };
        let panel = cx.entity();
        let mut rows = list(list_state, move |index, _, cx| {
            panel.update(cx, |this, cx| this.render_list_item(index, split, cx))
        })
        .flex_1()
        .min_h_0()
        .h_full()
        .text_size(px(13.))
        .font_family(cx.theme().mono_font_family.clone());
        if self.wrap {
            rows = rows.w_full();
        } else {
            rows = rows.min_w(px(content_width));
        }

        let mut viewport = div()
            .id("diff-body")
            .flex_1()
            .min_h_0()
            .overflow_x_scroll()
            .child(rows);
        // Do not let this horizontal overflow container translate ordinary
        // vertical wheel input into horizontal movement. The event can then
        // bubble to the List's vertical scroll handler; explicit horizontal
        // wheel/trackpad deltas (or Shift-wheel) still scroll this viewport.
        viewport.style().restrict_scroll_to_axis = Some(true);
        let mut content = v_flex().size_full().min_h_0().child(viewport);

        if let Some(preview) = self
            .git_preview
            .as_ref()
            .filter(|preview| preview.session == cache.session && preview.scope == cache.scope)
        {
            if preview.result.truncated {
                content = content
                    .child(self.render_notice(tcode_i18n::tr!("diff.truncated").into_owned(), cx));
            }
            if let Some(error) = &preview.result.error {
                content = content.child(self.render_notice(error.clone(), cx));
            }
        }

        content.into_any_element()
    }

    fn render_list_item(&self, index: usize, split: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(cache) = self.cache.as_ref() else {
            return div().into_any_element();
        };
        let item = if split {
            cache.split_items.get(index)
        } else {
            cache.unified_items.get(index)
        };
        let Some(item) = item.copied() else {
            return div().into_any_element();
        };
        match item {
            DiffListItem::Header(file_index) => {
                self.render_file_header(&cache.files[file_index], cx)
            }
            DiffListItem::UnifiedRow {
                file: file_index,
                row,
            } => {
                let file = &cache.files[file_index];
                let (rendered, comment_row) = match &cache.unified_visible[file_index][row] {
                    VisibleItem::Gap { count, .. } => (self.render_gap(*count, cx), None),
                    VisibleItem::Row(row_index) => match &file.all_rows[*row_index] {
                        RenderedRow::Gap(gap) => (self.render_gap(gap.count, cx), None),
                        RenderedRow::Code {
                            kind,
                            old,
                            new,
                            text,
                            runs,
                        } => (
                            self.render_code_row(
                                &file.path, *row_index, *kind, *old, *new, text, runs, self.wrap,
                                cx,
                            ),
                            Some(*row_index),
                        ),
                    },
                };
                v_flex()
                    .min_w_full()
                    .child(rendered)
                    .children(
                        comment_row
                            .into_iter()
                            .flat_map(|row| self.render_comment_ui(&file.path, row, cx)),
                    )
                    .into_any_element()
            }
            DiffListItem::SplitRow { file, row } => {
                let file_index = file;
                let file = &cache.files[file_index];
                let (rendered, comment_rows) = match &cache.split_visible[file_index][row] {
                    VisibleSplitItem::Gap { count, .. } => {
                        (self.render_gap(*count, cx), Vec::new())
                    }
                    VisibleSplitItem::Pair(pair_index) => {
                        let pair = file.all_split[*pair_index];
                        let rendered = self.render_split_row(file, pair, self.wrap, cx);
                        let mut indices =
                            pair.left.into_iter().chain(pair.right).collect::<Vec<_>>();
                        indices.sort_unstable();
                        indices.dedup();
                        let comments = indices
                            .into_iter()
                            .flat_map(|index| self.render_comment_ui(&file.path, index, cx))
                            .collect();
                        (rendered, comments)
                    }
                };
                v_flex()
                    .min_w_full()
                    .child(rendered)
                    .children(comment_rows)
                    .into_any_element()
            }
        }
    }

    fn render_file_header(&self, file: &RenderedFile, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let rail = match file.kind {
            FileChangeKind::Create => Some(cx.theme().success),
            FileChangeKind::Delete => Some(cx.theme().danger),
            FileChangeKind::Rename => Some(cx.theme().info),
            FileChangeKind::Modify => None,
        };
        let kind_label = match file.kind {
            FileChangeKind::Create => Some((
                tcode_i18n::tr!("diff.created"),
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            )),
            FileChangeKind::Delete => Some((
                tcode_i18n::tr!("diff.deleted"),
                cx.theme().danger.opacity(0.12),
                cx.theme().danger_foreground,
            )),
            FileChangeKind::Rename => Some((
                tcode_i18n::tr!("diff.renamed"),
                cx.theme().info.opacity(0.12),
                cx.theme().info_foreground,
            )),
            FileChangeKind::Modify => None,
        };
        h_flex()
            .min_w_full()
            .h(px(34.))
            .px_3()
            .gap_2()
            .items_center()
            .bg(cx.theme().secondary)
            .rounded(material::radius_card())
            .relative()
            .when_some(rail, |this, color| {
                this.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(6.))
                        .bottom(px(6.))
                        .w(px(2.))
                        .rounded_full()
                        .bg(color),
                )
            })
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(IconName::File).xsmall().text_color(muted))
            .child(
                div()
                    .text_size(px(13.))
                    .font_medium()
                    .child(file.path.clone()),
            )
            .when_some(kind_label, |this, (label, background, foreground)| {
                this.child(
                    div()
                        .px_1p5()
                        .rounded_full()
                        .bg(background)
                        .text_size(px(11.))
                        .text_color(foreground)
                        .child(label),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .text_size(px(13.))
                    .child(
                        div()
                            .text_color(cx.theme().success)
                            .child(format!("+{}", file.added)),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().danger)
                            .child(format!("-{}", file.removed)),
                    ),
            )
            .into_any_element()
    }

    fn render_gap(&self, count: u32, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .min_w_full()
            .h(px(24.))
            .px_3()
            .items_center()
            .bg(cx.theme().muted)
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .font_family(cx.theme().font_family.clone())
            .child(tcode_i18n::tr!("diff.unmodified_lines", count = count))
            .into_any_element()
    }

    fn render_notice(&self, message: String, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .min_w_full()
            .px_3()
            .py_2()
            .bg(cx.theme().warning.opacity(0.12))
            .text_size(px(11.))
            .text_color(cx.theme().warning_foreground)
            .font_family(cx.theme().font_family.clone())
            .child(Icon::new(IconName::TriangleAlert).xsmall())
            .child(message)
            .into_any_element()
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
        row_index: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = self
            .app_state
            .read(cx)
            .review_comments()
            .iter()
            .filter(|comment| comment.file == file && comment.end_index() == row_index)
            .map(|comment| {
                h_flex()
                    .min_w_full()
                    .px_3()
                    .py_1p5()
                    .gap_2()
                    .relative()
                    .rounded(material::radius_card())
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
            .selection
            .as_ref()
            .filter(|selection| selection.file == file && selection.row_end == row_index);
        if selection.is_some() {
            if let Some(input) = &self.comment_input {
                rows.push(
                    v_flex()
                        .min_w_full()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .bg(cx.theme().muted)
                        .rounded(material::radius_card())
                        .font_family(cx.theme().font_family.clone())
                        .child(Input::new(input).appearance(false))
                        .child(
                            h_flex().justify_end().child(
                                Button::new("diff-submit-comment")
                                    .primary()
                                    .small()
                                    .label(tcode_i18n::tr!("diff.submit_comment"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_comment(cx);
                                    })),
                            ),
                        )
                        .into_any_element(),
                );
            } else {
                rows.push(
                    h_flex()
                        .min_w_full()
                        .px_3()
                        .py_1()
                        .bg(cx.theme().muted)
                        .rounded(material::radius_card())
                        .font_family(cx.theme().font_family.clone())
                        .child(
                            Button::new("diff-add-comment")
                                .ghost()
                                .small()
                                .label(tcode_i18n::tr!("diff.add_comment"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.comment_input = Some(cx.new(|cx| {
                                        InputState::new(window, cx).placeholder(tcode_i18n::tr!(
                                            "diff.comment_placeholder"
                                        ))
                                    }));
                                    this.remeasure_lists();
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        rows
    }

    fn render_split_row(
        &self,
        file: &RenderedFile,
        pair: PairedRow,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let paired_as_context = pair.left.zip(pair.right).is_some_and(|(left, right)| {
            matches!(
                (&file.all_rows[left], &file.all_rows[right]),
                (
                    RenderedRow::Code { text: left, .. },
                    RenderedRow::Code { text: right, .. }
                ) if left == right
            )
        });
        let cell = |row_index: Option<usize>, side: ReviewSide, cx: &mut Context<Self>| {
            let index = row_index.unwrap_or_default();
            let Some(RenderedRow::Code {
                kind,
                old,
                new,
                text,
                runs,
            }) = row_index.map(|index| &file.all_rows[index])
            else {
                return div().flex_1().min_w_0().min_h(px(18.)).into_any_element();
            };
            let line = match side {
                ReviewSide::Old => *old,
                ReviewSide::New => *new,
            };
            self.render_split_cell(
                &file.path,
                index,
                if paired_as_context {
                    RowKind::Context
                } else {
                    *kind
                },
                line,
                side,
                text,
                runs,
                wrap,
                cx,
            )
        };
        h_flex()
            .min_w_full()
            .items_stretch()
            .child(cell(pair.left, ReviewSide::Old, cx))
            .child(div().w_px().bg(cx.theme().border.opacity(0.)))
            .child(cell(pair.right, ReviewSide::New, cx))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_split_cell(
        &self,
        file: &str,
        row_index: usize,
        kind: RowKind,
        line: Option<u32>,
        side: ReviewSide,
        text: &str,
        runs: &[(Range<usize>, HighlightStyle)],
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bg = match kind {
            RowKind::Added => Some(cx.theme().success.opacity(0.13)),
            RowKind::Removed => Some(cx.theme().danger.opacity(0.12)),
            RowKind::Context => None,
        };
        let file_down = file.to_string();
        let file_move = file.to_string();
        let gutter = div()
            .flex_none()
            .w(px(42.))
            .px_1()
            .text_right()
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground)
            .cursor_pointer()
            .child(line.map(|value| value.to_string()).unwrap_or_default())
            .when_some(line, |gutter, line| {
                gutter
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.select_line(file_down.clone(), row_index, line, side, false);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.dragging() {
                            this.select_line(file_move.clone(), row_index, line, side, true);
                            cx.notify();
                        }
                    }))
            });
        let mut code = div()
            .flex_1()
            .min_w_0()
            .px_2()
            .text_color(cx.theme().foreground)
            .child(StyledText::new(text.to_string()).with_highlights(runs.iter().cloned()));
        if !wrap {
            code = code.whitespace_nowrap();
        }
        h_flex()
            .flex_1()
            .min_w_0()
            .min_h(px(18.))
            .items_start()
            .when_some(bg, |cell, color| cell.bg(color))
            .child(gutter)
            .child(code)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_code_row(
        &self,
        file: &str,
        row_index: usize,
        kind: RowKind,
        old: Option<u32>,
        new: Option<u32>,
        text: &str,
        runs: &[(Range<usize>, HighlightStyle)],
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (bg, accent) = match kind {
            RowKind::Added => (
                Some(cx.theme().success.opacity(0.13)),
                Some(cx.theme().success),
            ),
            RowKind::Removed => (
                Some(cx.theme().danger.opacity(0.12)),
                Some(cx.theme().danger),
            ),
            RowKind::Context => (None, None),
        };
        let muted = cx.theme().muted_foreground;

        let gutter = |n: Option<u32>, side: ReviewSide, cx: &mut Context<Self>| {
            let file_down = file.to_string();
            let file_move = file.to_string();
            div()
                .flex_none()
                .w(px(44.))
                .px_1()
                .text_right()
                .text_size(px(11.))
                .text_color(muted)
                .child(n.map(|v| v.to_string()).unwrap_or_default())
                .cursor_pointer()
                .when_some(n, |gutter, line| {
                    gutter
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.select_line(file_down.clone(), row_index, line, side, false);
                                cx.notify();
                            }),
                        )
                        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                            if event.dragging() {
                                this.select_line(file_move.clone(), row_index, line, side, true);
                                cx.notify();
                            }
                        }))
                })
        };

        let mut code = div()
            .flex_1()
            .px_2()
            .text_color(cx.theme().foreground)
            .child(StyledText::new(text.to_string()).with_highlights(runs.iter().cloned()));
        if wrap {
            code = code.min_w_0();
        } else {
            code = code.whitespace_nowrap();
        }

        h_flex()
            .min_w_full()
            .min_h(px(18.))
            .items_start()
            .border_l_2()
            .border_color(accent.unwrap_or(gpui::transparent_black()))
            .when_some(bg, |this, c| this.bg(c))
            .child(gutter(old, ReviewSide::Old, cx))
            .child(gutter(new, ReviewSide::New, cx))
            .child(code)
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("diff.empty")),
            )
            .into_any_element()
    }
}

impl Render for DiffPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_cache(cx);
        let tab = self.app_state.read(cx).right_tab();
        let mut root = v_flex()
            .size_full()
            .min_w_0()
            .text_color(cx.theme().foreground)
            .child(self.render_tab_strip(window, cx));
        root = match tab {
            // Preview is rendered by its own panel (see ui/mod.rs); the diff
            // container only handles Diff/Plan, so treat Preview as the diff view
            // for the unreachable fallback.
            RightTab::Diff | RightTab::Preview => root
                .child(self.render_toolbar(cx))
                .child(self.render_body(cx)),
            RightTab::Plan => root.child(div().flex_1().min_h_0().child(self.plan.clone())),
        };
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn resolves_file_headers_independently_for_unified_and_split_lists() {
        let code_row = |text: &str| RenderedRow::Code {
            kind: RowKind::Added,
            old: None,
            new: Some(1),
            text: text.into(),
            runs: Vec::new(),
        };
        let first_rows = vec![code_row("one"), code_row("two"), code_row("three")];
        let second_rows = vec![code_row("replacement")];
        let files = vec![
            RenderedFile {
                path: "src/first.rs".into(),
                kind: FileChangeKind::Modify,
                added: 3,
                removed: 0,
                all_split: vec![PairedRow {
                    left: None,
                    right: Some(0),
                }],
                all_rows: first_rows,
                collapsed: Vec::new(),
                expandable: false,
            },
            RenderedFile {
                path: "tests/second.rs".into(),
                kind: FileChangeKind::Modify,
                added: 1,
                removed: 0,
                all_split: vec![PairedRow {
                    left: None,
                    right: Some(0),
                }],
                all_rows: second_rows,
                collapsed: Vec::new(),
                expandable: false,
            },
        ];
        let items = build_list_items(&files);
        let (unified, split) = (items.unified, items.split);

        assert_eq!(
            file_header_index(&files, &unified, "tests/second.rs"),
            Some(4)
        );
        assert_eq!(
            file_header_index(&files, &split, "tests/second.rs"),
            Some(2)
        );
        assert_eq!(file_header_index(&files, &unified, "missing.rs"), None);
    }

    #[test]
    fn large_diff_builds_virtual_list_models_without_row_elements() {
        let rows = (1..=5_000)
            .map(|line| RenderedRow::Code {
                kind: RowKind::Added,
                old: None,
                new: Some(line),
                text: format!("let value_{line} = {line};"),
                runs: Vec::new(),
            })
            .collect::<Vec<_>>();
        let all_split = (0..rows.len())
            .map(|row| PairedRow {
                left: None,
                right: Some(row),
            })
            .collect();
        let files = vec![RenderedFile {
            path: "src/large.rs".into(),
            kind: FileChangeKind::Modify,
            added: 5_000,
            removed: 0,
            all_rows: rows,
            all_split,
            collapsed: Vec::new(),
            expandable: false,
        }];

        let items = build_list_items(&files);
        let (unified, split) = (items.unified, items.split);

        assert_eq!(unified.len(), 5_001);
        assert_eq!(split.len(), 5_001);
        assert!(matches!(unified[0], DiffListItem::Header(0)));
        assert!(matches!(
            unified[5_000],
            DiffListItem::UnifiedRow {
                file: 0,
                row: 4_999
            }
        ));
    }

    #[gpui::test]
    fn virtual_list_constructs_only_the_large_diff_viewport(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let cx = cx.add_empty_window();
        let constructions = Arc::new(AtomicUsize::new(0));
        let state = ListState::new(5_001, ListAlignment::Top, px(180.));

        struct TestList {
            state: ListState,
            constructions: Arc<AtomicUsize>,
        }
        impl Render for TestList {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let observed = self.constructions.clone();
                list(self.state.clone(), move |_, _, _| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    div().h(px(18.)).w_full().into_any_element()
                })
                .size_full()
            }
        }
        let view_constructions = constructions.clone();

        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(900.), px(720.)),
            move |_, cx| {
                cx.new(|_| TestList {
                    state,
                    constructions: view_constructions,
                })
                .into_any_element()
            },
        );

        let count = constructions.load(Ordering::Relaxed);
        assert!(
            count < 100,
            "expected viewport-only construction for 5,001 items, got {count}"
        );
    }
}
