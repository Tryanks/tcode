//! The command palette (⌘K), V2-M6. A centered modal over the workspace with a
//! search input and grouped results (Actions + Threads), matching reference
//! shot 27-cmdk.png.
//!
//! Rendered by [`crate::AppShell`] as a full-window overlay only while
//! [`tcode_runtime::app::AppState::palette_open`] is set. Sources:
//! - Threads: fuzzy match over session titles (enter opens the thread).
//! - Actions: "New thread…" per project, "Open settings", "Toggle theme",
//!   "Toggle diff panel".
//!
//! Fuzzy matching is a hand-rolled subsequence scorer ([`fuzzy_score`], no deps).

use agent::ProviderKind;
use gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use tcode_runtime::app::AppState;

use crate::provider_card::provider_glyph;
use crate::settings::ThemeMode;
use crate::settings_page::apply_theme;
use crate::time::now_secs;

/// Score `text` against a fuzzy `query` (case-insensitive subsequence match).
/// Returns `None` when `query` is not a subsequence of `text`; a higher score
/// is a better match (consecutive and earlier hits score more). An empty query
/// matches everything with score 0.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut last: Option<usize> = None;
    for (ti, tc) in t.iter().enumerate() {
        if qi < q.len() && *tc == q[qi] {
            match last {
                Some(prev) if ti == prev + 1 => score += 10, // consecutive
                Some(_) => score += 1,
                None => score += 5 - (ti.min(5) as i32), // earlier first hit
            }
            last = Some(ti);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

/// A concrete action a palette row triggers.
#[derive(Clone)]
enum Action {
    NewThread {
        cwd: std::path::PathBuf,
        project_id: String,
    },
    OpenSettings,
    ToggleTheme,
    ToggleDiff,
    ToggleTerminal,
    OpenPreview,
    CheckUpdates,
    OpenThread {
        session_id: String,
    },
}

/// Compact relative-time label (e.g. "5m ago") from an elapsed-seconds count.
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

/// One rendered palette row.
#[derive(Clone)]
struct Item {
    icon: IconName,
    label: String,
    /// Optional muted subtitle (e.g. a thread's project).
    subtitle: Option<String>,
    /// Thread rows carry their provider glyph + last-activity time (right side).
    provider: Option<ProviderKind>,
    updated_at: Option<u64>,
    action: Action,
}

struct Group {
    label: String,
    items: Vec<Item>,
}

pub struct CommandPalette {
    app_state: Entity<AppState>,
    query: Entity<InputState>,
    focus_handle: FocusHandle,
    selected: usize,
    _subscriptions: Vec<Subscription>,
}

impl CommandPalette {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tcode_i18n::tr!("palette.placeholder"))
        });

        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &query,
                window,
                |this, _query, event, window, cx| match event {
                    InputEvent::Change => {
                        this.selected = 0;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.activate_selected(window, cx);
                    }
                    _ => {}
                },
            ),
        ];

        Self {
            app_state,
            query,
            focus_handle: cx.focus_handle(),
            selected: 0,
            _subscriptions: subscriptions,
        }
    }

    /// Focus the search input (called when the palette opens). `--debug-palette`
    /// seeds the query so palette states can be screenshotted headlessly.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seed = self
            .app_state
            .read(cx)
            .debug_palette
            .clone()
            .unwrap_or_default();
        self.query.update(cx, |state, cx| {
            state.set_value(seed, window, cx);
            state.focus(window, cx);
        });
        self.selected = 0;
    }

    fn close(&self, cx: &mut Context<Self>) {
        self.app_state
            .update(cx, |state, cx| state.close_palette(cx));
    }

    /// Build the grouped result list for the current query. A leading `>`
    /// restricts results to Actions (T3 §4).
    fn groups(&self, cx: &Context<Self>) -> Vec<Group> {
        let raw = self.query.read(cx).value().to_string();
        let actions_only = raw.trim_start().starts_with('>');
        // Strip the `>` prefix so the remainder still fuzzy-matches action labels.
        let query = if actions_only {
            raw.trim_start()
                .trim_start_matches('>')
                .trim_start()
                .to_string()
        } else {
            raw
        };
        let state = self.app_state.read(cx);

        // Actions.
        let mut actions: Vec<(i32, Item)> = Vec::new();
        let mut push_action = |label: String, icon: IconName, action: Action| {
            if let Some(score) = fuzzy_score(&query, &label) {
                actions.push((
                    score,
                    Item {
                        icon,
                        label,
                        subtitle: None,
                        provider: None,
                        updated_at: None,
                        action,
                    },
                ));
            }
        };
        for group in state.grouped_sessions() {
            push_action(
                tcode_i18n::tr!("palette.new_thread", project = group.project.name).into_owned(),
                IconName::Plus,
                Action::NewThread {
                    cwd: group.project.root.clone(),
                    project_id: group.project.id.clone(),
                },
            );
        }
        push_action(
            tcode_i18n::tr!("palette.open_settings").into_owned(),
            IconName::Settings,
            Action::OpenSettings,
        );
        push_action(
            tcode_i18n::tr!("palette.toggle_theme").into_owned(),
            IconName::Moon,
            Action::ToggleTheme,
        );
        push_action(
            tcode_i18n::tr!("palette.toggle_diff").into_owned(),
            IconName::PanelRight,
            Action::ToggleDiff,
        );
        push_action(
            tcode_i18n::tr!("palette.toggle_terminal").into_owned(),
            IconName::SquareTerminal,
            Action::ToggleTerminal,
        );
        push_action(
            tcode_i18n::tr!("palette.open_preview").into_owned(),
            IconName::Globe,
            Action::OpenPreview,
        );
        push_action(
            tcode_i18n::tr!("palette.check_updates").into_owned(),
            IconName::Inbox,
            Action::CheckUpdates,
        );
        actions.sort_by_key(|b| std::cmp::Reverse(b.0));

        let mut groups = Vec::new();
        if !actions.is_empty() {
            groups.push(Group {
                label: tcode_i18n::tr!("palette.actions").into_owned(),
                items: actions.into_iter().map(|(_, i)| i).collect(),
            });
        }

        // Threads (fuzzy over titles) — suppressed in `>`-actions-only mode.
        if !actions_only {
            let mut threads: Vec<(i32, Item)> = Vec::new();
            for group in state.grouped_sessions() {
                for meta in &group.sessions {
                    if let Some(score) = fuzzy_score(&query, &meta.title) {
                        threads.push((
                            score,
                            Item {
                                icon: IconName::SquareTerminal,
                                label: meta.title.clone(),
                                subtitle: Some(group.project.name.clone()),
                                provider: Some(meta.provider),
                                updated_at: Some(meta.updated_at),
                                action: Action::OpenThread {
                                    session_id: meta.id.clone(),
                                },
                            },
                        ));
                    }
                }
            }
            threads.sort_by_key(|b| std::cmp::Reverse(b.0));
            if !threads.is_empty() {
                groups.push(Group {
                    label: tcode_i18n::tr!("palette.threads").into_owned(),
                    items: threads.into_iter().map(|(_, i)| i).collect(),
                });
            }
        }
        groups
    }

    /// Flattened item list (row order), for keyboard selection.
    fn flat_items(&self, cx: &Context<Self>) -> Vec<Item> {
        self.groups(cx).into_iter().flat_map(|g| g.items).collect()
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.flat_items(cx);
        if let Some(item) = items.get(self.selected).cloned() {
            self.activate(item.action, window, cx);
        }
    }

    fn activate(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::NewThread { cwd, project_id } => {
                self.close(cx);
                self.app_state.update(cx, |state, cx| {
                    state.start_draft(project_id, cwd, cx);
                });
            }
            Action::OpenSettings => {
                // open_settings also clears palette_open.
                self.app_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }
            Action::ToggleTheme => {
                let next = if cx.theme().mode.is_dark() {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                self.app_state.update(cx, |state, cx| {
                    let mut settings = state.settings.clone();
                    settings.theme_mode = next;
                    state.update_settings(settings, cx);
                });
                apply_theme(next, window, cx);
                self.close(cx);
            }
            Action::ToggleDiff => {
                self.app_state
                    .update(cx, |state, cx| state.toggle_diff_panel(cx));
                self.close(cx);
            }
            Action::ToggleTerminal => {
                self.app_state
                    .update(cx, |state, cx| state.toggle_terminal_panel(cx));
                self.close(cx);
            }
            Action::OpenPreview => {
                self.app_state
                    .update(cx, |state, cx| state.open_preview_panel(cx));
                self.close(cx);
            }
            Action::CheckUpdates => {
                self.app_state
                    .update(cx, |state, cx| state.check_provider_versions(cx));
                self.close(cx);
            }
            Action::OpenThread { session_id } => {
                self.app_state
                    .update(cx, |state, cx| state.select_session(&session_id, cx));
                self.close(cx);
            }
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let total = self.flat_items(cx).len();
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.close(cx);
                cx.stop_propagation();
            }
            "down" => {
                if total > 0 {
                    self.selected = (self.selected + 1).min(total - 1);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            "up" => {
                self.selected = self.selected.saturating_sub(1);
                cx.notify();
                cx.stop_propagation();
            }
            _ => {}
        }
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let groups = self.groups(cx);
        let total: usize = groups.iter().map(|g| g.items.len()).sum();
        if total > 0 && self.selected >= total {
            self.selected = total - 1;
        }
        let muted = cx.theme().muted_foreground;

        // Build the result list, tracking a running flat index for highlight.
        let mut list_content = v_flex().w_full().px_2().py_2().gap_1();
        let mut flat = 0usize;
        for group in &groups {
            list_content = list_content.child(
                div()
                    .flex_none()
                    .px_2()
                    .pt_1()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(group.label.clone()),
            );
            for item in &group.items {
                let index = flat;
                flat += 1;
                let is_sel = index == self.selected;
                let action = item.action.clone();
                list_content = list_content.child(
                    h_flex()
                        .id(("palette-row", index))
                        .role(Role::ListBoxOption)
                        .aria_label(item.label.clone())
                        .aria_selected(is_sel)
                        .when(is_sel, |row| row.aria_active_descendant())
                        .flex_none()
                        .w_full()
                        .h(px(38.))
                        .px_2()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(is_sel, |s| s.bg(cx.theme().list_active))
                        .when(!is_sel, |s| {
                            s.hover(|style| style.bg(cx.theme().list_hover))
                        })
                        .child(Icon::new(item.icon.clone()).small().text_color(muted))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(15.))
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(item.label.clone()),
                                )
                                .when_some(item.subtitle.clone(), |this, sub| {
                                    this.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(muted)
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .child(sub),
                                    )
                                }),
                        )
                        // Thread rows: provider glyph + relative last-activity time.
                        .when_some(item.provider, |this, provider| {
                            this.child(
                                h_flex()
                                    .flex_none()
                                    .gap_1p5()
                                    .items_center()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .when_some(item.updated_at, |this, at| {
                                        let ago = now_secs().saturating_sub(at);
                                        this.child(div().child(humanize_ago(ago)))
                                    })
                                    .child(provider_glyph(provider).xsmall()),
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.activate(action.clone(), window, cx);
                        })),
                );
            }
        }
        if total == 0 {
            list_content = list_content.child(
                div()
                    .flex_none()
                    .px_2()
                    .py_4()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(tcode_i18n::tr!("palette.no_matches")),
            );
        }
        let list = div()
            .id("palette-list")
            .role(Role::ListBox)
            .aria_label(tcode_i18n::tr!("palette.results"))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list_content);

        // Centered modal card, anchored ~15% from the top over a dim backdrop.
        let card = crate::material::overlay_contour(
            v_flex()
                .w(px(640.))
                .max_h(px(440.))
                .rounded(crate::material::radius_overlay())
                .overflow_hidden(),
            cx,
        )
        .child(
            h_flex()
                .flex_none()
                .h(px(48.))
                .px_3()
                .gap_2()
                .items_center()
                .child(Icon::new(IconName::Search).small().text_color(muted))
                .child(
                    div().flex_1().child(
                        Input::new(&self.query)
                            .appearance(false)
                            .rounded(crate::material::radius_input()),
                    ),
                ),
        )
        .child(list)
        .child(
            h_flex()
                .flex_none()
                .h(px(34.))
                .px_3()
                .gap_3()
                .items_center()
                .text_size(px(11.))
                .text_color(muted)
                .child(tcode_i18n::tr!("palette.navigate"))
                .child(tcode_i18n::tr!("palette.select"))
                .child(tcode_i18n::tr!("palette.close")),
        );

        div()
            .id("palette-overlay")
            .track_focus(&self.focus_handle)
            .absolute()
            .inset_0()
            .size_full()
            .bg(gpui::black().opacity(0.35))
            .flex()
            .justify_center()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.close(cx);
                }),
            )
            .child(
                div()
                    .mt(px(96.))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use tcode_services::store::SessionStore;

    struct PaletteHarness {
        palette: Entity<CommandPalette>,
    }

    impl PaletteHarness {
        fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
            Self {
                palette: cx.new(|cx| CommandPalette::new(app_state, window, cx)),
            }
        }
    }

    impl Render for PaletteHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn dispatch_palette_key(
        palette: &Entity<CommandPalette>,
        cx: &mut VisualTestContext,
        key: &str,
    ) {
        let event = KeyDownEvent {
            keystroke: gpui::Keystroke::parse(key).expect("valid palette key"),
            is_held: false,
            prefer_character_input: false,
        };
        cx.update(|window, cx| {
            palette.update(cx, |palette, cx| {
                palette.on_key_down(&event, window, cx);
            });
        });
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("xyz", "New thread"), None);
        assert_eq!(fuzzy_score("ttt", "cat"), None);
    }

    #[test]
    fn subsequence_matches_case_insensitively() {
        assert!(fuzzy_score("nt", "New thread").is_some());
        assert!(fuzzy_score("NEW", "new thread").is_some());
    }

    #[test]
    fn consecutive_scores_higher_than_scattered() {
        // "set" contiguous in "settings" beats the scattered hit in "s...e...t".
        let contiguous = fuzzy_score("set", "settings").unwrap();
        let scattered = fuzzy_score("set", "some effort table").unwrap();
        assert!(contiguous > scattered, "{contiguous} !> {scattered}");
    }

    #[test]
    fn better_match_ranks_first_when_sorted() {
        let mut scored: Vec<(i32, &str)> = ["Open settings", "Toggle theme", "Toggle diff panel"]
            .into_iter()
            .filter_map(|s| fuzzy_score("tog", s).map(|score| (score, s)))
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        // Both "Toggle ..." match; "Open settings" does not.
        assert_eq!(scored.len(), 2);
        assert!(scored[0].1.starts_with("Toggle"));
    }

    #[gpui::test]
    fn arrow_keys_move_and_clamp_the_highlight_while_the_query_keeps_focus(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-palette-keyboard-test-{}",
            tcode_services::store::now_millis()
        ));
        let store = SessionStore::open_at(root.clone()).expect("open test store");
        let app_state = cx.new(|_| AppState::new(store));
        let palette_state = app_state.clone();
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            PaletteHarness::new(palette_state.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let palette = cx.update(|_, cx| harness.read(cx).palette.clone());
        cx.update(|window, cx| {
            let query = palette.read(cx).query.clone();
            query.read(cx).focus_handle(cx).focus(window, cx);
        });

        dispatch_palette_key(&palette, cx, "down");
        dispatch_palette_key(&palette, cx, "down");
        cx.update(|window, cx| {
            let palette = palette.read(cx);
            assert_eq!(palette.selected, 2);
            assert!(palette.query.read(cx).focus_handle(cx).is_focused(window));
        });

        dispatch_palette_key(&palette, cx, "up");
        dispatch_palette_key(&palette, cx, "up");
        dispatch_palette_key(&palette, cx, "up");
        cx.update(|_, cx| assert_eq!(palette.read(cx).selected, 0));

        for _ in 0..10 {
            dispatch_palette_key(&palette, cx, "down");
        }
        cx.update(|_, cx| {
            let total = palette.update(cx, |palette, cx| palette.flat_items(cx).len());
            assert_eq!(palette.read(cx).selected, total - 1);
        });

        drop(palette);
        drop(app_state);
        let _ = std::fs::remove_dir_all(root);
    }
}
