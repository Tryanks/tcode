//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel (see docs/DESIGN.md "Composer").

use std::cell::Cell;
use std::rc::Rc;

use agent::{
    ApprovalDecision, ApprovalKind, ApprovalRequest, FileChangeKind, ProviderKind, TokenUsage,
};
use gpui::{
    Anchor, AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::{Popover, PopoverState},
    spinner::Spinner,
    v_flex,
};

use crate::app::AppState;

/// Claude's warm brand tint for the starburst glyph.
const CLAUDE_TINT: u32 = 0xD97757;
/// T3's circular stop button red-orange.
const STOP_TINT: u32 = 0xF4562E;
/// Below this measured control-row width the row collapses its context /
/// permission / mode chips into a "⋯" overflow popover so nothing spills past
/// the card edge (diff panel open, or a small window).
const CONTROL_ROW_COMPACT_BELOW: f32 = 520.;

/// One selectable model in the picker.
#[derive(Clone)]
struct ModelRow {
    /// The id passed to the provider (`None` = provider default).
    id: Option<&'static str>,
    /// Display name (also the favorites key; unique across the catalog).
    name: &'static str,
    provider: ProviderKind,
}

fn model_catalog() -> Vec<ModelRow> {
    vec![
        ModelRow { id: None, name: "Default", provider: ProviderKind::ClaudeCode },
        ModelRow { id: Some("opus"), name: "Claude Opus", provider: ProviderKind::ClaudeCode },
        ModelRow { id: Some("sonnet"), name: "Claude Sonnet", provider: ProviderKind::ClaudeCode },
        ModelRow { id: Some("haiku"), name: "Claude Haiku", provider: ProviderKind::ClaudeCode },
        ModelRow { id: Some("gpt-5.6-sol"), name: "gpt-5.6-sol", provider: ProviderKind::Codex },
        ModelRow {
            id: Some("gpt-5.6-sol-mini"),
            name: "gpt-5.6-sol-mini",
            provider: ProviderKind::Codex,
        },
        ModelRow { id: Some("gpt-5.5-codex"), name: "gpt-5.5-codex", provider: ProviderKind::Codex },
    ]
}

fn provider_short(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "Claude",
        ProviderKind::Codex => "Codex",
    }
}

/// The provider glyph (Claude starburst / Codex OpenAI mark).
fn provider_glyph(provider: ProviderKind) -> Icon {
    match provider {
        ProviderKind::ClaudeCode => {
            Icon::empty().path("icons/claude.svg").text_color(rgb(CLAUDE_TINT))
        }
        ProviderKind::Codex => Icon::empty().path("icons/openai.svg"),
    }
}

/// Which rail filter the model picker is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerRail {
    Favorites,
    Provider(ProviderKind),
}

pub enum ComposerEvent {
    /// A turn was just submitted (chat view scrolls to the bottom).
    Submitted,
}

pub struct Composer {
    app_state: Entity<AppState>,
    input: Entity<InputState>,
    model_search: Entity<InputState>,
    /// `None` = follow the active session's provider (set on first open).
    picker_rail: Option<PickerRail>,
    /// Whether the approval panel's detail is expanded.
    approval_expanded: bool,
    /// Measured width of the control row (written from the prepaint callback,
    /// read at render time); drives the collapse to the "⋯" overflow layout at
    /// narrow widths. Shared via `Rc<Cell>` because the paint-phase callback
    /// cannot mutate the entity directly.
    control_width: Rc<Cell<Option<f32>>>,
    /// The width `render` last observed, to detect when a fresh measurement
    /// arrived and drive the reflow convergence (see `render`).
    prev_seen_width: Option<f32>,
    /// Whether the current render was scheduled by our own animation-frame
    /// request (vs. an external trigger). Used to stop the convergence loop.
    raf_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Ask anything, @tag files/folders, $use skills, or / for commands")
        });
        let model_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search models…"));

        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, input, event, window, cx| {
                match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        let input = input.clone();
                        this.submit(&input, window, cx);
                    }
                    // Re-render so the send button reflects whether there's text.
                    InputEvent::Change => cx.notify(),
                    _ => {}
                }
            }),
            // Live-filter the model picker as the user types in its search box.
            cx.subscribe(&model_search, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        Self {
            app_state,
            input,
            model_search,
            picker_rail: None,
            approval_expanded: false,
            control_width: Rc::new(Cell::new(None)),
            prev_seen_width: None,
            raf_pending: false,
            _subscriptions: subscriptions,
        }
    }

    fn submit(&mut self, input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.app_state.read(cx).active.is_none() {
            window.push_notification(Notification::info("Create or select a session first."), cx);
            return;
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.app_state.update(cx, |state, cx| state.send_turn(text, cx));
        cx.emit(ComposerEvent::Submitted);
        cx.notify();
    }

    fn rail(&self, provider: ProviderKind) -> PickerRail {
        self.picker_rail
            .unwrap_or(PickerRail::Provider(provider))
    }

    // -- control-row popovers ----------------------------------------------

    /// The model-picker button + popover (anchored above, ~360px).
    fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let app_state = self.app_state.read(cx);
        let (provider, current_model) = match &app_state.active {
            Some(active) => (active.meta.provider, active.meta.model.clone()),
            None => return div().into_any_element(),
        };
        let display = current_model_name(provider, current_model.as_deref());

        // Build the filtered + favorites-first row list for the current frame.
        let query = self.model_search.read(cx).value().to_lowercase();
        let rail = self.rail(provider);
        let mut rows: Vec<ModelRow> = model_catalog()
            .into_iter()
            .filter(|r| match rail {
                PickerRail::Favorites => app_state.is_favorite_model(r.name),
                PickerRail::Provider(p) => r.provider == p,
            })
            .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
            .collect();
        rows.sort_by_key(|r| !app_state.is_favorite_model(r.name));

        let composer = cx.entity();
        let app_entity = self.app_state.clone();
        let model_search = self.model_search.clone();
        let pending_restart = app_state.model_pending_restart();
        let selected = current_model.clone();

        let trigger = Button::new("model-picker")
            .ghost()
            .compact()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .child(provider_glyph(provider).small())
                    .child(div().font_medium().child(display))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    ),
            );

        Popover::new("model-picker-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let rows = rows.clone();
                let app_entity = app_entity.clone();
                let model_search = model_search.clone();
                let composer = composer.clone();
                let selected = selected.clone();
                let popover = cx.entity();
                render_model_pane(
                    &rows,
                    &selected,
                    rail,
                    pending_restart,
                    &app_entity,
                    &model_search,
                    &composer,
                    &popover,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The context-usage chip + popover.
    fn render_context_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let usage = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.timeline.usage);
        let label = context_label(usage);
        let muted = cx.theme().muted_foreground;

        let trigger = Button::new("context-chip").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        Popover::new("context-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| render_context_pane(usage, cx))
            .into_any_element()
    }

    /// A static (non-selectable) chip: icon + label + "coming soon" tooltip.
    fn render_static_chip(
        &self,
        id: &'static str,
        icon: Icon,
        label: &'static str,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        Button::new(id)
            .ghost()
            .compact()
            .tooltip(tooltip)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(icon.small().text_color(muted))
                    .child(label),
            )
            .into_any_element()
    }

    /// The "⋯" overflow button + popover holding the context / permission /
    /// mode controls when the control row is too narrow to show them inline.
    fn render_overflow_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let usage = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.timeline.usage);
        let muted = cx.theme().muted_foreground;

        let trigger = Button::new("overflow-controls")
            .ghost()
            .compact()
            .tooltip("More controls")
            .child(
                Icon::new(IconName::Ellipsis)
                    .small()
                    .text_color(muted),
            );

        Popover::new("overflow-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| render_overflow_pane(usage, cx))
            .into_any_element()
    }

    // -- send / stop --------------------------------------------------------

    fn render_send_or_stop(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if turn_running {
            return h_flex()
                .gap_2()
                .items_center()
                // Blue activity spinner.
                .child(Spinner::new().small().color(cx.theme().primary))
                // Circular red-orange stop button.
                .child(
                    div()
                        .id("stop-turn")
                        .size(px(36.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgb(STOP_TINT))
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .child(div().size(px(11.)).rounded(px(2.)).bg(gpui::white()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.app_state.update(cx, |state, cx| state.interrupt(cx));
                        })),
                )
                .into_any_element();
        }

        let has_text = !self.input.read(cx).value().trim().is_empty();
        let (bg, fg) = if has_text {
            (cx.theme().primary, cx.theme().primary_foreground)
        } else {
            (cx.theme().muted, cx.theme().muted_foreground)
        };
        div()
            .id("send-message")
            .size(px(36.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(bg)
            .cursor_pointer()
            .when(has_text, |s| s.hover(|s| s.opacity(0.9)))
            .child(Icon::new(IconName::ArrowUp).small().text_color(fg))
            .on_click(cx.listener(|this, _, window, cx| {
                let input = this.input.clone();
                this.submit(&input, window, cx);
            }))
            .into_any_element()
    }

    // -- below-card + approval ---------------------------------------------

    fn render_checkout_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (branch, branches, turn_running) = {
            let state = self.app_state.read(cx);
            let active = state.active.as_ref()?;
            let branch = active.git_branch.clone()?;
            (branch, active.branches.clone(), active.timeline.turn_running)
        };
        let muted = cx.theme().muted_foreground;

        // The branch chip: a popover listing local branches. While a turn runs
        // the selector is disabled (it just shows a "wait" tooltip).
        let right: AnyElement = if turn_running {
            Button::new("branch-picker")
                .ghost()
                .compact()
                .tooltip("Wait for the current turn")
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                        .child(branch),
                )
                .into_any_element()
        } else {
            let app_open = self.app_state.clone();
            let app_content = self.app_state.clone();
            let current = branch.clone();
            let trigger = Button::new("branch-picker").ghost().compact().child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                    .child(branch)
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .text_color(muted),
                    ),
            );
            Popover::new("branch-popover")
                .anchor(Anchor::BottomRight)
                .trigger(trigger)
                .on_open_change(move |open, _window, cx| {
                    // Load branches lazily each time the popover opens.
                    if *open {
                        app_open.update(cx, |state, cx| state.load_branches(cx));
                    }
                })
                .content(move |_state, _window, cx| {
                    let branches = branches.clone();
                    let current = current.clone();
                    let popover = cx.entity();
                    let muted = cx.theme().muted_foreground;
                    let mut col = v_flex()
                        .w(px(220.))
                        .max_h(px(280.))
                        .overflow_hidden()
                        .p_1()
                        .gap_0p5();
                    if branches.is_empty() {
                        col = col.child(
                            div()
                                .px_2()
                                .py_1p5()
                                .text_size(px(13.))
                                .text_color(muted)
                                .child("Loading…"),
                        );
                    } else {
                        for (index, name) in branches.iter().enumerate() {
                            let is_current = *name == current;
                            let branch_name = name.clone();
                            let app_pick = app_content.clone();
                            let pop = popover.clone();
                            col = col.child(
                                h_flex()
                                    .id(("branch-row", index))
                                    .w_full()
                                    .px_2()
                                    .py_1p5()
                                    .gap_2()
                                    .items_center()
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .hover(|s| s.bg(cx.theme().muted))
                                    .child(
                                        div().flex_1().min_w_0().overflow_hidden().child(name.clone()),
                                    )
                                    .when(is_current, |this| {
                                        this.child(
                                            Icon::new(IconName::Check)
                                                .xsmall()
                                                .text_color(cx.theme().primary),
                                        )
                                    })
                                    .on_click(move |_, window, cx| {
                                        let branch_name = branch_name.clone();
                                        app_pick.update(cx, |state, cx| {
                                            state.checkout_branch(branch_name, cx);
                                        });
                                        pop.update(cx, |st, cx| st.dismiss(window, cx));
                                    }),
                            );
                        }
                    }
                    col.into_any_element()
                })
                .into_any_element()
        };

        Some(
            h_flex()
                .w_full()
                .px_2()
                .pt_2()
                .items_center()
                .justify_between()
                .text_size(px(12.))
                .text_color(muted)
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                        .child("Local checkout"),
                )
                .child(right)
                .into_any_element(),
        )
    }

    fn render_approval_panel(&self, request: &ApprovalRequest, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let summary = match &request.kind {
            ApprovalKind::ExecCommand { .. } => "Command approval requested",
            ApprovalKind::FileChange { .. } => "File change approval requested",
            ApprovalKind::ToolUse { .. } => "Tool approval requested",
        };
        let muted = cx.theme().muted_foreground;

        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, .. } => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(command.clone()),
                )
                .when_some(cwd.clone(), |this, cwd| {
                    this.child(div().text_size(px(11.)).text_color(muted).child(format!("in {cwd}")))
                })
                .into_any_element(),
            ApprovalKind::FileChange { changes, .. } => v_flex()
                .gap_0p5()
                .children(changes.iter().map(|change| {
                    div()
                        .text_size(px(12.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!("{} {}", file_change_kind_label(change.kind), change.path))
                }))
                .into_any_element(),
            ApprovalKind::ToolUse { name, input } => div()
                .text_size(px(12.5))
                .font_family(cx.theme().mono_font_family.clone())
                .child(format!("{name} {input}"))
                .into_any_element(),
        };

        let expanded = self.approval_expanded;
        let approve_id = request.id.clone();
        let always_id = request.id.clone();
        let deny_id = request.id.clone();

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_sm()
            .child(
                h_flex()
                    .id("approval-header")
                    .w_full()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.approval_expanded = !this.approval_expanded;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(muted)
                            .child("PENDING APPROVAL"),
                    )
                    .child(div().flex_1().text_size(px(13.)).font_medium().child(summary))
                    .when(count > 1, |this| {
                        this.child(
                            div()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(format!("1/{count}")),
                        )
                    })
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(muted),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    div()
                        .w_full()
                        .p_2()
                        .rounded(px(8.))
                        .bg(cx.theme().muted)
                        .child(detail),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("approval-deny")
                            .ghost()
                            .small()
                            .label("Deny")
                            .text_color(cx.theme().danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(deny_id.clone(), ApprovalDecision::Deny, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-always")
                            .ghost()
                            .small()
                            .label("Always allow")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(always_id.clone(), ApprovalDecision::ApproveForSession, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-approve")
                            .primary()
                            .small()
                            .label("Approve")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(approve_id.clone(), ApprovalDecision::Approve, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn respond(&mut self, request_id: String, decision: ApprovalDecision, cx: &mut Context<Self>) {
        self.approval_expanded = false;
        self.app_state
            .update(cx, |state, cx| state.respond_approval(request_id, decision, cx));
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (turn_running, approval, approval_count) = {
            let state = self.app_state.read(cx);
            match &state.active {
                Some(active) => (
                    active.timeline.turn_running,
                    active.timeline.pending_approvals.first().cloned(),
                    active.timeline.pending_approvals.len(),
                ),
                None => (false, None, 0),
            }
        };

        let border = cx.theme().border;
        let divider = move || div().w_px().h(px(16.)).bg(border);

        // Collapse to the compact "⋯" layout once the row is measured narrower
        // than the threshold. Until the first prepaint measurement lands we
        // assume the full layout (the common wide case).
        let measured = self.control_width.get();
        let compact = measured.is_some_and(|w| w < CONTROL_ROW_COMPACT_BELOW);

        // The control row's width is only known after layout (the paint-phase
        // callback below), one frame behind this render, and that callback
        // cannot itself re-render. So we drive a short animation-frame loop:
        // request another frame after any render that could have changed the
        // measurement, and stop once two consecutive frames agree. This keeps
        // the composer in sync when the diff panel toggles or the window/panels
        // resize, without perpetually rendering when idle.
        let external_trigger = !self.raf_pending;
        self.raf_pending = false;
        let need_frame = external_trigger || measured != self.prev_seen_width;
        self.prev_seen_width = measured;
        if need_frame {
            self.raf_pending = true;
            window.request_animation_frame();
        }

        let control_row_base = h_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .px_2()
            .pb_2()
            .pt_1()
            .gap_1()
            .items_center();

        let control_row = if compact {
            control_row_base
                .child(self.render_model_picker(cx))
                .child(self.render_overflow_menu(cx))
                .child(div().flex_1())
                .child(self.render_send_or_stop(turn_running, cx))
        } else {
            control_row_base
                .child(self.render_model_picker(cx))
                .child(divider())
                .child(self.render_context_chip(cx))
                .child(self.render_static_chip(
                    "permission-chip",
                    Icon::empty().path("icons/lock.svg"),
                    "Ask to edit",
                    "Permission profiles: coming soon",
                    cx,
                ))
                .child(self.render_static_chip(
                    "mode-chip",
                    Icon::empty().path("icons/box.svg"),
                    "Build",
                    "Modes: coming soon",
                    cx,
                ))
                .child(div().flex_1())
                .child(self.render_send_or_stop(turn_running, cx))
        };

        // Measure the control row's laid-out width so the next frame can decide
        // whether to collapse. The paint-phase callback can't mutate the entity
        // or re-run its render, so the width lives in a shared Cell; on a real
        // change we schedule an entity notify on the next frame (outside paint)
        // to re-render with the new layout.
        let width_cell = self.control_width.clone();
        let control_row = control_row.on_prepaint(move |bounds, _window, _cx| {
            let width: f32 = bounds.size.width.into();
            let changed = width_cell
                .get()
                .is_none_or(|prev| (prev - width).abs() > 0.5);
            if changed {
                width_cell.set(Some(width));
            }
        });

        let card = v_flex()
            .w_full()
            .rounded(px(16.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_sm()
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .child(Input::new(&self.input).appearance(false)),
            )
            .child(control_row);

        v_flex()
            .flex_shrink_0()
            .px_4()
            .pt_2()
            .pb_3()
            .gap_2()
            .when_some(approval, |this, request| {
                this.child(self.render_approval_panel(&request, approval_count, cx))
            })
            .child(card)
            .children(self.render_checkout_row(cx))
    }
}

// ---------------------------------------------------------------------------
// Popover panes (free functions: they run in a `PopoverState` context)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_model_pane(
    rows: &[ModelRow],
    selected: &Option<String>,
    rail: PickerRail,
    pending_restart: bool,
    app_entity: &Entity<AppState>,
    model_search: &Entity<InputState>,
    composer: &Entity<Composer>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    // Left rail: favorites star + one glyph per provider.
    let rail_icon = |id: &'static str,
                     icon: Icon,
                     active: bool,
                     target: PickerRail,
                     cx: &mut Context<PopoverState>|
     -> AnyElement {
        let composer = composer.clone();
        div()
            .id(id)
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |s| s.bg(cx.theme().muted))
            .hover(|s| s.bg(cx.theme().muted))
            .child(icon.small().text_color(if active {
                cx.theme().foreground
            } else {
                muted
            }))
            .on_click(move |_, _, cx| {
                composer.update(cx, |c, cx| {
                    c.picker_rail = Some(target);
                    cx.notify();
                });
            })
            .into_any_element()
    };

    let rail_col = v_flex()
        .flex_none()
        .py_2()
        .px_1p5()
        .gap_1()
        .border_r_1()
        .border_color(cx.theme().border)
        .child(rail_icon(
            "rail-fav",
            Icon::new(IconName::Star),
            rail == PickerRail::Favorites,
            PickerRail::Favorites,
            cx,
        ))
        .child(rail_icon(
            "rail-claude",
            provider_glyph(ProviderKind::ClaudeCode),
            rail == PickerRail::Provider(ProviderKind::ClaudeCode),
            PickerRail::Provider(ProviderKind::ClaudeCode),
            cx,
        ))
        .child(rail_icon(
            "rail-codex",
            provider_glyph(ProviderKind::Codex),
            rail == PickerRail::Provider(ProviderKind::Codex),
            PickerRail::Provider(ProviderKind::Codex),
            cx,
        ));

    // Main pane: search + rows.
    let mut list = v_flex().w_full().min_h_0().gap_0p5().px_1().py_1();
    for (index, row) in rows.iter().enumerate() {
        list = list.child(render_model_row(row, index, selected, app_entity, popover, cx));
    }
    if rows.is_empty() {
        list = list.child(
            div()
                .px_3()
                .py_4()
                .text_size(px(13.))
                .text_color(muted)
                .child("No models"),
        );
    }

    let mut pane = v_flex()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(Input::new(model_search).appearance(false)),
        )
        .child(list);
    if pending_restart {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child("applies on next turn session restart"),
        );
    }

    // ⌘1-9 selects the corresponding row while the popover is open.
    let key_rows: Vec<ModelRow> = rows.iter().take(9).cloned().collect();
    let app_key = app_entity.clone();
    let popover_key = popover.clone();

    h_flex()
        .w(px(360.))
        .items_stretch()
        .rounded(px(12.))
        .overflow_hidden()
        .on_key_down(move |ev, window, cx| {
            if !ev.keystroke.modifiers.platform {
                return;
            }
            if let Ok(n) = ev.keystroke.key.parse::<usize>() {
                if n >= 1 && n <= key_rows.len() {
                    let row = &key_rows[n - 1];
                    let id = row.id.map(str::to_string);
                    app_key.update(cx, |s, cx| s.set_active_model(id, cx));
                    popover_key.update(cx, |st, cx| st.dismiss(window, cx));
                }
            }
        })
        .child(rail_col)
        .child(pane)
        .into_any_element()
}

fn render_model_row(
    row: &ModelRow,
    index: usize,
    selected: &Option<String>,
    app_entity: &Entity<AppState>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let is_current = selected.as_deref() == row.id;
    let is_fav = app_entity.read(cx).is_favorite_model(row.name);
    let name = row.name;
    let id = row.id.map(str::to_string);

    let app_select = app_entity.clone();
    let popover_select = popover.clone();
    let app_fav = app_entity.clone();
    let popover_fav = popover.clone();

    h_flex()
        .id(("model-row", index))
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(cx.theme().muted))
        .on_click(move |_, window, cx| {
            app_select.update(cx, |s, cx| s.set_active_model(id.clone(), cx));
            popover_select.update(cx, |st, cx| st.dismiss(window, cx));
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(13.))
                        .child(div().font_medium().child(name))
                        .when(is_current, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .xsmall()
                                    .text_color(cx.theme().primary),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(provider_glyph(row.provider).xsmall())
                        .child(provider_short(row.provider)),
                ),
        )
        .when(index < 9, |this| {
            this.child(
                div()
                    .flex_none()
                    .px_1()
                    .py(px(1.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_size(px(10.))
                    .text_color(muted)
                    .child(format!("⌘{}", index + 1)),
            )
        })
        .child(
            div()
                .id(("model-fav", index))
                .flex_none()
                .p(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().accent))
                .child(
                    Icon::new(if is_fav { IconName::StarFill } else { IconName::Star })
                        .xsmall()
                        .text_color(if is_fav { rgb(CLAUDE_TINT).into() } else { muted }),
                )
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    app_fav.update(cx, |s, cx| s.toggle_favorite_model(name, cx));
                    // Refresh the open popover so the star + ordering update.
                    popover_fav.update(cx, |_, cx| cx.notify());
                }),
        )
        .into_any_element()
}

/// The "⋯" overflow popover: the context chip's usage summary plus the static
/// permission / mode chips, shown when the control row collapses at narrow
/// widths.
fn render_overflow_pane(usage: Option<TokenUsage>, cx: &mut Context<PopoverState>) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let item = |icon: Icon, label: String| -> AnyElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1p5()
            .items_center()
            .rounded(px(6.))
            .text_size(px(13.))
            .text_color(muted)
            .child(icon.small().text_color(muted))
            .child(label)
            .into_any_element()
    };

    v_flex()
        .w(px(220.))
        .p_1()
        .gap_0p5()
        .child(item(Icon::new(IconName::Info), context_label(usage)))
        .child(item(Icon::empty().path("icons/lock.svg"), "Ask to edit".into()))
        .child(item(Icon::empty().path("icons/box.svg"), "Build".into()))
        .into_any_element()
}

fn render_context_pane(usage: Option<TokenUsage>, cx: &mut Context<PopoverState>) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let row = |label: &'static str, value: String, cx: &mut Context<PopoverState>| -> AnyElement {
        h_flex()
            .w_full()
            .justify_between()
            .gap_4()
            .text_size(px(12.))
            .child(div().text_color(muted).child(label))
            .child(div().text_color(cx.theme().foreground).child(value))
            .into_any_element()
    };

    let mut pane = v_flex().w(px(240.)).p_3().gap_1().child(
        div()
            .pb_1()
            .text_size(px(11.))
            .font_medium()
            .text_color(muted)
            .child("CONTEXT"),
    );

    match usage {
        Some(u) => {
            pane = pane
                .child(row("Used", opt_tokens(u.used_tokens.or(u.input_tokens)), cx))
                .child(row("Cached", opt_tokens(u.cached_input_tokens), cx))
                .child(row("Output", opt_tokens(u.output_tokens), cx))
                .child(row("Context window", opt_tokens(u.context_window), cx));
        }
        None => {
            pane = pane.child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("No usage yet this session."),
            );
        }
    }
    pane.into_any_element()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn current_model_name(provider: ProviderKind, model: Option<&str>) -> String {
    if let Some(found) = model_catalog()
        .iter()
        .find(|r| r.provider == provider && r.id == model)
    {
        return found.name.to_string();
    }
    match model {
        Some(id) => id.to_string(),
        None => "Default".to_string(),
    }
}

/// Compact token count, e.g. 42_000 -> "42k", 1_500_000 -> "1.5M".
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        format!("{m:.1}M")
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn opt_tokens(n: Option<u64>) -> String {
    n.map(compact_tokens).unwrap_or_else(|| "—".to_string())
}

/// The context chip label: "42k / 200k" when both known, "200k" when only the
/// window is known, "Context" when nothing is known.
fn context_label(usage: Option<TokenUsage>) -> String {
    match usage {
        Some(u) => {
            let window = u.context_window;
            let used = u.used_tokens.or(u.input_tokens);
            match (used, window) {
                (Some(used), Some(window)) => {
                    format!("{} / {}", compact_tokens(used), compact_tokens(window))
                }
                (Some(used), None) => compact_tokens(used),
                (None, Some(window)) => compact_tokens(window),
                (None, None) => "Context".to_string(),
            }
        }
        None => "Context".to_string(),
    }
}

fn file_change_kind_label(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Create => "create",
        FileChangeKind::Modify => "modify",
        FileChangeKind::Delete => "delete",
        FileChangeKind::Rename => "rename",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_label_variants() {
        assert_eq!(context_label(None), "Context");
        assert_eq!(
            context_label(Some(TokenUsage {
                used_tokens: Some(42_000),
                context_window: Some(200_000),
                ..Default::default()
            })),
            "42k / 200k"
        );
        assert_eq!(
            context_label(Some(TokenUsage {
                context_window: Some(200_000),
                ..Default::default()
            })),
            "200k"
        );
    }

    #[test]
    fn current_model_name_maps_catalog() {
        assert_eq!(current_model_name(ProviderKind::ClaudeCode, None), "Default");
        assert_eq!(
            current_model_name(ProviderKind::ClaudeCode, Some("opus")),
            "Claude Opus"
        );
        // Unknown id falls back to the raw id.
        assert_eq!(
            current_model_name(ProviderKind::Codex, Some("gpt-9")),
            "gpt-9"
        );
    }
}
