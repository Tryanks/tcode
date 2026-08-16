//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel (see docs/DESIGN.md "Composer").

mod components;
mod model;

use components::images::PendingImage;
use model::*;

use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{
    ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalOptionKind, ApprovalRequest,
    InteractionMode, ModelSpec, OptionDescriptor, ProviderCommandKind, ProviderKind, TokenUsage,
    UserInputQuestion,
};
use chrono::Local;
use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClipboardEntry,
    Context, Entity, EventEmitter, ExternalPaths, Focusable as _, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_base::PopoverState;
use gpui_base::{ElementExt as _, StyledExt as _, h_flex, v_flex};

use crate::attachments::attach_error_message;
use crate::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::context_meter;
use crate::palette::fuzzy_score;
use crate::provider_card::{CLAUDE_BRAND_COLOR, provider_glyph};
use crate::settings::provider_label;
use crate::shortcut::format_secondary_shortcut;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::workspace_walk::filter_entries;
use tcode_core::attachments::{mime_from_path, validate_attachment};
use tcode_core::ui::WorkspaceMode;
use tcode_protocol::PathEntry;

/// Blue-500 (normal meter) and red-500 (>90% overloaded), matching T3.
const METER_BLUE: u32 = 0x3B82F6;
const METER_RED: u32 = 0xEF4444;
/// File mentions are potentially unbounded; command and skill feeds are not
/// capped and instead use the trigger menu's scrolling viewport.
const FILE_MENU_ROW_CAP: usize = 50;
const PICKER_PROVIDER_KINDS: [ProviderKind; 4] = [
    ProviderKind::ClaudeCode,
    ProviderKind::Codex,
    ProviderKind::Pi,
    ProviderKind::OpenCode,
];

/// T3's circular stop button red-orange.
const STOP_TINT: u32 = 0xF4562E;
/// Below this measured control-row width the row collapses its context /
/// permission / mode chips into a "⋯" overflow popover so nothing spills past
/// the card edge (diff panel open, or a small window).
const CONTROL_ROW_COMPACT_BELOW: f32 = 520.;

/// Which rail filter the model picker is showing.
#[derive(Clone, PartialEq, Eq)]
enum PickerRail {
    Favorites,
    /// One provider profile (by profile id) — a built-in (`"claude"`/`"codex"`)
    /// or a user-created third-party profile. Each profile is its own rail entry
    /// and lists only its own models.
    Profile(String),
    /// One installed ACP agent (by registry id). ACP agents have no model
    /// catalog — the agent publishes its models over the wire once the session
    /// is up — so this rail lists the agent itself.
    Acp(String),
}

pub enum ComposerEvent {
    /// A turn was just submitted (chat view scrolls to the bottom).
    Submitted,
}

pub struct Composer {
    workspace_store: Entity<WorkspaceStore>,
    input: Entity<TextareaState>,
    /// Dedicated free-form answer field shown inside an agent question card.
    /// Keeping it separate from the turn composer makes the pending question
    /// and the destination of typed text unambiguous.
    user_input_custom: Entity<TextareaState>,
    /// Unsent text is isolated by persisted thread or project New thread page.
    text_cache: ComposerTextCache,
    model_search: Entity<InputState>,
    /// `None` = follow the active session's provider (set on first open).
    picker_rail: Option<PickerRail>,
    /// Whether the approval panel's detail is expanded.
    approval_expanded: bool,
    /// The user-input request currently being answered (its id), plus the
    /// question index and per-question selected option labels. Reset when a new
    /// request arrives or it resolves.
    ui_request_id: Option<String>,
    ui_question_index: usize,
    ui_selections: std::collections::HashMap<String, Vec<String>>,
    /// The placeholder text last applied to the input (so it is only re-set —
    /// which notifies — when it actually changes).
    applied_placeholder: String,
    /// Bumped by `/model` so the model-picker popover re-opens (a fresh popover
    /// instance, keyed by this token, starts open).
    model_picker_token: u64,
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
    /// The inline trigger (`@`/`/`/`$`) active at the cursor, recomputed on every
    /// input change. Drives the trigger menu.
    active_trigger: Option<ComposerTrigger>,
    /// Highlighted row index within the open trigger menu (arrows + hover).
    menu_highlight: usize,
    /// The trigger identity the menu was last shown for; when it changes the
    /// highlight resets and any Escape-dismissal clears.
    menu_last_key: Option<String>,
    /// Set when Escape dismissed the menu (until the query changes).
    menu_dismissed: bool,
    /// Cached workspace listing for the active session cwd (for `@`-mentions),
    /// loaded lazily in the background the first time a mention trigger opens.
    workspace: Option<(PathBuf, Vec<PathEntry>)>,
    workspace_loading: bool,
    /// Pending image attachments for the active session, validated + persisted to
    /// disk. Cleared on send and whenever the active session changes.
    pending_images: Vec<PendingImage>,
    /// The session id `pending_images` belongs to (reset the strip on switch).
    images_session: Option<String>,
    /// Invalidates image jobs when the owning session changes or a turn sends.
    image_load_generation: u64,
    /// Reserved strip slots for image jobs that have not completed yet.
    pending_image_loads: usize,
    /// One cancellable one-second repaint loop, present only while the queue
    /// strip contains at least one scheduled row.
    scheduled_countdown_tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(crate::tr!("composer.placeholder"))
        });
        let model_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("composer.search_models"))
        });
        let user_input_custom = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(1)
                .submit_on_enter(true)
                .placeholder(crate::tr!("userinput.custom_placeholder"))
        });

        let subscriptions = vec![
            // Re-render when app state changes (e.g. the provider's commands /
            // skills feed arrives after session start, feeding the `/`+`$` menus).
            observe_store_topics(
                &workspace_store,
                &[
                    TopicKind::ActiveSession,
                    TopicKind::SessionStatus,
                    TopicKind::SessionEvents,
                    TopicKind::Settings,
                    TopicKind::Providers,
                ],
                cx,
            ),
            cx.subscribe_in(&input, window, |this, input, event, window, cx| {
                match event {
                    InputEvent::PressEnter {
                        shift: false,
                        secondary,
                    } => {
                        // Enter accepts the highlighted trigger-menu row when the
                        // menu is open, otherwise submits the turn.
                        //
                        // The platform modifier (⌘ on macOS, Ctrl elsewhere)
                        // makes it a STEER instead of a QUEUE: the message is
                        // injected into the turn that is already running rather
                        // than held until it finishes. With no turn running the
                        // two are equivalent (there is nothing to steer into).
                        if this.menu_visible() {
                            this.accept_menu(this.menu_highlight, window, cx);
                        } else {
                            let input = input.clone();
                            this.submit(&input, *secondary, window, cx);
                        }
                    }
                    // Recompute the active `@`/`/`/`$` trigger and re-render (also
                    // refreshes the send button's has-text state).
                    InputEvent::Change => {
                        this.recompute_trigger(cx);
                        cx.notify();
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(
                &user_input_custom,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        this.submit_custom_user_input(input, window, cx);
                    }
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            // Live-filter the model picker as the user types in its search box.
            cx.subscribe(&model_search, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        Self {
            workspace_store,
            input,
            user_input_custom,
            text_cache: ComposerTextCache::default(),
            model_search,
            picker_rail: None,
            approval_expanded: false,
            ui_request_id: None,
            ui_question_index: 0,
            ui_selections: std::collections::HashMap::new(),
            applied_placeholder: crate::tr!("composer.placeholder").into_owned(),
            model_picker_token: 0,
            control_width: Rc::new(Cell::new(None)),
            prev_seen_width: None,
            raf_pending: false,
            active_trigger: None,
            menu_highlight: 0,
            menu_last_key: None,
            menu_dismissed: false,
            workspace: None,
            workspace_loading: false,
            pending_images: Vec::new(),
            images_session: None,
            image_load_generation: 0,
            pending_image_loads: 0,
            scheduled_countdown_tick: None,
            _subscriptions: subscriptions,
        }
    }

    /// Save the outgoing destination's text before replacing the shared input
    /// with the incoming destination's cached text. The cache's `current` key
    /// is updated before `set_value`, so the resulting recursive Change event
    /// cannot be attributed to the destination being left.
    fn sync_text_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let destination = self
            .workspace_store
            .read(cx)
            .with_composer_destination(composer_destination)
            .flatten();
        let outgoing_text = self.input.read(cx).value().to_string();
        let Some(incoming_text) = self.text_cache.switch_to(destination, &outgoing_text) else {
            return;
        };
        let cursor = incoming_text.len();
        self.input.update(cx, |state, cx| {
            state.set_value(incoming_text, window, cx);
            state
                .base_state()
                .update(cx, |state, cx| state.set_selected_range(cursor..cursor, cx));
        });
        self.recompute_trigger(cx);
    }

    /// Claude's native conversation rewind returns the selected user prompt.
    /// Put that provider-owned prefill into the ordinary composer so the user
    /// can edit or resend it; no inline transcript editor is involved.
    fn sync_native_rewind_prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = self
            .workspace_store
            .update(cx, |store, _cx| store.take_native_rewind_prefill());
        let Some(prefill) = prefill else {
            return;
        };
        let cursor = prefill.len();
        self.input.update(cx, |state, cx| {
            state.set_value(prefill, window, cx);
            state
                .base_state()
                .update(cx, |state, cx| state.set_selected_range(cursor..cursor, cx));
            state.focus(window, cx);
        });
        self.recompute_trigger(cx);
    }

    /// Whether `submit` has anything to send. Keep the primary-action choice on
    /// this same predicate so attachment/context-only drafts never masquerade
    /// as an empty plan that Enter would implement.
    fn has_sendable_content(&self, cx: &App) -> bool {
        !self.input.read(cx).value().trim().is_empty()
            || !self.pending_images.is_empty()
            || !self
                .workspace_store
                .read(cx)
                .composer_state()
                .terminal_contexts
                .is_empty()
            || !self.workspace_store.read(cx).review_comments().is_empty()
    }

    /// Send the composer's contents. `steer` is set by the ⌘/Ctrl+Enter gesture:
    /// inject into the running turn rather than queue behind it. It is a no-op
    /// when no turn is running (steering just sends), and degrades to
    /// queueing (with a notice) on providers that cannot steer.
    fn submit(
        &mut self,
        input: &Entity<TextareaState>,
        steer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While a user-input question is pending, normal send is suppressed
        // (S1 §7). Typed text becomes the current question's custom answer and
        // flows through the same advance-or-submit path as clicking an option.
        if self.pending_user_input(cx).is_some() {
            self.submit_custom_user_input(input, window, cx);
            return;
        }
        let text = input.read(cx).value().trim().to_string();
        let composer_state = self.workspace_store.read(cx).composer_state();
        let terminal_contexts = composer_state.terminal_contexts;
        if !self.has_sendable_content(cx) {
            return;
        }
        if !composer_state.has_active_session {
            window.push_notification(Notification::info(crate::tr!("composer.no_session")), cx);
            return;
        }
        if terminal_contexts.is_empty()
            && let Some(later) = parse_later(&text, Local::now())
        {
            let Ok((fire_at_unix_secs, message)) = later else {
                window
                    .push_notification(Notification::error(crate::tr!("composer.later_usage")), cx);
                return;
            };
            let attachment_paths = self
                .pending_images
                .iter()
                .map(|image| image.path.clone())
                .collect();
            self.text_cache.clear_current();
            input.update(cx, |state, cx| state.set_value("", window, cx));
            self.pending_images.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
            self.workspace_store.update(cx, |store, _cx| {
                store.schedule_turn(message, attachment_paths, fire_at_unix_secs);
            });
            cx.emit(ComposerEvent::Submitted);
            cx.notify();
            return;
        }
        // Intercept the minimal `/`-command set (S1 §4/§7): `/plan` and
        // `/default` switch mode and are stripped; `/model` opens the picker.
        if terminal_contexts.is_empty()
            && let Some(command) = slash_command(&text)
        {
            self.text_cache.clear_current();
            input.update(cx, |state, cx| state.set_value("", window, cx));
            match command {
                SlashIntent::Plan => self.workspace_store.update(cx, |store, _cx| {
                    store.set_interaction_mode(InteractionMode::Plan)
                }),
                SlashIntent::Default => self.workspace_store.update(cx, |store, _cx| {
                    store.set_interaction_mode(InteractionMode::Build)
                }),
                SlashIntent::Model => {
                    self.model_picker_token = self.model_picker_token.wrapping_add(1);
                }
            }
            cx.notify();
            return;
        }
        let orchestrate_text = strip_orchestrate_prefix(&text).map(str::to_string);
        let prompt_text = orchestrate_text.as_deref().unwrap_or(&text);
        let sent_text = prompt_text.to_string();
        let attachment_paths = self
            .pending_images
            .iter()
            .map(|image| image.path.clone())
            .collect::<Vec<_>>();
        if let Some((from, to)) = self
            .workspace_store
            .read(cx)
            .composer_state()
            .relay_confirmation
        {
            let composer = cx.entity();
            let input = input.clone();
            window.open_alert_dialog(cx, move |alert, _, cx| {
                let alert = alert.bg(cx.theme().popover);
                let composer = composer.clone();
                let input = input.clone();
                let sent_text = sent_text.clone();
                let attachment_paths = attachment_paths.clone();
                alert
                    .title(crate::tr!("composer.relay_title"))
                    .description(crate::tr!(
                        "composer.relay_description",
                        from = from,
                        to = to
                    ))
                    .button_props(
                        DialogButtons::default()
                            .ok_text(crate::tr!("composer.relay_confirm"))
                            .cancel_text(crate::tr!("composer.relay_cancel"))
                            .show_cancel(true),
                    )
                    .on_ok(move |_, window, cx| {
                        composer.update(cx, |composer, cx| {
                            composer.finish_submit(
                                &input,
                                sent_text.clone(),
                                attachment_paths.clone(),
                                false,
                                false,
                                true,
                                window,
                                cx,
                            );
                        });
                        true
                    })
            });
            return;
        }
        self.finish_submit(
            input,
            sent_text,
            attachment_paths,
            orchestrate_text.is_some(),
            steer,
            false,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_submit(
        &mut self,
        input: &Entity<TextareaState>,
        sent_text: String,
        attachment_paths: Vec<PathBuf>,
        orchestrate: bool,
        steer: bool,
        relay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text_cache.clear_current();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.pending_images.clear();
        self.image_load_generation = self.image_load_generation.wrapping_add(1);
        self.pending_image_loads = 0;
        self.workspace_store.update(cx, |store, _cx| {
            if relay {
                store.confirm_relay_and_send(sent_text, attachment_paths);
            } else if orchestrate {
                store.orchestrate_turn(sent_text, attachment_paths);
            } else if steer {
                store.steer(sent_text, attachment_paths);
            } else {
                store.send_turn(sent_text, attachment_paths);
            }
        });
        cx.emit(ComposerEvent::Submitted);
        cx.notify();
    }

    // -- inline triggers (@ mentions / $ skills / commands) ----------------

    // -- image attachments --------------------------------------------------

    // -- control-row popovers ----------------------------------------------

    // -- trigger menu + image strip ----------------------------------------

    // -- send / stop --------------------------------------------------------

    fn render_send_or_stop(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if turn_running {
            // Providers with native mid-turn steering keep a send button active
            // beside Stop while a turn runs.
            let steers = self
                .workspace_store
                .read(cx)
                .composer_state()
                .steering_supported;
            let has_text = !self.input.read(cx).value().trim().is_empty();
            let mut row = h_flex()
                .gap_2()
                .items_center()
                // Blue activity spinner.
                .child(Spinner::new().small().color(cx.theme().primary));
            if steers {
                let queue_hint = crate::tr!(
                    "composer.queue_hint",
                    shortcut = format_secondary_shortcut("enter")
                )
                .into_owned();
                let (bg, fg) = if has_text {
                    (cx.theme().primary, cx.theme().primary_foreground)
                } else {
                    (cx.theme().muted, cx.theme().muted_foreground)
                };
                row = row.child(
                    crate::material::accessible_clickable(
                        div(),
                        "steer-turn",
                        Role::Button,
                        crate::tr!("composer.steer_tooltip"),
                        cx,
                    )
                    .size(px(28.))
                    .rounded(crate::material::radius_input())
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(bg)
                    .cursor_pointer()
                    .when(has_text, |s| s.hover(|s| s.opacity(0.9)))
                    .tooltip(move |window, cx| {
                        crate::widgets::tooltip::Tooltip::new(queue_hint.clone()).build(window, cx)
                    })
                    .child(Icon::new(IconName::ArrowUp).small().text_color(fg))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let input = this.input.clone();
                        this.submit(&input, false, window, cx);
                    })),
                );
            }
            return row
                // Circular red-orange stop button.
                .child(
                    crate::material::accessible_clickable(
                        div(),
                        "stop-turn",
                        Role::Button,
                        crate::tr!("composer.stop"),
                        cx,
                    )
                    .size(px(28.))
                    .rounded(crate::material::radius_input())
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(STOP_TINT))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .child(div().size(px(11.)).rounded(px(2.)).bg(gpui::white()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.interrupt());
                    })),
                )
                .into_any_element();
        }

        // Group C: while the first send is creating a worktree, show a disabled
        // "Preparing worktree…" pill instead of the send button.
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .preparing_worktree
        {
            return h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small().color(cx.theme().primary))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("composer.preparing_worktree")),
                )
                .into_any_element();
        }

        let has_text = !self.input.read(cx).value().trim().is_empty();
        let (bg, fg) = if has_text {
            (cx.theme().primary, cx.theme().primary_foreground)
        } else {
            (cx.theme().muted, cx.theme().muted_foreground)
        };
        crate::material::accessible_clickable(
            div(),
            "send-message",
            Role::Button,
            crate::tr!("composer.send"),
            cx,
        )
        .size(px(28.))
        .rounded(crate::material::radius_input())
        .flex()
        .items_center()
        .justify_center()
        .bg(bg)
        .cursor_pointer()
        .when(has_text, |s| s.hover(|s| s.opacity(0.9)))
        .child(Icon::new(IconName::ArrowUp).small().text_color(fg))
        .on_click(cx.listener(|this, _, window, cx| {
            let input = this.input.clone();
            this.submit(&input, false, window, cx);
        }))
        .into_any_element()
    }

    /// The composer's primary control: the stop button while a turn runs, the
    /// Refine / Implement (split) controls in the plan-ready state, else send.
    fn render_primary_action(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if turn_running {
            return self.render_send_or_stop(true, cx);
        }
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .plan_ready_markdown
            .is_some()
        {
            if self.has_sendable_content(cx) && self.refines_the_plan(cx) {
                // Refine: send the feedback and stay in Plan mode (a normal send
                // while the session is in Plan mode continues planning).
                return Button::new("plan-refine")
                    .primary()
                    .label(crate::tr!("plan.refine"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let input = this.input.clone();
                        this.submit(&input, false, window, cx);
                    }))
                    .into_any_element();
            }
            if !self.has_sendable_content(cx) {
                return self.render_implement_split(cx);
            }
        }
        self.render_send_or_stop(turn_running, cx)
    }

    // -- user-input question panel (S1 §7) ---------------------------------

    // -- below-card + approval ---------------------------------------------
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_user_input_state(window, cx);
        self.sync_images_session(cx);
        self.sync_text_destination(window, cx);
        self.sync_native_rewind_prefill(window, cx);
        let composer_state = self.workspace_store.read(cx).composer_state();
        let turn_running = composer_state.turn_running;
        let approval = composer_state.pending_approval;
        let approval_count = composer_state.pending_approval_count;

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
            .gap_1()
            .items_center();

        let control_row = if compact {
            control_row_base
                .child(self.render_model_picker(cx))
                .child(self.render_overflow_menu(cx))
                .child(div().flex_1())
                .child(self.render_primary_action(turn_running, cx))
        } else {
            control_row_base
                .child(self.render_model_picker(cx))
                .child(self.render_traits_picker(cx))
                .child(divider())
                .child(self.render_context_meter(cx))
                .child(self.render_permission_picker(cx))
                .child(self.render_mode_chip(cx))
                .child(div().flex_1())
                .child(self.render_primary_action(turn_running, cx))
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

        // Plan-ready state: a "Plan Ready" header strip + refine placeholder.
        let plan_ready_title = self
            .workspace_store
            .read(cx)
            .composer_state()
            .plan_ready_markdown
            .map(|md| {
                tcode_core::session::plan_title(&md)
                    .unwrap_or_else(|| crate::tr!("plan.proposed_plan").into_owned())
            });
        // Only Plan mode refines: in Build a typed message is an ordinary build
        // turn, so promising refinement there would misdescribe what Enter does.
        let desired_placeholder = if plan_ready_title.is_some() && self.refines_the_plan(cx) {
            crate::tr!("plan.refine_placeholder").into_owned()
        } else {
            crate::tr!("composer.placeholder").into_owned()
        };
        if self.applied_placeholder != desired_placeholder {
            self.applied_placeholder = desired_placeholder.clone();
            self.input.update(cx, |state, cx| {
                state.base_state().update(cx, |state, cx| {
                    state.set_placeholder(desired_placeholder, window, cx)
                })
            });
        }

        let user_input = self.pending_user_input(cx);

        // Dropping image files onto the card attaches them (T3 drag-drop).
        let composer = cx.entity();
        let terminal_contexts = self
            .workspace_store
            .read(cx)
            .composer_state()
            .terminal_contexts;
        let has_terminal_contexts = !terminal_contexts.is_empty();
        let context_chips =
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .children(terminal_contexts.into_iter().map(|context| {
                    let id = context.id;
                    let range = if context.line_start == context.line_end {
                        format!("L{}", context.line_start)
                    } else {
                        format!("L{}-L{}", context.line_start, context.line_end)
                    };
                    Button::new(("terminal-context-chip", id))
                        .ghost()
                        .small()
                        .h(px(22.))
                        .rounded(crate::material::radius_chip())
                        .text_size(px(11.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .label(format!("{} · {}  ×", context.terminal_label, range))
                        .tooltip(context.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_terminal_context(id));
                        }))
                }));
        let review_comments = self.workspace_store.read(cx).review_comments();
        let has_review_comments = !review_comments.is_empty();
        let review_chips = h_flex().w_full().flex_wrap().gap_1().children(
            review_comments
                .into_iter()
                .enumerate()
                .map(|(index, comment)| {
                    let range = if comment.line_start == comment.line_end {
                        format!("L{}", comment.line_start)
                    } else {
                        format!("L{}-L{}", comment.line_start, comment.line_end)
                    };
                    Button::new(("review-comment-chip", index))
                        .ghost()
                        .small()
                        .h(px(22.))
                        .rounded(crate::material::radius_chip())
                        .text_size(px(11.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .label(format!("{} {}  ×", comment.file, range))
                        .tooltip(comment.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_review_comment(index));
                        }))
                }),
        );

        // Focus swaps the hairline to primary in one frame. Geometry stays
        // fixed: focus never changes border width, radius, or layout.
        let composer_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let card = v_flex()
            .w_full()
            .gap_1p5()
            .p(px(6.))
            .rounded(crate::material::radius_composer())
            .border_1()
            .border_color(if composer_focused {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            // White console on paper (T3-grade fill): the glass `background`
            // token would render as a murky translucent wash here.
            .bg(cx.theme().popover)
            .shadow_md()
            // Secondary+V with image clipboard content, and arrow/Escape trigger-menu
            // navigation (fires after the input's own key actions).
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if key == "v" && ev.keystroke.modifiers.secondary() {
                    this.paste_clipboard_image(window, cx);
                    return;
                }
                if this.handle_user_input_digit(ev, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                if !this.menu_visible() {
                    return;
                }
                let (rows, _, _) = this.menu_rows(cx);
                match key {
                    "up" => {
                        this.menu_highlight = this.menu_highlight.saturating_sub(1);
                        cx.notify();
                    }
                    "down" => {
                        if !rows.is_empty() {
                            this.menu_highlight = (this.menu_highlight + 1).min(rows.len() - 1);
                        }
                        cx.notify();
                    }
                    "escape" => {
                        this.menu_dismissed = true;
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            .on_drop(
                move |paths: &ExternalPaths, window: &mut Window, cx: &mut App| {
                    let paths: Vec<PathBuf> = paths.paths().to_vec();
                    composer.update(cx, |this, cx| {
                        for path in paths {
                            if mime_from_path(&path).starts_with("image/") {
                                this.add_image_path(path, window, cx);
                            }
                        }
                    });
                },
            )
            .when_some(plan_ready_title, |this, title| {
                this.child(self.render_plan_ready_header(title, cx))
            })
            .when(has_terminal_contexts, |this| this.child(context_chips))
            .when(has_review_comments, |this| this.child(review_chips))
            .child(Textarea::new(&self.input).appearance(false))
            .children(self.render_image_strip(cx))
            .child(control_row);

        // Mirror the chat timeline's centered content column (same page
        // padding, same max width) so the composer lines up with the messages
        // above it instead of stretching edge to edge on wide windows.
        v_flex()
            .relative()
            .flex_shrink_0()
            .w_full()
            .items_center()
            .px(px(crate::chat::CONTENT_MIN_PADDING))
            .pt_2()
            .pb_3()
            // Shift+Tab toggles Build ↔ Plan (S1 §4).
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "tab" && ev.keystroke.modifiers.shift {
                    this.workspace_store
                        .update(cx, |store, _cx| store.toggle_interaction_mode());
                    cx.notify();
                }
            }))
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(crate::chat::CONTENT_MAX_WIDTH))
                    .gap_2()
                    .when_some(approval, |this, request| {
                        this.child(self.render_approval_panel(&request, approval_count, cx))
                    })
                    .when_some(user_input, |this, (request_id, questions)| {
                        this.child(self.render_user_input_panel(request_id, questions, cx))
                    })
                    .children(self.render_trigger_menu(cx))
                    .children(self.render_queue_strip(cx))
                    .child(card)
                    .children(self.render_checkout_row(cx)),
            )
    }
}
