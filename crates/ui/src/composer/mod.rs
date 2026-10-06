//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel.

mod components;
pub(crate) mod model;

use components::images::PendingImage;
#[cfg(feature = "voice")]
use components::voice::Voice;
use model::*;

use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;
use std::time::Duration;

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState, Paste, Textarea, TextareaState};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
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
use gpui_base::input::{MoveDown, MoveUp};

pub(crate) const CONTEXT: &str = "Composer";

gpui::actions!(tcode_composer, [ToggleInteractionMode]);

/// A context-menu action on the composer's chips, queue rows and pending
/// images, which the composer applies as their buttons do.
#[derive(gpui::Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = tcode_composer, no_json)]
enum ComposerMenu {
    RemoveTerminalContext(u64),
    RemoveReviewComment(usize),
    QueueSteer(u64),
    QueueEdit { id: u64, text: String },
    QueueDrop(u64),
    ImageOpen(usize),
    ImageRemove(usize),
}
use gpui_base::{ElementExt as _, StyledExt as _, h_flex, v_flex};

use crate::attachments::attach_error_message;
use crate::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::context_meter;
use crate::palette::fuzzy_score;
use crate::provider_card::provider_glyph;
use crate::settings::provider_label;
use crate::shortcut::format_secondary_shortcut;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::workspace_walk::filter_entries;
use tcode_core::attachments::{mime_from_path, validate_attachment};
use tcode_core::provider_models::FastMode;
use tcode_core::ui::WorkspaceMode;
use tcode_protocol::PathEntry;

/// Context-meter colors, shared with provider usage bars.
use crate::usage::{METER_BLUE, METER_RED};
/// File mentions are potentially unbounded; command and skill feeds are not
/// capped and instead use the trigger menu's scrolling viewport.
const FILE_MENU_ROW_CAP: usize = 50;

/// Stop-button red-orange.
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

/// How far the draft field may grow before it scrolls. A compact window has to
/// leave room for the keyboard, so it starts taller and stops sooner.
fn auto_grow_rows(compact: bool) -> (usize, usize) {
    if compact { (2, 5) } else { (1, 8) }
}

fn draft_placeholder(compact: bool) -> String {
    if compact {
        crate::tr!("mobile.message").into_owned()
    } else {
        crate::tr!("composer.placeholder").into_owned()
    }
}

pub struct Composer {
    compact: bool,
    workspace_store: Entity<WorkspaceStore>,
    input: Entity<TextareaState>,
    /// Dedicated free-form answer field shown inside an agent question card.
    /// Keeping it separate from the turn composer makes the pending question
    /// and the destination of typed text unambiguous.
    user_input_custom: Entity<TextareaState>,
    /// The editable clarification draft suggested by the fallback reviewer, plus
    /// the draft last seeded into it — the field is only re-seeded when the
    /// suggestion itself changes, so the user's edits survive re-renders.
    fallback_review_input: Entity<TextareaState>,
    fallback_review_seeded: Option<String>,
    /// Unsent text is isolated by persisted thread or project New thread page.
    text_cache: ComposerTextCache,
    model_search: Entity<InputState>,
    context_window_custom: Entity<InputState>,
    context_window_custom_error: bool,
    traits_popover: Option<Entity<PopoverState>>,
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
    /// A non-blocking request the user closed on this client; its panel stays
    /// hidden until a newer request replaces it. The agent keeps working and
    /// the question text remains in the transcript.
    ui_dismissed_request_id: Option<String>,
    /// Whether the question card shows its answer area. A blocking request
    /// opens it; a non-blocking one arrives closed so it never takes the
    /// composer's place.
    ui_expanded: bool,
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
    queued_refill: Option<(String, u64, String)>,
    /// Mic button + live dictation session (see `components::voice`).
    #[cfg(feature = "voice")]
    voice: Voice,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    fn interactive(&self, cx: &App) -> bool {
        !self.workspace_store.read(cx).native_subagent_readonly()
            && !matches!(self.workspace_store.read(cx).connection_state(),
            tcode_client::ConnectionState::Offline { reason } if reason.is_terminal())
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.interactive(cx) && !crate::window_seam::is_mobile(cx) {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    #[cfg(test)]
    pub(crate) fn input_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn model_search_focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.model_search.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn draft(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    #[cfg(test)]
    pub(crate) fn set_draft(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.replace_all(text, window, cx));
    }

    #[cfg(test)]
    pub(crate) fn is_compact(&self) -> bool {
        self.compact
    }

    /// Follow the window onto the other layout. Updated in place on purpose: a
    /// rebuilt composer would drop the draft, its selection and its pending
    /// attachments, and the user only resized a window.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        let (min_rows, max_rows) = auto_grow_rows(compact);
        // The placeholder has one owner in `render`; nudge it to re-apply there.
        self.applied_placeholder.clear();
        self.input
            .update(cx, |input, cx| input.set_auto_grow(min_rows, max_rows, cx));
        cx.notify();
    }
    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_layout(workspace_store, false, window, cx)
    }

    pub fn new_with_layout(
        workspace_store: Entity<WorkspaceStore>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (min_rows, max_rows) = auto_grow_rows(compact);
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(min_rows, max_rows)
                // Whether Enter sends is an input-device question, not a width
                // one: a wide tablet still types on glass, and a desktop window
                // dragged narrow still has a hardware Enter key.
                .submit_on_enter(!gpui_base::is_mobile())
                .placeholder(draft_placeholder(compact))
        });
        let model_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("composer.search_models"))
        });
        let context_window_custom = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::tr!("composer.context_window_custom_placeholder"))
        });
        let user_input_custom = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(1)
                .submit_on_enter(true)
                .placeholder(crate::tr!("userinput.custom_placeholder"))
        });
        let fallback_review_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .placeholder(crate::tr!("fallback.review_placeholder"))
        });

        let subscriptions = vec![
            // Re-render when app state changes (e.g. the provider's commands /
            // skills feed arrives after session start, feeding the `/`+`$` menus).
            observe_store_topics(
                &workspace_store,
                &[
                    TopicKind::ActiveSession,
                    TopicKind::Index,
                    TopicKind::SessionStatus,
                    TopicKind::SessionPlan,
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
                        if this.menu_visible(cx) {
                            this.accept_menu(this.menu_highlight, window, cx);
                        } else {
                            let input = input.clone();
                            this.submit(&input, *secondary, window, cx);
                        }
                    }
                    // Recompute the active `@`/`/`/`$` trigger and re-render (also
                    // refreshes the send button's has-text state).
                    InputEvent::Change => {
                        this.queued_refill = None;
                        // An edit that did not come from the transcript writer
                        // ends dictation (see `components::voice`).
                        #[cfg(feature = "voice")]
                        this.stop_dictation_on_user_edit(cx);
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
            cx.subscribe(&fallback_review_input, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&model_search, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &context_window_custom,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        let value = input.read(cx).value().to_string();
                        if let Some(tokens) = agent::claude::parse_context_window_tokens(
                            &serde_json::Value::String(value),
                        ) {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.set_active_option(
                                    "contextWindow".to_string(),
                                    Some(serde_json::json!(tokens)),
                                );
                            });
                            this.context_window_custom_error = false;
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                            if let Some(popover) = this.traits_popover.clone() {
                                popover.update(cx, |state, cx| state.dismiss(window, cx));
                            }
                        } else {
                            this.context_window_custom_error = true;
                            cx.notify();
                        }
                    }
                    InputEvent::Change => {
                        this.context_window_custom_error = false;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];

        #[cfg(feature = "voice")]
        let (voice, subscriptions) = {
            let mut subscriptions = subscriptions;
            let voice = Voice::new(window, cx, &mut subscriptions);
            (voice, subscriptions)
        };

        Self {
            compact,
            workspace_store,
            input,
            user_input_custom,
            fallback_review_input,
            fallback_review_seeded: None,
            text_cache: ComposerTextCache::default(),
            model_search,
            context_window_custom,
            context_window_custom_error: false,
            traits_popover: None,
            picker_rail: None,
            approval_expanded: compact,
            ui_request_id: None,
            ui_question_index: 0,
            ui_selections: std::collections::HashMap::new(),
            ui_dismissed_request_id: None,
            ui_expanded: false,
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
            queued_refill: None,
            #[cfg(feature = "voice")]
            voice,
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
        // The dictation anchor belongs to the text we are about to swap out.
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        let cursor = incoming_text.len();
        self.input.update(cx, |state, cx| {
            state.set_value(incoming_text, window, cx);
            state.set_selected_range(cursor..cursor, cx);
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
        self.set_input_text(prefill, window, cx);
    }

    /// Replace the composer text with `text`, caret at the end.
    /// Software-keyboard devices wait for an explicit tap to focus.
    fn set_input_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = text.len();
        self.input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(cursor..cursor, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    /// Remove a queued message and return its text to the composer for editing.
    /// Selecting the replacement lets the user discard it with one keystroke,
    /// while queued attachments are deliberately ignored.
    fn on_menu(&mut self, action: &ComposerMenu, window: &mut Window, cx: &mut Context<Self>) {
        match action.clone() {
            ComposerMenu::RemoveTerminalContext(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.remove_terminal_context(id)),
            ComposerMenu::RemoveReviewComment(index) => self
                .workspace_store
                .update(cx, |store, _cx| store.remove_review_comment(index)),
            ComposerMenu::QueueSteer(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.steer_queued(id)),
            ComposerMenu::QueueEdit { id, text } => {
                self.drop_queued_and_refill(id, text, window, cx)
            }
            ComposerMenu::QueueDrop(id) => self
                .workspace_store
                .update(cx, |store, _cx| store.drop_queued(id)),
            ComposerMenu::ImageOpen(index) => self.open_image_preview(index, window, cx),
            ComposerMenu::ImageRemove(index) => self.remove_image(index, cx),
        }
    }

    fn drop_queued_and_refill(
        &mut self,
        id: u64,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.read(cx);
        if !self.interactive(cx)
            || !store.composer_state().queue.is_some_and(|queue| {
                queue
                    .messages
                    .iter()
                    .any(|message| message.id == id && message.editable)
            })
        {
            return;
        }
        let Some(session_id) = store.active_session_id() else {
            return;
        };
        self.queued_refill = Some((session_id, id, text));
        self.workspace_store
            .update(cx, |store, _cx| store.drop_queued(id));
        cx.notify();
    }

    fn sync_queued_refill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session_id, id, _)) = &self.queued_refill else {
            return;
        };
        let store = self.workspace_store.read(cx);
        if store.active_session_id().as_ref() != Some(session_id) {
            self.queued_refill = None;
            return;
        }
        let Some(queue) = store.composer_state().queue else {
            return;
        };
        if let Some(message) = queue.messages.iter().find(|message| message.id == *id) {
            if !message.editable {
                self.queued_refill = None;
            }
            return;
        }
        let (_, _, text) = self.queued_refill.take().unwrap();
        let selection = 0..text.len();
        self.input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(selection, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
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
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .conversation_read_only
        {
            return;
        }
        if self.compact
            && !self
                .workspace_store
                .read(cx)
                .connection_state()
                .is_connected()
        {
            return;
        }
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        // A blocking question uses this text as the current custom answer,
        // advancing through the same path as an option click. A non-blocking
        // one leaves the composer to ordinary sends and steers.
        if self
            .pending_user_input(cx)
            .is_some_and(|pending| pending.delivery.is_blocking())
        {
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
        // Local mode and picker commands are consumed without sending a turn.
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
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
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

    /// Compact control that switches between Send, Queue and Stop.
    fn render_compact_primary_action(
        &self,
        turn_running: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let interactive = self.interactive(cx);
        let has_text = interactive && !self.input.read(cx).value().trim().is_empty();
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .preparing_worktree
        {
            return div()
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new().small().color(cx.theme().primary))
                .into_any_element();
        }
        let stopping = turn_running && !has_text;
        let stop_pending = self.workspace_store.read(cx).composer_state().stopping;
        let (label, id): (_, &'static str) = if stopping {
            (crate::tr!("composer.stop"), "stop-turn")
        } else if turn_running {
            (crate::tr!("mobile.queue"), "steer-turn")
        } else {
            (crate::tr!("composer.send"), "send-message")
        };
        let (bg, fg) = if stopping {
            (rgb(STOP_TINT).into(), gpui::white())
        } else if has_text {
            (cx.theme().primary, cx.theme().primary_foreground)
        } else {
            (cx.theme().muted, cx.theme().muted_foreground)
        };
        // Keep the touch target larger than the visible circle.
        let button = crate::material::accessible_clickable(div(), id, Role::Button, label, cx)
            .debug_selector(move || id.into())
            .size(px(44.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(stopping && (!interactive || stop_pending), |el| {
                el.opacity(0.4)
            })
            .child(
                div()
                    .size(px(40.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(bg)
                    .child(if stopping {
                        div()
                            .size(px(13.))
                            .rounded(px(2.))
                            .bg(gpui::white())
                            .into_any_element()
                    } else {
                        Icon::new(IconName::ArrowUp)
                            .size(px(18.))
                            .text_color(fg)
                            .into_any_element()
                    }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if stopping {
                    if this.interactive(cx) {
                        this.workspace_store
                            .update(cx, |store, _cx| store.interrupt());
                    }
                    return;
                }
                let input = this.input.clone();
                this.submit(&input, false, window, cx);
            }));
        v_flex()
            .flex_none()
            .items_center()
            .gap(px(1.))
            .child(button)
            .when(turn_running && has_text, |el| {
                el.child(
                    div()
                        .text_size(px(10.))
                        .line_height(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!("mobile.queue")),
                )
            })
            .into_any_element()
    }

    fn render_send_or_stop(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if self.compact {
            return self.render_compact_primary_action(turn_running, cx);
        }
        if turn_running {
            // Providers with native mid-turn steering keep a send button active
            // beside Stop while a turn runs.
            let steers = self
                .workspace_store
                .read(cx)
                .composer_state()
                .steering_supported;
            let has_text = self.interactive(cx) && !self.input.read(cx).value().trim().is_empty();
            let mut row = h_flex()
                .gap_2()
                .items_center()
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
                    .rounded(crate::material::radius_input(cx))
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
                .child(
                    crate::material::accessible_clickable(
                        div(),
                        "stop-turn",
                        Role::Button,
                        crate::tr!("composer.stop"),
                        cx,
                    )
                    .size(px(28.))
                    .rounded(crate::material::radius_input(cx))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(STOP_TINT))
                    .when(
                        !self.interactive(cx)
                            || self.workspace_store.read(cx).composer_state().stopping,
                        |el| el.opacity(0.4),
                    )
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .child(div().size(px(11.)).rounded(px(2.)).bg(gpui::white()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.interactive(cx) {
                            return;
                        }
                        this.workspace_store
                            .update(cx, |store, _cx| store.interrupt());
                    })),
                )
                .into_any_element();
        }

        // First-send worktree preparation temporarily disables the send control.
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

        let has_text = self.interactive(cx) && !self.input.read(cx).value().trim().is_empty();
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
        .rounded(crate::material::radius_input(cx))
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
        if self
            .workspace_store
            .read(cx)
            .composer_state()
            .conversation_read_only
        {
            return Button::new("send-message")
                .debug_selector(|| "send-message".into())
                .ghost()
                .compact()
                .disabled(true)
                .aria_label(crate::tr!("composer.send").into_owned())
                .size(px(if self.compact { 44. } else { 28. }))
                .child(Icon::new(IconName::ArrowUp).small())
                .into_any_element();
        }
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
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_user_input_state(window, cx);
        self.sync_images_session(cx);
        self.sync_text_destination(window, cx);
        self.sync_queued_refill(window, cx);
        self.sync_native_rewind_prefill(window, cx);
        self.sync_fallback_review_draft(window, cx);
        let composer_state = self.workspace_store.read(cx).composer_state();
        let readonly = composer_state.conversation_read_only;
        let turn_running = composer_state.turn_running;
        let approval = composer_state.pending_approval;
        let approval_count = composer_state.pending_approval_count;

        let border = cx.theme().border;
        let divider = move || div().w_px().h(px(16.)).bg(border);

        // Collapse to the compact "⋯" layout once the row is measured narrower
        // than the threshold. Until the first prepaint measurement lands we
        // assume the full layout (the common wide case).
        let measured = self.control_width.get();
        let compact = self.compact || measured.is_some_and(|w| w < CONTROL_ROW_COMPACT_BELOW);

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

        #[cfg(feature = "voice")]
        let mic = if self.compact || readonly {
            None
        } else {
            self.render_mic_button(cx)
        };
        #[cfg(not(feature = "voice"))]
        let mic: Option<AnyElement> = None;

        // The platform picker is the phone's only way in; desktops paste and
        // drop, and the readonly and offline states have nothing to add to.
        let attach_menu = (self.compact && !readonly && self.interactive(cx))
            .then(|| {
                cx.try_global::<crate::remote::ClientAttachment>()
                    .is_some_and(|client| client.host().supports_image_picker())
                    .then(|| self.render_attach_menu(cx))
            })
            .flatten();
        let control_row = if self.compact || (readonly && compact) {
            control_row_base
                .children(attach_menu)
                .child(div().flex_1().min_w_0().child(self.render_model_picker(cx)))
                .child(self.render_traits_picker(cx))
                .child(self.render_primary_action(turn_running, cx))
        } else if compact {
            control_row_base
                .child(self.render_model_picker(cx))
                .child(self.render_overflow_menu(cx))
                .child(div().flex_1())
                .children(mic)
                .child(self.render_context_meter(cx))
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
                .children(mic)
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
        let desired_placeholder = if readonly {
            crate::tr!("chat.subagent_readonly").into_owned()
        } else if plan_ready_title.is_some() && self.refines_the_plan(cx) {
            crate::tr!("plan.refine_placeholder").into_owned()
        } else if self.compact && !self.interactive(cx) {
            crate::tr!("mobile.offline_message").into_owned()
        } else {
            draft_placeholder(self.compact)
        };
        if self.applied_placeholder != desired_placeholder {
            self.applied_placeholder = desired_placeholder.clone();
            self.input.update(cx, |state, cx| {
                state.set_placeholder(desired_placeholder, window, cx)
            });
        }

        let user_input = self.pending_user_input(cx);
        let fallback_block = self
            .workspace_store
            .read(cx)
            .active_fallback_block()
            .cloned();
        let fallback_review = self
            .workspace_store
            .read(cx)
            .active_fallback_review()
            .cloned();

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
                    let text = context.text.clone();
                    Button::new(("terminal-context-chip", id))
                        .ghost()
                        .small()
                        .h(px(22.))
                        .rounded(crate::material::radius_chip(cx))
                        .text_size(px(11.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .label(format!("{} · {}  ×", context.terminal_label, range))
                        .tooltip(context.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_terminal_context(id));
                        }))
                        .context_menu(move |menu, _, _| {
                            menu.menu(
                                crate::tr!("chat.copy_text").into_owned(),
                                Box::new(CopyText(text.clone())),
                            )
                            .separator()
                            .menu(
                                crate::tr!("composer.remove_context").into_owned(),
                                Box::new(ComposerMenu::RemoveTerminalContext(id)),
                            )
                        })
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
                    let text = comment.text.clone();
                    Button::new(("review-comment-chip", index))
                        .ghost()
                        .small()
                        .h(px(22.))
                        .rounded(crate::material::radius_chip(cx))
                        .text_size(px(11.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .label(format!("{} {}  ×", comment.file, range))
                        .tooltip(comment.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, _cx| store.remove_review_comment(index));
                        }))
                        .context_menu(move |menu, _, _| {
                            menu.menu(
                                crate::tr!("chat.copy_text").into_owned(),
                                Box::new(CopyText(text.clone())),
                            )
                            .separator()
                            .menu(
                                crate::tr!("composer.remove_context").into_owned(),
                                Box::new(ComposerMenu::RemoveReviewComment(index)),
                            )
                        })
                }),
        );

        // Focus swaps the hairline to primary in one frame. Geometry stays
        // fixed: focus never changes border width, radius, or layout.
        let composer_focused = !readonly && self.input.read(cx).focus_handle(cx).is_focused(window);
        let card = v_flex()
            .debug_selector(|| "composer-card".into())
            .w_full()
            .gap_1p5()
            .p(px(6.))
            .rounded(if self.compact {
                px(16.)
            } else {
                crate::material::radius_composer(cx)
            })
            .border_1()
            .border_color(if composer_focused {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            // The editor needs an opaque fill over the translucent canvas.
            .bg(cx.theme().popover)
            .shadow_md()
            // The editor binds Secondary+V to Paste, which consumes the keystroke
            // before any key-down listener runs — so image paste must intercept
            // the Paste *action* in the capture phase. Swallow it only when the
            // clipboard held an image; text paste propagates to the editor.
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.interactive(cx) && !this.compact && this.paste_clipboard_image(window, cx) {
                    cx.stop_propagation();
                }
            }))
            // Arrow/Escape trigger-menu navigation (fires after the input's own
            // key actions).
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if this.handle_user_input_digit(ev, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                // Escape ends dictation (keeping the transcript) before it can
                // mean anything else.
                #[cfg(feature = "voice")]
                if key == "escape" && this.stop_dictation(cx) {
                    cx.stop_propagation();
                    return;
                }
                if key == "escape" && this.menu_visible(cx) {
                    this.menu_dismissed = true;
                    cx.notify();
                }
            }))
            // The editor binds Up and Down to cursor moves, which consume the
            // keystroke before any key-down listener runs.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.menu_visible(cx) {
                    this.menu_highlight = this.menu_highlight.saturating_sub(1);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.menu_visible(cx) {
                    let (rows, _, _) = this.menu_rows(cx);
                    this.menu_highlight =
                        (this.menu_highlight + 1).min(rows.len().saturating_sub(1));
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_drop(
                move |paths: &ExternalPaths, window: &mut Window, cx: &mut App| {
                    let paths: Vec<PathBuf> = paths.paths().to_vec();
                    composer.update(cx, |this, cx| {
                        if this.compact || !this.interactive(cx) {
                            return;
                        }
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
            // Match sent-message typography while composing.
            .child(
                Textarea::new(&self.input)
                    .disabled(readonly)
                    .appearance(false)
                    .text_size(px(13.5))
                    .line_height(px(21.)),
            )
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
            .px(px(if self.compact {
                16.
            } else {
                crate::chat::CONTENT_MIN_PADDING
            }))
            .pb_2()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_menu))
            .on_action(cx.listener(|this, _: &ToggleInteractionMode, _, cx| {
                if !this.interactive(cx) {
                    cx.propagate();
                    return;
                }
                this.workspace_store
                    .update(cx, |store, _cx| store.toggle_interaction_mode());
                cx.notify();
            }))
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(crate::chat::CONTENT_MAX_WIDTH))
                    .gap_2()
                    .when_some(approval, |this, request| {
                        this.child(self.render_approval_panel(&request, approval_count, cx))
                    })
                    .when_some(user_input, |this, pending| {
                        this.child(self.render_user_input_panel(&pending, window, cx))
                    })
                    .when_some(fallback_block, |this, block| {
                        this.child(self.render_fallback_panel(&block, cx))
                    })
                    .when_some(fallback_review, |this, review| {
                        this.child(self.render_fallback_review_panel(&review, cx))
                    })
                    .children(self.render_trigger_menu(cx))
                    .children(self.render_queue_strip(cx))
                    .child(v_flex().w_full().child(card).when(
                        self.compact || (readonly && compact),
                        |el| {
                            el.child(
                                h_flex()
                                    .debug_selector(|| "composer-settings-drawer".into())
                                    .mx_2()
                                    .px_1()
                                    .min_w_0()
                                    .gap_1()
                                    .items_center()
                                    .rounded_b(px(12.))
                                    .border_1()
                                    .border_t_0()
                                    .border_color(cx.theme().border)
                                    .bg(cx.theme().muted)
                                    .child(self.render_permission_picker(cx))
                                    .child(self.render_mode_chip(cx))
                                    .child(div().flex_1())
                                    .child(self.render_context_meter(cx)),
                            )
                        },
                    ))
                    .when(!self.compact && !readonly, |el| {
                        el.children(self.render_checkout_row(cx))
                    }),
            )
    }
}

#[cfg(test)]
mod replica_tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext};
    use tcode_protocol::{
        EventEnvelope, HostMessage, ServerEvent, SessionStatus, Topic, encode_line,
    };

    fn attach(
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        async_channel::Sender<String>,
        async_channel::Receiver<String>,
        SessionStatus,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-composer-replica-{}-{}",
            std::process::id(),
            tcode_services::store::now_millis()
        ));
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(root.clone()).unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("replica".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        cx.background_executor
            .spawn(async move {
                pump.pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                    .await;
            })
            .detach();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        store.update(cx, |store, _| {
            store.select_session(status.session_id.clone())
        });
        (store, incoming, outgoing, status)
    }

    fn replace(incoming: &async_channel::Sender<String>, topic: Topic, event: ServerEvent) {
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic,
                    event,
                }))
                .unwrap(),
            )
            .unwrap();
    }

    #[gpui::test]
    fn queue_edit_waits_for_host_removal_and_in_flight_actions_are_disabled(
        cx: &mut TestAppContext,
    ) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        status.queued_messages = vec![tcode_protocol::QueuedMessageStatus {
            id: 7,
            editable: false,
            delivery_key: None,
            text: "Keep this queued until the host accepts the edit".into(),
            fire_at_unix_secs: Some(u64::MAX),
        }];
        let topic = Topic::SessionStatus {
            session_id: status.session_id.clone(),
        };
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        cx.update(|window, cx| _ = window.draw(cx));
        for action in ["queue-drop-7", "queue-steer-7"] {
            let button = cx.debug_bounds(action).expect("queue action visible");
            cx.simulate_click(button.center(), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            assert!(
                composer
                    .read_with(cx, |composer, cx| composer.draft(cx))
                    .is_empty()
            );
        }
        while let Ok(line) = outgoing.try_recv() {
            let message = tcode_protocol::decode_client_line(&line).unwrap();
            assert!(
                !matches!(
                    message.payload,
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::DropQueued { .. }
                            | tcode_protocol::Command::SteerQueued { .. }
                    )
                ),
                "disabled actions must not send queue commands"
            );
        }
        status.queued_messages[0].editable = true;
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let drop_button = cx.debug_bounds("queue-drop-7").unwrap();
        cx.simulate_click(drop_button.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(
            composer
                .read_with(cx, |composer, cx| composer.draft(cx))
                .is_empty(),
            "the pending host drop is still queued"
        );
        assert!(
            std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                tcode_protocol::decode_client_line(&line).unwrap().payload,
                tcode_protocol::ClientPayload::Command(tcode_protocol::Command::DropQueued {
                    id: 7,
                    ..
                })
            )),
            "the enabled drop must reach the host"
        );
        status.queued_messages.clear();
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("queue-drop-7").is_none());
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "Keep this queued until the host accepts the edit"
        );
        cx.update(|window, cx| {
            composer.update(cx, |composer, cx| composer.set_draft("", window, cx))
        });
        status
            .queued_messages
            .push(tcode_protocol::QueuedMessageStatus {
                id: 8,
                editable: true,
                delivery_key: None,
                text: "Queued text".into(),
                fire_at_unix_secs: None,
            });
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let drop_button = cx.debug_bounds("queue-drop-8").unwrap();
        cx.simulate_click(drop_button.center(), Modifiers::default());
        cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
        cx.simulate_input("New draft while the drop is pending");
        cx.run_until_parked();
        status.queued_messages.clear();
        replace(
            &incoming,
            topic,
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "New draft while the drop is pending"
        );
    }

    struct PlanAndComposer {
        plan: Entity<crate::plan_panel::PlanPanel>,
        composer: Entity<Composer>,
    }

    impl Render for PlanAndComposer {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .child(div().flex_1().child(self.plan.clone()))
                .child(self.composer.clone())
        }
    }

    #[gpui::test]
    fn ready_plan_replica_renders_without_a_plan_in_the_history_window(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, mut status) = attach(cx);
        status.draft = false;
        status.interaction_mode = InteractionMode::Plan;
        let session_id = status.session_id.clone();
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        replace(
            &incoming,
            Topic::SessionEvents {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionSnapshot {
                from: 0,
                end: 1,
                total: 1,
                total_turns: 10,
                truncated: false,
                records: vec![tcode_core::session::StoredEvent {
                    ts: Some(1),
                    elided: None,
                    event: agent::AgentEvent::TurnStarted {
                        turn_id: "recent-turn".into(),
                    },
                }],
            },
        );
        replace(
            &incoming,
            Topic::SessionPlan {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                session_id,
                proposed: Some(tcode_protocol::ProposedPlanStatus {
                    item_id: "older-plan".into(),
                    turn: 3,
                    markdown: "# Ready across devices\n\nImplement the shared plan.".into(),
                    ready: true,
                    resolved: false,
                }),
                steps: vec![agent::PlanStep {
                    step: "First shared task".into(),
                    status: agent::PlanStepStatus::Pending,
                }],
            }),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        assert_eq!(
            store.read_with(cx, |store, _| store.with_active_timeline(|timeline| {
                timeline.shown_proposed_plan().is_none()
            })),
            Some(true)
        );
        let (_, cx) = cx.add_window_view(|window, cx| PlanAndComposer {
            plan: cx.new(|cx| crate::plan_panel::PlanPanel::new(store.clone(), cx)),
            composer: cx.new(|cx| Composer::new_with_layout(store.clone(), true, window, cx)),
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("panel-proposed-plan").is_some());
        assert!(cx.debug_bounds("plan-step-0").is_some());
        assert!(
            cx.debug_bounds("implement-main").is_some(),
            "Implement must use the ready replica"
        );
    }
}
