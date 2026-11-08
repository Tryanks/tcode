//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel.

mod components;
pub(crate) mod model;

pub(crate) use components::goal_strip::{
    status_label as goal_status_label, usage_line as goal_usage_line,
};

use components::images::PendingImage;
use components::steer_gesture::SteerGesture;
#[cfg(feature = "voice")]
use components::voice::Voice;
use model::*;

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;

use crate::overlay::{DialogButtons, Notification, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{
    BrowserPaste, Input, InputEvent, InputState, Paste, Textarea, TextareaState,
};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::widgets::spinner::Spinner;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use agent::{
    ApprovalDecision, ApprovalKind, ApprovalOptionKind, ApprovalRequest, ModelSpec,
    OptionDescriptor, ProviderCommandKind, ProviderKind, TokenUsage, UserInputQuestion,
};
use chrono::Local;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardEntry, Context, Entity, EventEmitter,
    ExternalPaths, Focusable as _, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, Role, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_base::PopoverState;
use gpui_base::input::{MoveDown, MoveUp};

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) const CONTEXT: &str = "Composer";

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
    FileRemove(usize),
}
use gpui_base::{ElementExt as _, h_flex, v_flex};

use crate::attachments::attach_error_message;
use crate::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::context_meter;
use crate::palette::fuzzy_score;
use crate::provider_card::profile_glyph;
use crate::settings::provider_label;
use crate::shortcut::format_secondary_shortcut;
use crate::store::{ComposerState, WorkspaceStore};
use crate::workspace_walk::filter_entries;
use tcode_core::attachments::{mime_from_path, text_with_file_paths, validate_attachment};
use tcode_core::provider_models::{FastLevel, FastMode};
use tcode_core::ui::WorkspaceMode;
use tcode_protocol::PathEntry;

/// Context-meter colors, shared with provider usage bars.
use crate::usage::{METER_BLUE, METER_RED};
/// File mentions are potentially unbounded; command and skill feeds are not
/// capped and instead use the trigger menu's scrolling viewport.
const FILE_MENU_ROW_CAP: usize = 50;
/// Matching threads an `@` menu offers above the files.
const THREAD_MENU_ROW_CAP: usize = 5;

/// The desktop send and stop square: as tall as the control row's chips
/// ([`Button::chip`]), which carry no square form.
const SEND_SIZE: f32 = 28.;
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
    /// A message just went into the outbox for the thread: a new turn, one
    /// queued behind the running turn, or a steer.
    Sent,
}

/// How far the draft field may grow before it scrolls. A compact window has to
/// leave room for the keyboard, so it starts taller and stops sooner.
fn auto_grow_rows(compact: bool) -> (usize, usize) {
    if compact { (2, 5) } else { (1, 8) }
}

fn draft_placeholder(compact: bool) -> Cow<'static, str> {
    if compact {
        crate::tr!("mobile.message")
    } else {
        crate::tr!("composer.placeholder")
    }
}

pub struct Composer {
    compact: bool,
    /// How far right of the page inset the column keeps clear, for the
    /// chat's side rail ([`Self::set_column_shift`]).
    column_shift: Pixels,
    /// How far the card's bottom is above the composer's, as last laid out:
    /// what the column keeps under the card, and the inset under that.
    below_card: Rc<Cell<Pixels>>,
    workspace_store: Entity<WorkspaceStore>,
    /// The thread this composer writes to.
    session_id: Option<String>,
    /// The composer state of the render under way, if one is; see
    /// [`Self::composer_state`].
    frame_state: Option<Rc<ComposerState>>,
    input: Entity<TextareaState>,
    /// What the composer's render needs of `input`, kept here so that the
    /// render never reads it: its focus handle, and whether the draft has
    /// text. gpui-base's input notifies its whole state each time the caret
    /// blinks, which would draw again whatever read it.
    input_focus: gpui::FocusHandle,
    draft_has_text: bool,
    /// The view drawing `input`.
    field: Entity<ComposerField>,
    /// Dedicated free-form answer field shown inside an agent question card.
    /// Keeping it separate from the turn composer makes the pending question
    /// and the destination of typed text unambiguous.
    user_input_custom: Entity<TextareaState>,
    /// Unsent text is isolated by persisted thread or project New thread page.
    input_destination: Option<ComposerDestination>,
    input_cache: std::collections::HashMap<ComposerDestination, Entity<TextareaState>>,
    input_subscription: Subscription,
    input_observation: Subscription,
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
    /// A non-blocking request the user closed on this client; its panel stays
    /// hidden until a newer request replaces it. The agent keeps working and
    /// the question text remains in the transcript.
    ui_dismissed_request_id: Option<String>,
    /// Whether the question card shows its answer area. A blocking request
    /// opens it; a non-blocking one arrives closed so it never takes the
    /// composer's place.
    ui_expanded: bool,
    /// Bumped by `/model` so the model-picker popover re-opens (a fresh popover
    /// instance, keyed by this token, starts open).
    model_picker_token: u64,
    /// Whether the control row was last laid out narrower than its controls
    /// fit, which folds them into the "⋯" overflow menu. Only layout knows
    /// the width, so its prepaint callback writes it here.
    control_row_narrow: Rc<Cell<bool>>,
    /// The composer's motion, kept here so it carries on when the window
    /// draws the composer under another parent: the chat moves between the
    /// phone's thread page and a pane as the window turns compact or wide,
    /// and a zoom lifts its pane over the column. Kept for the thread it
    /// shows, its scene's, which its element-keyed motion is drawn under too.
    motion: crate::motion::owned::Owned,
    scene: crate::motion::Epoch,
    /// The inline trigger (`@`/`/`/`$`) active at the cursor, recomputed on every
    /// input change. Drives the trigger menu.
    active_trigger: Option<ComposerTrigger>,
    /// Highlighted row index within the open trigger menu (arrows + hover).
    menu_highlight: usize,
    /// Every row the open trigger menu has listed, by key, for a row a
    /// keystroke takes away to be drawn from as it folds.
    menu_rows_seen: Rc<std::cell::RefCell<std::collections::HashMap<u64, MenuRow>>>,
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
    /// Attached non-image files, under the same rules.
    pending_files: Vec<components::files::PendingFile>,
    next_file_id: u64,
    /// The session id `pending_images` belongs to (reset the strip on switch).
    images_session: Option<String>,
    /// Invalidates image jobs when the owning session changes or a turn sends.
    image_load_generation: u64,
    /// Reserved strip slots for image jobs that have not completed yet.
    pending_image_loads: usize,
    queued_refill: Option<(String, u64, String)>,
    queue_drag: Option<components::queue_strip::QueueDrag>,
    /// Redraws the composer when an unanswered stop has waited long enough to
    /// offer the force terminate, keyed by when that stop went out.
    stop_wait: Option<(u64, gpui::Task<()>)>,
    queue_scroll: gpui::ScrollHandle,
    /// A touch on the compact Queue button that has not lifted yet.
    steer_gesture: Option<SteerGesture>,
    /// Mic button + live dictation session (see `components::voice`).
    #[cfg(feature = "voice")]
    voice: Voice,
    /// Where the keyboard rests in a subagent's thread, which has no field.
    focus_handle: gpui::FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    fn interactive(&self, cx: &App) -> bool {
        !self.subagent(cx)
            && !matches!(self.workspace_store.read(cx).connection_state(),
            tcode_client::ConnectionState::Offline { reason } if reason.is_terminal())
    }

    /// Whether the thread is a native subagent's, which only its lead
    /// writes to: the composer shows its settings, read-only, and no field.
    fn subagent(&self, cx: &App) -> bool {
        self.workspace_store
            .read(cx)
            .native_subagent_readonly_for(self.session_id.as_deref(), cx)
    }

    /// What the composer draws from the store for its thread. Within a
    /// render, the one state built for it: everything the composer draws
    /// reads it, many times over, and building it clones the thread's
    /// branches, commands, options and approvals.
    fn composer_state(&self, cx: &App) -> Rc<ComposerState> {
        self.frame_state.clone().unwrap_or_else(|| {
            Rc::new(
                self.workspace_store
                    .read(cx)
                    .composer_state_for(self.session_id.as_deref(), cx),
            )
        })
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::window_seam::is_mobile(cx) {
            return;
        }
        if self.subagent(cx) {
            self.focus_handle.focus(window, cx);
        } else if self.interactive(cx) {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Whether the keyboard is in the composer: in the field of a
    /// conversation that takes text, which the card's hairline shows, or on
    /// a subagent's row.
    pub(crate) fn has_keyboard(&self, window: &Window, cx: &App) -> bool {
        if self.subagent(cx) {
            self.focus_handle.is_focused(window)
        } else {
            self.input_focus.is_focused(window)
        }
    }

    /// The keyboard follows the composer's shape: out of the field a
    /// subagent's thread does not draw, which would leave it nowhere, and
    /// back into the field from the row when the thread takes text again.
    fn sync_keyboard(&self, subagent: bool, window: &mut Window, cx: &mut Context<Self>) {
        let field = self.input_focus.clone();
        let target = if subagent && field.is_focused(window) {
            self.focus_handle.clone()
        } else if !subagent
            && self.focus_handle.is_focused(window)
            && !crate::window_seam::is_mobile(cx)
        {
            field
        } else {
            return;
        };
        window.defer(cx, move |window, cx| target.focus(window, cx));
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

    /// Write to `session_id`'s thread, or none. What the composer holds for
    /// a thread follows on the next render. Another thread is a new scene:
    /// its controls are drawn at rest, rather than carrying on from where
    /// the composer last drew that thread's, and the composer's motion holds
    /// only the thread it shows.
    pub(crate) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        if self.session_id != session_id {
            self.scene.show(session_id.as_deref());
            self.session_id = session_id;
            cx.notify();
        }
    }

    /// Keep the composer's motion in `scene`, its host's: what the host
    /// shows is what the composer shows.
    pub(crate) fn share_scene(&mut self, scene: &crate::motion::Epoch) {
        scene.show(self.session_id.as_deref());
        self.scene = scene.clone();
        self.motion = crate::motion::owned::Owned::in_scene(scene);
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
        self.input
            .update(cx, |input, cx| input.set_auto_grow(min_rows, max_rows, cx));
        cx.notify();
    }

    /// Keep `shift` clear right of the page inset, so the column centres
    /// where the chat's rows do beside its side rail. Only the column's
    /// place changes: the width it is laid out at stays the same.
    pub(crate) fn set_column_shift(&mut self, shift: Pixels, cx: &mut Context<Self>) {
        if self.column_shift != shift {
            self.column_shift = shift;
            cx.notify();
        }
    }

    /// How far its card's bottom is above the composer's bottom, as its
    /// layout put it this frame, or the last frame it was laid out in: read
    /// only once the composer is laid out and prepainted.
    pub(crate) fn below_card(&self) -> Rc<Cell<Pixels>> {
        self.below_card.clone()
    }

    pub fn new(
        workspace_store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_layout(workspace_store, session_id, false, window, cx)
    }

    pub fn new_with_layout(
        workspace_store: Entity<WorkspaceStore>,
        session_id: Option<String>,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (min_rows, max_rows) = auto_grow_rows(compact);
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .autocorrect(true)
                .input_action(gpui::TextInputAction::Enter)
                .auto_grow(min_rows, max_rows)
                .submit_on_enter(true)
        });
        let model_search = cx.new(|cx| InputState::new(window, cx));
        let user_input_custom = cx.new(|cx| {
            TextareaState::new(window, cx)
                .input_action(gpui::TextInputAction::Enter)
                .rows(1)
                .submit_on_enter(true)
        });

        let input_subscription = cx.subscribe_in(&input, window, Self::on_input_event);
        let input_observation = cx.observe(&input, Self::on_input_notified);
        let input_focus = input.read(cx).focus_handle(cx);
        let draft_has_text = draft_has_text(&input, cx);
        let field = cx.new(|_| ComposerField {
            input: input.clone(),
            glide: Default::default(),
            placeholder: Default::default(),
            disabled: false,
        });
        let subscriptions = vec![
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
            cx.subscribe(&model_search, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        #[cfg(feature = "voice")]
        let (voice, subscriptions) = {
            let mut subscriptions = subscriptions;
            let voice = Voice::new(window, cx, &mut subscriptions);
            (voice, subscriptions)
        };

        let scene = crate::motion::Epoch::default();
        scene.show(session_id.as_deref());
        Self {
            compact,
            column_shift: px(0.),
            below_card: Rc::default(),
            session_id,
            workspace_store,
            frame_state: None,
            input,
            input_focus,
            draft_has_text,
            field,
            user_input_custom,
            input_destination: None,
            input_cache: Default::default(),
            input_subscription,
            input_observation,
            model_search,
            picker_rail: None,
            approval_expanded: true,
            ui_request_id: None,
            ui_question_index: 0,
            ui_selections: std::collections::HashMap::new(),
            ui_dismissed_request_id: None,
            ui_expanded: false,
            model_picker_token: 0,
            control_row_narrow: Rc::default(),
            motion: crate::motion::owned::Owned::in_scene(&scene),
            scene,
            active_trigger: None,
            menu_highlight: 0,
            menu_rows_seen: Rc::default(),
            menu_last_key: None,
            menu_dismissed: false,
            workspace: None,
            workspace_loading: false,
            pending_images: Vec::new(),
            pending_files: Vec::new(),
            next_file_id: 0,
            images_session: None,
            image_load_generation: 0,
            pending_image_loads: 0,
            queued_refill: None,
            queue_drag: None,
            stop_wait: None,
            queue_scroll: gpui::ScrollHandle::new(),
            steer_gesture: None,
            #[cfg(feature = "voice")]
            voice,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn on_input_event(
        &mut self,
        input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter {
                shift: false,
                secondary,
            } => {
                if self.menu_visible(cx) {
                    self.accept_menu(self.menu_highlight, window, cx);
                } else {
                    self.submit(input, *secondary, window, cx);
                }
            }
            InputEvent::Change => {
                self.queued_refill = None;
                // An edit that did not come from the transcript writer
                // ends dictation (see `components::voice`).
                #[cfg(feature = "voice")]
                self.stop_dictation_on_user_edit(cx);
                self.recompute_trigger(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// The draft gaining its first character or losing its last, which
    /// enables or disables sending, draws the composer again; anything else
    /// the input notifies (its caret's blink) leaves it alone.
    fn on_input_notified(&mut self, input: Entity<TextareaState>, cx: &mut Context<Self>) {
        let has_text = draft_has_text(&input, cx);
        if self.draft_has_text != has_text {
            self.draft_has_text = has_text;
            cx.notify();
        }
    }

    fn sync_text_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let destination = self
            .workspace_store
            .read(cx)
            .with_composer_destination_for(self.session_id.as_deref(), composer_destination, cx)
            .flatten();
        if self.input_destination == destination {
            return;
        }
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        self.input.update(cx, |input, cx| {
            crate::widgets::input::commit_marked_text(input, window, cx);
        });
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        if let Some(outgoing) = self.input_destination.take() {
            self.input_cache.insert(outgoing, self.input.clone());
        }
        let (min_rows, max_rows) = auto_grow_rows(self.compact);
        self.input = destination
            .as_ref()
            .and_then(|key| self.input_cache.remove(key))
            .unwrap_or_else(|| {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .autocorrect(true)
                        .input_action(gpui::TextInputAction::Enter)
                        .auto_grow(min_rows, max_rows)
                        .submit_on_enter(true)
                })
            });
        self.input_destination = destination;
        self.input_subscription = cx.subscribe_in(&self.input, window, Self::on_input_event);
        self.input_observation = cx.observe(&self.input, Self::on_input_notified);
        self.input_focus = self.input.read(cx).focus_handle(cx);
        self.draft_has_text = draft_has_text(&self.input, cx);
        self.input.update(cx, |state, cx| {
            state.set_auto_grow(min_rows, max_rows, cx);
            if focused {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    /// The prompt a rewind this client asked for gave back goes into the
    /// ordinary composer to edit or resend: first, with what the user had
    /// typed after it, never in its place; its images join the strip.
    fn sync_native_rewind_prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefill = self.workspace_store.update(cx, |store, cx| {
            store.take_native_rewind_prefill_for(self.session_id.as_deref(), cx)
        });
        let Some(prefill) = prefill else {
            return;
        };
        self.sync_images_session(cx);
        for path in prefill.attachments {
            let path = PathBuf::from(path);
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.pending_images
                .push(components::images::PendingImage { path, name });
        }
        let typed = self.input.read(cx).value().trim().to_owned();
        let text = if typed.is_empty() {
            prefill.text
        } else {
            format!("{}\n\n{typed}", prefill.text.trim_end())
        };
        self.set_input_text(text, window, cx);
    }

    /// Text another surface put here goes after what the user typed, which it never replaces.
    fn sync_composer_append(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.workspace_store.update(cx, |store, cx| {
            store.take_composer_append_for(self.session_id.as_deref(), cx)
        }) else {
            return;
        };
        let typed = self.input.read(cx).value().trim_end().to_owned();
        let text = if typed.is_empty() {
            text
        } else {
            format!("{typed}\n\n{text}")
        };
        self.set_input_text(text, window, cx);
    }

    /// Replace the composer text with `text`, caret at the end.
    /// Software-keyboard devices wait for an explicit tap to focus.
    fn set_input_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
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
            ComposerMenu::RemoveTerminalContext(id) => {
                self.workspace_store.update(cx, |store, _cx| {
                    store.remove_terminal_context_for(
                        self.session_id.clone().unwrap_or_default(),
                        id,
                    )
                })
            }
            ComposerMenu::RemoveReviewComment(index) => {
                self.workspace_store.update(cx, |store, _cx| {
                    store.remove_review_comment_for(
                        self.session_id.clone().unwrap_or_default(),
                        index,
                    )
                })
            }
            ComposerMenu::QueueSteer(id) => self.workspace_store.update(cx, |store, _cx| {
                store.steer_queued_for(self.session_id.clone().unwrap_or_default(), id)
            }),
            ComposerMenu::QueueEdit { id, text } => {
                self.drop_queued_and_refill(id, text, window, cx)
            }
            ComposerMenu::QueueDrop(id) => self.workspace_store.update(cx, |store, _cx| {
                store.drop_queued_for(self.session_id.clone().unwrap_or_default(), id)
            }),
            ComposerMenu::ImageOpen(index) => self.open_image_preview(index, window, cx),
            ComposerMenu::ImageRemove(index) => self.remove_image(index, cx),
            ComposerMenu::FileRemove(index) => self.remove_file(index, cx),
        }
    }

    fn drop_queued_and_refill(
        &mut self,
        id: u64,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.interactive(cx)
            || !self.composer_state(cx).queue.as_ref().is_some_and(|queue| {
                queue
                    .messages
                    .iter()
                    .any(|message| message.id == id && message.editable)
            })
        {
            return;
        }
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        self.queued_refill = Some((session_id, id, text));
        self.workspace_store.update(cx, |store, _cx| {
            store.drop_queued_for(self.session_id.clone().unwrap_or_default(), id)
        });
        cx.notify();
    }

    fn sync_queued_refill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session_id, id, _)) = &self.queued_refill else {
            return;
        };
        let store = self.workspace_store.read(cx);
        if self.session_id.clone().as_ref() != Some(session_id) {
            self.queued_refill = None;
            return;
        }
        let Some(queue) = store
            .session_status_for(self.session_id.as_deref(), cx)
            .map(|status| &status.queued_messages)
        else {
            return;
        };
        if let Some(message) = queue.iter().find(|message| message.id == *id) {
            if !message.editable {
                self.queued_refill = None;
            }
            return;
        }
        let (_, _, text) = self.queued_refill.take().unwrap();
        let selection = 0..text.len();
        self.input.update(cx, |state, cx| {
            state.replace_all(text, window, cx);
            state.set_selected_range(selection, cx);
            if !crate::window_seam::is_mobile(cx) {
                state.focus(window, cx);
            }
        });
        self.recompute_trigger(cx);
    }

    fn has_sendable_content(&self, cx: &App) -> bool {
        !self.input.read(cx).value().trim().is_empty()
            || !self.pending_images.is_empty()
            || !self.pending_files.is_empty()
            || self
                .workspace_store
                .read(cx)
                .session_status_for(self.session_id.as_deref(), cx)
                .is_some_and(|status| !status.terminal_contexts.is_empty())
            || !self
                .workspace_store
                .read(cx)
                .review_comments_for(self.session_id.as_deref(), cx)
                .is_empty()
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
        if self.subagent(cx) {
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
        if self.submit_after_dictation(steer, cx) {
            return;
        }
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
        let composer_state = self.composer_state(cx);
        let compact_offered = compact_offered(&composer_state);
        let goal_offered = goal_offered(&composer_state);
        let current_goal = composer_state.goal.clone();
        let terminal_contexts = composer_state.terminal_contexts.clone();
        if !self.has_sendable_content(cx) {
            return;
        }
        if !composer_state.has_active_session {
            window.push_notification(Notification::info(crate::tr!("composer.no_session")), cx);
            return;
        }
        if self.files_unsettled() {
            window.push_notification(Notification::info(crate::tr!("attach.wait_for_files")), cx);
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
            input.update(cx, |state, cx| state.replace_all("", window, cx));
            let message = text_with_file_paths(&message, &self.file_paths());
            self.pending_images.clear();
            self.pending_files.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
            self.workspace_store.update(cx, |store, _cx| {
                store.schedule_turn_for(
                    self.session_id.clone().unwrap_or_default(),
                    message,
                    attachment_paths,
                    fire_at_unix_secs,
                );
            });
            cx.notify();
            return;
        }
        // Local mode and picker commands are consumed without sending a turn.
        if terminal_contexts.is_empty()
            && let Some(command) = slash_command(&text, compact_offered, goal_offered)
        {
            if let SlashIntent::Goal(GoalIntent::Edit) = command {
                match current_goal {
                    Some(goal) => self.edit_goal(&goal.objective, window, cx),
                    None => window.push_notification(
                        Notification::info(crate::tr!("composer.goal_usage")),
                        cx,
                    ),
                }
                return;
            }
            input.update(cx, |state, cx| state.replace_all("", window, cx));
            match command {
                SlashIntent::Model => {
                    self.model_picker_token = self.model_picker_token.wrapping_add(1);
                }
                SlashIntent::Compact => {
                    self.workspace_store.update(cx, |store, _cx| {
                        store.compact_context_for(self.session_id.clone().unwrap_or_default())
                    });
                }
                SlashIntent::Goal(intent) => {
                    self.workspace_store.update(cx, |store, _cx| match intent {
                        GoalIntent::Set(objective) => store.set_goal_for(
                            self.session_id.clone().unwrap_or_default(),
                            Some(objective),
                            Some(agent::GoalStatus::Active),
                        ),
                        GoalIntent::Pause => store.set_goal_for(
                            self.session_id.clone().unwrap_or_default(),
                            None,
                            Some(agent::GoalStatus::Paused),
                        ),
                        GoalIntent::Resume => store.set_goal_for(
                            self.session_id.clone().unwrap_or_default(),
                            None,
                            Some(agent::GoalStatus::Active),
                        ),
                        GoalIntent::Clear => {
                            store.clear_goal_for(self.session_id.clone().unwrap_or_default())
                        }
                        GoalIntent::Edit => {}
                    })
                }
            }
            cx.notify();
            return;
        }
        // While the agent waits on its plan, words sent from here answer it:
        // the agent keeps planning from them.
        if let Some(plan) = composer_state.pending_plan.clone()
            && !text.is_empty()
        {
            if self
                .workspace_store
                .read(cx)
                .approval_delivery_pending_for(self.session_id.as_deref(), &plan.id)
            {
                return;
            }
            input.update(cx, |state, cx| state.replace_all("", window, cx));
            let text = text_with_file_paths(&text, &self.file_paths());
            self.pending_files.clear();
            self.respond(plan.id, ApprovalDecision::Feedback(text), cx);
            cx.notify();
            return;
        }
        let sent_text = text_with_file_paths(&text, &self.file_paths());
        let attachment_paths = self
            .pending_images
            .iter()
            .map(|image| image.path.clone())
            .collect::<Vec<_>>();
        if let Some((from, to)) = self.composer_state(cx).relay_confirmation.clone() {
            let composer = cx.entity();
            let input = input.clone();
            window.open_alert_dialog(cx, move |alert, _, _| {
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
        self.finish_submit(input, sent_text, attachment_paths, steer, false, window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_submit(
        &mut self,
        input: &Entity<TextareaState>,
        sent_text: String,
        attachment_paths: Vec<PathBuf>,
        steer: bool,
        relay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(feature = "voice")]
        self.abort_dictation(cx);
        input.update(cx, |state, cx| state.replace_all("", window, cx));
        self.pending_images.clear();
        self.pending_files.clear();
        self.image_load_generation = self.image_load_generation.wrapping_add(1);
        self.pending_image_loads = 0;
        self.workspace_store.update(cx, |store, _cx| {
            if relay {
                store.confirm_relay_and_send_for(
                    self.session_id.clone().unwrap_or_default(),
                    sent_text,
                    attachment_paths,
                );
            } else if steer {
                store.steer_for(
                    self.session_id.clone().unwrap_or_default(),
                    sent_text,
                    attachment_paths,
                );
            } else {
                store.send_turn_for(
                    self.session_id.clone().unwrap_or_default(),
                    sent_text,
                    attachment_paths,
                );
            }
        });
        cx.emit(ComposerEvent::Sent);
        cx.notify();
    }

    /// Compact control that switches between Send, Queue and Stop: one
    /// cross-fades into the next, and the caption naming what a press does
    /// folds in under it while a turn runs and there is text.
    fn render_compact_primary_action(
        &self,
        turn_running: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let interactive = self.interactive(cx);
        let has_text = interactive && self.draft_has_text;
        let composer_state = self.composer_state(cx);
        let stopping = turn_running && !has_text;
        let stop_pending = composer_state.stopping;
        let steerable = turn_running && has_text && composer_state.steering_supported;
        let armed = self.steer_gesture.as_ref().is_some_and(SteerGesture::armed);
        let mode = if composer_state.preparing_worktree {
            CompactAction::Preparing
        } else if stopping {
            CompactAction::Stop
        } else {
            CompactAction::Arrow
        };
        let touch = px(crate::material::TOUCH_TARGET);
        let buttons = self.button_motion();
        let button = self.motion.cross_fade(
            self.motion_key("compact-action"),
            mode,
            CHANGE,
            window,
            cx,
            move |mode, _, cx| {
                let (label, id): (_, &'static str) = match mode {
                    CompactAction::Preparing => {
                        return div()
                            .size(touch)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Spinner::new().color(cx.theme().primary))
                            .into_any_element();
                    }
                    CompactAction::Stop => (crate::tr!("composer.stop"), "stop-turn"),
                    CompactAction::Arrow if turn_running => {
                        (crate::tr!("mobile.queue"), "steer-turn")
                    }
                    CompactAction::Arrow => (crate::tr!("composer.send"), "send-message"),
                };
                let stop = *mode == CompactAction::Stop;
                Button::new(id)
                    .motion(&buttons)
                    .debug_selector(move || id.into())
                    .with_size(touch)
                    .rounded(touch / 2.)
                    .aria_label(label)
                    .map(|button| {
                        if stop {
                            // A filled stop square; the icon set's square is an
                            // outline. A child is not an icon, so the button's
                            // width is set too.
                            button
                                .danger()
                                .w(touch)
                                .disabled(!interactive || stop_pending)
                                .child(div().size(px(13.)).rounded(px(2.)).bg(cx.theme().danger))
                        } else {
                            button.primary().disabled(!has_text).icon(IconName::ArrowUp)
                        }
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if stop {
                            if this.interactive(cx) {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.interrupt_for(this.session_id.clone().unwrap_or_default())
                                });
                            }
                            return;
                        }
                        let input = this.input.clone();
                        this.submit(&input, false, window, cx);
                    }))
                    .into_any_element()
            },
        );
        let button = div().relative().flex_none().child(button).when(
            mode != CompactAction::Preparing && (steerable || self.steer_gesture.is_some()),
            |el| el.child(self.render_steer_gesture(cx)),
        );
        let caption = self
            .motion
            .shown(
                self.motion_key("compact-action-caption"),
                (mode == CompactAction::Arrow && turn_running && has_text).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let color = self.motion.transition(
                    self.motion_key("compact-action-caption-color"),
                    if armed {
                        cx.theme().primary
                    } else {
                        cx.theme().muted_foreground
                    },
                    GLIDE,
                    window,
                    cx,
                );
                let text = self.motion.cross_fade_part(
                    self.motion_key("compact-action-caption-text"),
                    armed,
                    CHANGE,
                    window,
                    cx,
                    |armed, _, _| {
                        div()
                            .whitespace_nowrap()
                            .child(crate::tr!(if *armed {
                                "mobile.steer"
                            } else {
                                "mobile.queue"
                            }))
                            .into_any_element()
                    },
                );
                shown.fold(
                    div()
                        .flex()
                        .justify_center()
                        .pt(cx.theme().tokens.spacing.xxs)
                        .text_secondary(false, cx)
                        .text_color(color)
                        .child(text),
                )
            });
        v_flex()
            .flex_none()
            .items_center()
            .child(button)
            .children(caption)
            .into_any_element()
    }

    fn render_stop(&self, cx: &mut Context<Self>) -> Button {
        let size = px(SEND_SIZE);
        // A filled stop square; the icon set's square is an outline. A child is
        // not an icon, so the button's width is set too.
        Button::new("stop-turn")
            .motion(&self.button_motion())
            .debug_selector(|| "stop-turn".into())
            .danger()
            .with_size(size)
            .w(size)
            .aria_label(crate::tr!("composer.stop"))
            .disabled(!self.interactive(cx) || self.composer_state(cx).stopping)
            .child(div().size(px(11.)).rounded(px(2.)).bg(cx.theme().danger))
            .on_click(cx.listener(|this, _, _, cx| {
                if !this.interactive(cx) {
                    return;
                }
                this.workspace_store.update(cx, |store, _cx| {
                    store.interrupt_for(this.session_id.clone().unwrap_or_default())
                });
            }))
    }

    /// The desktop send control. A running turn widens it into a spinner,
    /// Steer (for providers that steer) and Stop, worktree preparation into
    /// a spinner and its caption: each part folds in or away along the row
    /// and the parts that stay move over, so the chips beside it give way
    /// rather than jump.
    fn render_send_or_stop(
        &self,
        turn_running: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.compact {
            return self.render_compact_primary_action(turn_running, window, cx);
        }
        let composer_state = self.composer_state(cx);
        let has_text = self.interactive(cx) && self.draft_has_text;
        let parts: &[DesktopAction] = if turn_running {
            // Providers with native mid-turn steering keep a send button
            // active beside Stop while a turn runs.
            if composer_state.steering_supported {
                &[
                    DesktopAction::Spinner,
                    DesktopAction::Steer,
                    DesktopAction::Stop,
                ]
            } else {
                &[DesktopAction::Spinner, DesktopAction::Stop]
            }
        } else if composer_state.preparing_worktree {
            // First-send worktree preparation holds the send control.
            &[DesktopAction::Spinner, DesktopAction::Preparing]
        } else {
            &[DesktopAction::Send]
        };
        crate::motion::keyed::keyed_owned(
            &self.motion,
            self.motion_key("send-or-stop"),
            crate::motion::keyed::Fold::Across(px(0.)),
            parts
                .iter()
                .map(|part| (gpui::SharedString::from(part.key()), *part)),
            ROWS,
            window,
            cx,
            |_, part, _, cx| {
                let part = match part {
                    DesktopAction::Spinner => {
                        Spinner::new().color(cx.theme().primary).into_any_element()
                    }
                    DesktopAction::Steer => {
                        let queue_hint = crate::tr!(
                            "composer.queue_hint",
                            shortcut = format_secondary_shortcut("enter", cx)
                        )
                        .into_owned();
                        Button::new("steer-turn")
                            .motion(&self.button_motion())
                            .debug_selector(|| "steer-turn".into())
                            .primary()
                            .with_size(px(SEND_SIZE))
                            .icon(IconName::ArrowUp)
                            .aria_label(crate::tr!("composer.steer_tooltip"))
                            .tooltip(queue_hint)
                            .disabled(!has_text)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let input = this.input.clone();
                                this.submit(&input, false, window, cx);
                            }))
                            .into_any_element()
                    }
                    DesktopAction::Stop => self.render_stop(cx).into_any_element(),
                    DesktopAction::Preparing => div()
                        .whitespace_nowrap()
                        .text_secondary(false, cx)
                        .child(crate::tr!("composer.preparing_worktree"))
                        .into_any_element(),
                    DesktopAction::Send => Button::new("send-message")
                        .motion(&self.button_motion())
                        .debug_selector(|| "send-message".into())
                        .primary()
                        .with_size(px(SEND_SIZE))
                        .icon(IconName::ArrowUp)
                        .aria_label(crate::tr!("composer.send"))
                        .disabled(!has_text)
                        .on_click(cx.listener(|this, _, window, cx| {
                            let input = this.input.clone();
                            this.submit(&input, false, window, cx);
                        }))
                        .into_any_element(),
                };
                // Each part carries the gap before it, so the gap goes with it.
                h_flex()
                    .pl_2()
                    .items_center()
                    .child(part)
                    .into_any_element()
            },
        )
        .render(h_flex().items_center())
    }
}

/// What the compact send control shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactAction {
    Preparing,
    /// Send, or Queue while a turn runs.
    Arrow,
    Stop,
}

/// One part of the desktop send control.
#[derive(Clone, Copy)]
enum DesktopAction {
    Spinner,
    Steer,
    Stop,
    Preparing,
    Send,
}

impl DesktopAction {
    fn key(self) -> &'static str {
        match self {
            Self::Spinner => "spinner",
            Self::Steer => "steer",
            Self::Stop => "stop",
            Self::Preparing => "preparing",
            Self::Send => "send",
        }
    }
}

/// One row of the card's chips: what the draft carries besides its text.
#[derive(Clone)]
enum ChipRow {
    Terminal(Vec<tcode_protocol::TerminalContextStatus>),
    Review(Vec<tcode_core::session::ReviewComment>),
    References(String, Vec<tcode_protocol::ThreadReferenceStatus>),
}

impl Composer {
    /// The id of a part's motion.
    fn motion_id(&self, part: impl std::fmt::Display) -> gpui::ElementId {
        gpui::ElementId::Name(self.motion_key(part))
    }

    /// [`Self::motion_id`] as a key of the composer's own motion.
    fn motion_key(&self, part: impl std::fmt::Display) -> gpui::SharedString {
        format!("composer:{part}").into()
    }

    /// The motion of the composer's buttons ([`Button::motion`]).
    fn button_motion(&self) -> crate::motion::owned::Owned {
        self.motion.scope(self.motion_key("buttons"))
    }

    /// The chip rows above the field: terminal contexts, review comments and
    /// thread references. A chip that goes narrows away and the ones after
    /// it move over; a row folds in with its first chip and away with its
    /// last. Each chip carries the gap after and under it, and each row the
    /// space under it, so a gap goes with what it separates.
    fn render_chip_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let composer_state = self.composer_state(cx);
        let terminal = &composer_state.terminal_contexts;
        let review = store.review_comments_for(self.session_id.as_deref(), cx);
        let mut rows = Vec::new();
        if !terminal.is_empty() {
            rows.push((
                gpui::SharedString::from("terminal"),
                ChipRow::Terminal(terminal.clone()),
            ));
        }
        if !review.is_empty() {
            rows.push((gpui::SharedString::from("review"), ChipRow::Review(review)));
        }
        if let Some((session_id, references)) = self.thread_references(cx) {
            rows.push((
                gpui::SharedString::from("references"),
                ChipRow::References(session_id, references),
            ));
        }
        let chip = |button: AnyElement| div().pr_1().pb_1().child(button).into_any_element();
        crate::motion::keyed::keyed_owned(
            &self.motion,
            self.motion_key("chip-rows"),
            crate::motion::keyed::Fold::Down,
            rows,
            ROWS,
            window,
            cx,
            |key, row, window, cx| {
                let id = self.motion_key(format_args!("chips-{key}"));
                let chips = match row {
                    ChipRow::Terminal(contexts) => crate::motion::keyed::keyed_owned(
                        &self.motion,
                        id.clone(),
                        crate::motion::keyed::Fold::Across(px(0.)),
                        contexts
                            .iter()
                            .map(|context| (context.id.to_string().into(), context.clone())),
                        ROWS,
                        window,
                        cx,
                        |_, context, _, cx| chip(self.terminal_context_chip(context, cx)),
                    ),
                    ChipRow::Review(comments) => {
                        // The same comment twice is two chips, told apart by
                        // how many came before.
                        let mut seen = std::collections::HashMap::<String, usize>::new();
                        let items = comments.iter().enumerate().map(|(index, comment)| {
                            let key = format!(
                                "{}:{:?}:{}-{}:{}",
                                comment.file,
                                comment.side,
                                comment.line_start,
                                comment.line_end,
                                comment.text
                            );
                            let nth = seen.entry(key.clone()).or_default();
                            *nth += 1;
                            (format!("{key}#{nth}").into(), (index, comment.clone()))
                        });
                        crate::motion::keyed::keyed_owned(
                            &self.motion,
                            id.clone(),
                            crate::motion::keyed::Fold::Across(px(0.)),
                            items.collect::<Vec<_>>(),
                            ROWS,
                            window,
                            cx,
                            |key, (index, comment), _, cx| {
                                chip(self.review_comment_chip(key, *index, comment, cx))
                            },
                        )
                    }
                    ChipRow::References(session_id, references) => {
                        crate::motion::keyed::keyed_owned(
                            &self.motion,
                            id.clone(),
                            crate::motion::keyed::Fold::Across(px(0.)),
                            references.iter().map(|reference| {
                                (reference.thread_id.clone().into(), reference.clone())
                            }),
                            ROWS,
                            window,
                            cx,
                            |_, reference, _, cx| {
                                chip(
                                    self.reference_chip(session_id, reference, cx)
                                        .into_any_element(),
                                )
                            },
                        )
                    }
                };
                chips.render(h_flex().w_full().flex_wrap().pb_0p5())
            },
        )
        .render(v_flex().w_full())
    }

    fn terminal_context_chip(
        &self,
        context: &tcode_protocol::TerminalContextStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = context.id;
        let range = if context.line_start == context.line_end {
            format!("L{}", context.line_start)
        } else {
            format!("L{}-L{}", context.line_start, context.line_end)
        };
        let text = context.text.clone();
        Button::new(("terminal-context-chip", id))
            .motion(&self.button_motion())
            .ghost()
            .small()
            .chip()
            .text_meta_mono(self.compact, cx)
            .label(format!("{} · {}  ×", context.terminal_label, range))
            .tooltip(context.text.clone())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.workspace_store.update(cx, |store, _cx| {
                    store.remove_terminal_context_for(
                        this.session_id.clone().unwrap_or_default(),
                        id,
                    )
                });
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
            .into_any_element()
    }

    /// The chip for the review comment at `index`; `key` names it for as
    /// long as it is drafted, wherever it moves in the list.
    fn review_comment_chip(
        &self,
        key: &gpui::SharedString,
        index: usize,
        comment: &tcode_core::session::ReviewComment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let range = if comment.line_start == comment.line_end {
            format!("L{}", comment.line_start)
        } else {
            format!("L{}-L{}", comment.line_start, comment.line_end)
        };
        let text = comment.text.clone();
        Button::new(gpui::ElementId::NamedChild(
            std::sync::Arc::new("review-comment-chip".into()),
            key.clone(),
        ))
        .motion(&self.button_motion())
        .ghost()
        .small()
        .chip()
        .text_meta_mono(self.compact, cx)
        .label(format!("{} {}  ×", comment.file, range))
        .tooltip(comment.text.clone())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.workspace_store.update(cx, |store, _cx| {
                store.remove_review_comment_for(this.session_id.clone().unwrap_or_default(), index)
            });
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
        .into_any_element()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_state = Some(Rc::new(
            self.workspace_store
                .read(cx)
                .composer_state_for(self.session_id.as_deref(), cx),
        ));
        let composer = self.render_composer(window, cx);
        self.frame_state = None;
        composer
    }
}

impl Composer {
    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.sync_user_input_state(window, cx);
        self.sync_images_session(cx);
        self.sync_text_destination(window, cx);
        self.sync_queued_refill(window, cx);
        // Taking the text writes the store, which draws every view that
        // reads it again; most frames have none to take.
        if self
            .workspace_store
            .read(cx)
            .composer_text_waiting_for(self.session_id.as_deref(), cx)
        {
            self.sync_native_rewind_prefill(window, cx);
            self.sync_composer_append(window, cx);
        }
        let composer_state = self.composer_state(cx);
        let subagent = composer_state.conversation_read_only;
        self.sync_keyboard(subagent, window, cx);
        let turn_running = composer_state.turn_running;
        let approval = composer_state.pending_approval.clone();
        let approval_count = composer_state.pending_approval_count;

        let border = cx.theme().border;
        let divider = move || div().w_px().h(px(16.)).bg(border);

        let compact = self.compact || self.control_row_narrow.get();

        let control_row_base = h_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .gap_1()
            .items_center();

        // A subagent's thread shows what it runs with and takes nothing: its
        // settings stay, read-only, and the field, send and mic go, so the
        // timeline keeps their height.
        #[cfg(feature = "voice")]
        let mic = self.render_mic_button(cx);
        #[cfg(not(feature = "voice"))]
        let mic: Option<AnyElement> = None;
        let mic = self
            .motion
            .shown(
                self.motion_key("mic"),
                (!subagent && mic.is_some()).then_some(()),
                SHOW,
                window,
                cx,
            )
            .and_then(|shown| mic.map(|mic| shown.fold_across(mic)));

        // A platform picker is the only way in for phones and browsers, and
        // desktops have the system's file picker beside paste and drop; the
        // subagent and offline states have nothing to add to.
        let attach_shown = !subagent
            && self.interactive(cx)
            && (cfg!(feature = "native-dialogs")
                || cx
                    .try_global::<crate::remote::ClientAttachment>()
                    .is_some_and(|client| {
                        client.host().supports_image_picker()
                            || client.host().supports_file_picker()
                    }));
        // It narrows away when the host goes offline, and widens back.
        let attach_menu = self.motion.shown(
            self.motion_key("attach-menu"),
            attach_shown.then_some(()),
            SHOW,
            window,
            cx,
        );
        // The model picker and the traits chip widen in once the thread has
        // a model (and the model traits), and narrow away as they go, drawn
        // with the name they last had.
        let leaving_chip = |id: &'static str, label: &gpui::SharedString, cx: &App| {
            Button::new(id)
                .motion(&self.button_motion())
                .ghost()
                .chip()
                .touch(self.compact)
                .label(label.clone())
                .text_color(cx.theme().muted_foreground)
                .into_any_element()
        };
        let mut model_picker = self.render_model_picker(cx);
        let model_label = model_picker.as_ref().map(|(label, _)| label.clone());
        let model_picker = self
            .motion
            .shown(
                self.motion_key("model-picker"),
                model_label,
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let picker = match model_picker.take() {
                    Some((_, picker)) if !shown.leaving() => picker,
                    _ => leaving_chip("model-picker-leaving", &shown.value, cx),
                };
                // The name truncates to make room, down to one
                // control's width so the picker can still be opened.
                shown.fold_across_min(px(SEND_SIZE), div().min_w(px(SEND_SIZE)).child(picker))
            });
        let mut traits_picker = self.render_traits_picker(cx);
        // The chips give way to the send/stop control: Stop must stay
        // reachable however long the chips' labels run.
        let leading = || {
            h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .gap_1()
                .items_center()
        };
        let primary = self
            .motion
            .shown(
                self.motion_key("primary"),
                (!subagent).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold_across(div().flex_none().child(self.render_send_or_stop(
                    turn_running,
                    window,
                    cx,
                )))
            });
        let mut reflow_scope = None;
        let control_row = if self.compact {
            // The phone keeps the permission and the context meter in the
            // drawer under the card; a subagent's thread has no card, so they
            // join the row.
            let beside = |part: &'static str,
                          build: &mut dyn FnMut(&mut Context<Self>) -> AnyElement,
                          window: &mut Window,
                          cx: &mut Context<Self>| {
                self.motion
                    .shown(
                        self.motion_key(format_args!("control-row-{part}")),
                        subagent.then_some(()),
                        SHOW,
                        window,
                        cx,
                    )
                    .map(|shown| shown.fold_across_gap(px(4.), build(cx)))
            };
            let meter = beside("meter", &mut |cx| self.render_context_meter(cx), window, cx);
            let permission = beside(
                "permission",
                &mut |cx| self.render_permission_picker(cx),
                window,
                cx,
            );
            control_row_base
                .child(
                    leading()
                        .children(attach_menu.map(|shown| {
                            shown.fold_across_gap(
                                px(4.),
                                div().flex_none().child(self.render_attach_menu(cx)),
                            )
                        }))
                        .children(model_picker)
                        .children(
                            self.motion
                                .shown(
                                    self.motion_key("control-row-traits"),
                                    traits_picker.as_ref().map(|(label, _)| label.clone()),
                                    SHOW,
                                    window,
                                    cx,
                                )
                                .map(|shown| {
                                    let picker = match traits_picker.take() {
                                        Some((_, picker)) if !shown.leaving() => picker,
                                        _ => leaving_chip("traits-chip-leaving", &shown.value, cx),
                                    };
                                    shown.fold_across_gap(px(4.), picker)
                                }),
                        )
                        .children(meter)
                        .children(permission)
                        .child(div().flex_1())
                        .children(mic),
                )
                .children(primary)
        } else {
            // A row that folds narrows the traits, permission and divider
            // away and the overflow in, and glides the context meter and
            // the mic to their places on the frame that folds it. Each part
            // after the model carries the gap before it, so a gap goes with
            // its part. A subagent's row has nothing to fold into: its four
            // controls stay, the model's name giving way first.
            let folded = compact && !subagent;
            let reflow = self.motion.reflow_in(
                self.motion_key("control-row"),
                u64::from(folded),
                window,
                cx,
            );
            let gapped = |part: AnyElement| div().flex_none().pl_1().child(part);
            let fold = |part: &'static str,
                        shown: bool,
                        build: &mut dyn FnMut(&mut Context<Self>) -> AnyElement,
                        window: &mut Window,
                        cx: &mut Context<Self>| {
                self.motion
                    .shown(
                        self.motion_key(format_args!("control-row-{part}")),
                        shown.then_some(()),
                        SHOW,
                        window,
                        cx,
                    )
                    .map(|shown| shown.fold_across(gapped(build(cx))))
            };
            let traits = self
                .motion
                .shown(
                    self.motion_key("control-row-traits"),
                    traits_picker
                        .as_ref()
                        .filter(|_| !folded)
                        .map(|(label, _)| label.clone()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| {
                    let picker = match traits_picker.take() {
                        Some((_, picker)) if !shown.leaving() => picker,
                        _ => leaving_chip("traits-chip-leaving", &shown.value, cx),
                    };
                    shown.fold_across(gapped(picker))
                });
            let divider = fold(
                "divider",
                !folded && !subagent,
                &mut |_| divider().into_any_element(),
                window,
                cx,
            );
            let permission = fold(
                "permission",
                !folded,
                &mut |cx| self.render_permission_picker(cx),
                window,
                cx,
            );
            let overflow = fold(
                "overflow",
                folded,
                &mut |cx| self.render_overflow_menu(cx),
                window,
                cx,
            );
            let meter = reflow.part("meter", gapped(self.render_context_meter(cx)));
            let (meter_inside, meter_end) = if folded {
                (None, Some(meter))
            } else {
                (Some(meter), None)
            };
            let row = control_row_base
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .items_center()
                        .children(attach_menu.map(|shown| {
                            shown.fold_across(
                                div().flex_none().pr_1().child(self.render_attach_menu(cx)),
                            )
                        }))
                        .children(model_picker)
                        .children(traits)
                        .children(divider)
                        .children(meter_inside)
                        .children(permission)
                        .children(overflow)
                        .child(div().flex_1())
                        .children(mic.map(|mic| reflow.part("mic", gapped(mic))))
                        .children(meter_end),
                )
                .children(primary);
            reflow_scope = Some(reflow);
            row
        };

        // The row's width is known only once it is laid out, after this
        // render; one that folds or unfolds the controls renders again.
        let narrow_cell = self.control_row_narrow.clone();
        let composer = cx.entity_id();
        let control_row = control_row.on_prepaint(move |bounds, window, _| {
            let narrow = f32::from(bounds.size.width) < CONTROL_ROW_COMPACT_BELOW;
            if narrow_cell.replace(narrow) != narrow {
                window.on_next_frame(move |_, cx| cx.notify(composer));
            }
        });
        let control_row = match reflow_scope {
            Some(reflow) => reflow.render(control_row),
            None => control_row.into_any_element(),
        };

        let desired_placeholder = if self.compact && !self.interactive(cx) {
            crate::tr!("mobile.offline_message")
        } else if composer_state.pending_plan.is_some() {
            crate::tr!("composer.plan_feedback_placeholder")
        } else {
            draft_placeholder(self.compact)
        };

        let user_input = self.pending_user_input(cx);

        let composer = cx.entity();
        let chip_rows = self.render_chip_rows(window, cx);
        let input = self.input.clone();
        let draft_glide = self.motion.glide_state(self.motion_key("draft-glide"));
        self.field.update(cx, |field, _| {
            field.input = input;
            field.glide = draft_glide;
            field.placeholder = desired_placeholder.into();
            field.disabled = subagent;
        });

        // The keyboard turns the hairline primary, gliding there and back.
        // Geometry stays fixed: focus never changes border width, radius, or
        // layout.
        let composer_focused = !subagent && self.has_keyboard(window, cx);
        let hairline = self.motion.transition(
            self.motion_key("hairline"),
            if composer_focused {
                cx.theme().primary
            } else {
                cx.theme().border
            },
            GLIDE,
            window,
            cx,
        );
        // A subagent's row keeps the card's place but none of its frame:
        // the hairline, the fill, the shadow and the space above and below
        // the controls fade and close as the field folds away.
        let frame = self.motion.transition(
            self.motion_key("card-frame"),
            if subagent { 0f32 } else { 1. },
            GLIDE,
            window,
            cx,
        );
        let draft = self
            .motion
            .shown(
                self.motion_key("draft"),
                (!subagent).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold(
                    v_flex()
                        .w_full()
                        .pb_1p5()
                        .child(chip_rows)
                        .child(self.field.clone())
                        .child(self.render_image_strip(window, cx)),
                )
            });
        let card = v_flex()
            .debug_selector(move || {
                if subagent {
                    "composer-subagent".into()
                } else {
                    "composer-card".into()
                }
            })
            .w_full()
            .px(px(6.))
            .py(px(6. * frame))
            .rounded(if self.compact {
                px(16.)
            } else {
                crate::material::radius_composer(cx)
            })
            .border_1()
            .border_color(hairline.opacity(frame))
            // The editor needs an opaque fill over the translucent canvas.
            .bg(cx.theme().popover.opacity(frame))
            .shadow_md()
            .map(|mut card| {
                for shadow in card.style().box_shadow.iter_mut().flatten() {
                    shadow.color.a *= frame;
                }
                card
            })
            // Nothing here takes text, so the keyboard rests on the row: the
            // pane keeps it, its shortcuts work, and a key types nowhere.
            .when(subagent, |card| card.track_focus(&self.focus_handle))
            // The editor binds Secondary+V to Paste, which consumes the keystroke
            // before any key-down listener runs — so image paste must intercept
            // the Paste *action* in the capture phase. Swallow it only when the
            // clipboard held an image; text paste propagates to the editor.
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                if this.interactive(cx) && !this.compact && this.paste_clipboard_image(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, paste: &BrowserPaste, window, cx| {
                if this.interactive(cx) && !this.compact && this.paste_images(&paste.0, window, cx)
                {
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
                            this.add_path(path, window, cx);
                        }
                    });
                },
            )
            .children(draft)
            .child(div().w_full().child(control_row));

        // The panels above the card, each folding in as it comes and away,
        // drawn as it was, as it goes.
        let mut panels = Vec::new();
        if let Some(shown) = self.motion.shown(
            self.motion_key("approval"),
            approval.map(|request| (request, approval_count)),
            SHOW,
            window,
            cx,
        ) {
            let (request, count) = &shown.value;
            let panel = self.render_approval_panel(request, *count, window, cx);
            panels.push(column_fold(&shown, panel));
        }
        if let Some(shown) = self.motion.shown(
            self.motion_key("control-request"),
            self.control_request(cx),
            SHOW,
            window,
            cx,
        ) {
            let panel = self.render_control_request_panel(&shown.value, window, cx);
            panels.push(column_fold(&shown, panel));
        }
        if let Some(shown) =
            self.motion
                .shown(self.motion_key("user-input"), user_input, SHOW, window, cx)
        {
            let panel = self.render_user_input_panel(&shown.value, window, cx);
            panels.push(column_fold(&shown, panel));
        }
        if let Some(shown) = self.motion.shown(
            self.motion_key("trigger-menu"),
            self.trigger_menu(cx),
            SHOW,
            window,
            cx,
        ) {
            let panel = self.render_trigger_menu(&shown.value, cx);
            panels.push(column_fold(&shown, panel));
        } else {
            self.menu_rows_seen.borrow_mut().clear();
        }
        if let Some(shown) = self.motion.shown(
            self.motion_key("goal"),
            composer_state.goal.clone(),
            SHOW,
            window,
            cx,
        ) {
            let panel = self.render_goal_strip(&shown.value, window, cx);
            panels.push(column_fold(&shown, panel));
        }
        panels.extend(self.render_force_terminate(window, cx));
        panels.extend(self.render_background_strip(window, cx));
        panels.extend(self.render_queue_strip(window, cx));
        // The checkout row under the card carries the space above it; it
        // folds away as the window turns compact.
        let checkout = self
            .motion
            .shown(
                self.motion_key("checkout"),
                composer_state
                    .checkout
                    .clone()
                    .filter(|_| !self.compact && !subagent),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let row = self.render_checkout_row(&shown.value, cx);
                shown.fold(
                    div()
                        .debug_selector(|| "composer-checkout".into())
                        .w_full()
                        .pt_2()
                        .child(row),
                )
            });
        // The phone's permission and context meter under the card; a
        // subagent's row carries them itself.
        let drawer = self
            .motion
            .shown(
                self.motion_key("settings-drawer"),
                (self.compact && !subagent).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold(
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
                        .child(div().flex_1())
                        .child(self.render_context_meter(cx)),
                )
            });

        // Mirror the chat timeline's centered content column (same page
        // padding, same max width) so the composer lines up with the messages
        // above it instead of stretching edge to edge on wide windows.
        let inset = px(if self.compact {
            crate::material::COMPACT_PAGE_INSET
        } else {
            crate::chat::CONTENT_MIN_PADDING
        });
        // Where the card ends, measured from the composer's bottom once both
        // are laid out: the line under the card, then the composer's whole
        // extent, which comes after it.
        let card_bottom = Rc::new(Cell::new(px(0.)));
        let under_card = gpui::canvas(
            {
                let card_bottom = card_bottom.clone();
                move |bounds, _, _| card_bottom.set(bounds.top())
            },
            |_, _, _, _| {},
        )
        .w_full()
        .h_0();
        let extent = gpui::canvas(
            {
                let below_card = self.below_card.clone();
                move |bounds, _, _| below_card.set(bounds.bottom() - card_bottom.get())
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        v_flex()
            .id(self.scene.id("composer"))
            .relative()
            .flex_shrink_0()
            .w_full()
            .items_center()
            .px(inset)
            .pr(inset + self.column_shift)
            .pb_2()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_menu))
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(crate::chat::CONTENT_MAX_WIDTH))
                    .children(panels)
                    .child(
                        v_flex()
                            .w_full()
                            .child(card)
                            .child(under_card)
                            .children(drawer),
                    )
                    .children(checkout),
            )
            .child(extent)
            .into_any_element()
    }
}

/// Whether `input`'s draft has anything to send but whitespace.
fn draft_has_text(input: &Entity<TextareaState>, cx: &App) -> bool {
    !input.read(cx).value().trim().is_empty()
}

/// The composer's field, drawn by a view of its own: the caret's blink
/// draws this view again, not the composer around it.
struct ComposerField {
    input: Entity<TextareaState>,
    glide: crate::motion::GlideState,
    placeholder: gpui::SharedString,
    disabled: bool,
}

impl Render for ComposerField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The field grows and shrinks a line at a time as the draft does, and
        // clears to one line on send; its height glides there, carrying the
        // card and everything above it.
        let draft_key = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.input.read(cx).value().hash(&mut hasher);
            hasher.finish()
        };
        crate::motion::SizeGlide::new(
            &self.glide,
            Textarea::new(&self.input)
                .placeholder(self.placeholder.clone())
                .disabled(self.disabled)
                .appearance(false),
        )
        .on_change(draft_key)
    }
}

/// A panel in the column above the card, folding in as it comes and away as
/// it goes. It carries the space under it, so the space folds with it.
fn column_fold<T>(shown: &crate::motion::Shown<T>, panel: impl IntoElement) -> AnyElement {
    shown.fold(div().w_full().pb_2().child(panel))
}

#[cfg(test)]
mod replica_tests {
    use super::*;
    use crate::test_frames::draw_settled;
    use gpui::{Modifiers, TestAppContext};
    use std::time::Duration;
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
        attach_as(crate::store::WorkspaceAttachment::Local, cx)
    }

    fn attach_as(
        attachment: crate::store::WorkspaceAttachment,
        cx: &mut TestAppContext,
    ) -> (
        Entity<WorkspaceStore>,
        async_channel::Sender<String>,
        async_channel::Receiver<String>,
        SessionStatus,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("composer-replica");
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
                pump.pump_with_timer(|delay| executor.timer(delay)).await;
            })
            .detach();
        let store =
            cx.new(|cx| WorkspaceStore::new_attached(link, attachment, None, None, false, cx));
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        store.update(cx, |store, cx| {
            store.show_alone(status.session_id.clone(), cx)
        });
        (store, incoming, outgoing, status)
    }

    /// A composer in a window, seeded with its thread's status, ready to send.
    fn composer_on<'a>(
        store: &Entity<WorkspaceStore>,
        incoming: &async_channel::Sender<String>,
        mut status: SessionStatus,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Composer>, &'a mut gpui::VisualTestContext) {
        status.draft = false;
        let session_id = status.session_id.clone();
        replace(
            incoming,
            Topic::SessionStatus {
                session_id: session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let store = store.clone();
        let slot = std::rc::Rc::new(std::cell::OnceCell::new());
        let view = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let composer = cx.new(|cx| Composer::new(store, Some(session_id), window, cx));
            let _ = view.set(composer.clone());
            gpui_base::Root::new(composer, window, cx)
        });
        let composer: Entity<Composer> = slot.get().unwrap().clone();
        // An upload's spinner never rests otherwise.
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        draw_settled(cx);
        (composer, cx)
    }

    /// Drop `paths` on the composer card, as the platform delivers a drag.
    fn drop_paths(paths: Vec<PathBuf>, cx: &mut gpui::VisualTestContext) {
        let position = cx.debug_bounds("composer-card").unwrap().center();
        cx.simulate_event(gpui::FileDropEvent::Entered {
            position,
            paths: ExternalPaths(paths.into()),
        });
        cx.simulate_event(gpui::FileDropEvent::Pending { position });
        cx.simulate_event(gpui::FileDropEvent::Submit { position });
        draw_settled(cx);
    }

    fn sent_texts(outgoing: &async_channel::Receiver<String>) -> Vec<String> {
        std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(tcode_protocol::Command::SendTurn {
                        text,
                        ..
                    }) => Some(text),
                    _ => None,
                },
            )
            .collect()
    }

    /// Answer the next upload query the client sent, returning it.
    fn answer_upload(
        outgoing: &async_channel::Receiver<String>,
        incoming: &async_channel::Sender<String>,
        answer: impl FnOnce(&tcode_protocol::Query) -> Result<tcode_protocol::QueryResponse, String>,
    ) -> Option<tcode_protocol::Query> {
        while let Ok(line) = outgoing.try_recv() {
            let message = tcode_protocol::decode_client_line(&line).unwrap();
            if let tcode_protocol::ClientPayload::Query(
                query @ (tcode_protocol::Query::BeginUpload { .. }
                | tcode_protocol::Query::WriteUpload { .. }
                | tcode_protocol::Query::DiscardUpload { .. }),
            ) = message.payload
            {
                let result = answer(&query).map_err(|message| tcode_protocol::ProtocolError {
                    code: "io_error".into(),
                    message,
                });
                incoming
                    .try_send(
                        encode_line(&HostMessage::QueryResult {
                            id: message.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
                return Some(query);
            }
        }
        None
    }

    fn remote() -> crate::store::WorkspaceAttachment {
        crate::store::WorkspaceAttachment::Remote {
            host_id: "laptop".into(),
            host_name: "Laptop".into(),
        }
    }

    /// A file and a folder on the host's own disk are attached in place: the
    /// message carries their own paths, and nothing is uploaded.
    #[gpui::test]
    fn a_local_file_or_folder_is_sent_as_its_own_path(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, status) = attach(cx);
        let root = crate::store::tests::scratch_root("composer-local-file");
        std::fs::create_dir_all(&root).unwrap();
        let notes = root.join("notes.txt");
        std::fs::write(&notes, "notes").unwrap();
        let (_composer, cx) = composer_on(&store, &incoming, status, cx);
        drop_paths(vec![notes.clone(), root.clone()], cx);
        assert!(cx.debug_bounds("attached-file-0").is_some());
        assert!(cx.debug_bounds("attached-file-1").is_some());
        while outgoing.try_recv().is_ok() {}
        cx.simulate_input("read these");
        cx.simulate_keystrokes("enter");
        draw_settled(cx);
        assert_eq!(
            sent_texts(&outgoing),
            [format!(
                "read these\n\n{}\n{}",
                notes.display(),
                root.display()
            )]
        );
        assert!(cx.debug_bounds("attached-file-0").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The desktop's "+" opens the system's file picker for any number of
    /// files, and what is chosen attaches as a drop would: here, files on the
    /// host's own disk, sent as their own paths.
    #[cfg(feature = "native-dialogs")]
    #[gpui::test]
    fn the_desktop_plus_attaches_picked_files_as_a_drop_would(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, status) = attach(cx);
        let root = crate::store::tests::scratch_root("composer-picked-files");
        std::fs::create_dir_all(&root).unwrap();
        let notes = root.join("notes.txt");
        std::fs::write(&notes, "notes").unwrap();
        let plan = root.join("plan.md");
        std::fs::write(&plan, "plan").unwrap();
        let (_composer, cx) = composer_on(&store, &incoming, status, cx);
        let plus = cx
            .debug_bounds("attach-files")
            .expect("the desktop draws a +");
        cx.simulate_click(plus.center(), gpui::Modifiers::default());
        assert!(cx.did_prompt_for_paths());
        cx.simulate_path_prompt_response(|options| {
            assert!(options.files && options.multiple);
            Some(vec![notes.clone(), plan.clone()])
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("attached-file-0").is_some());
        assert!(cx.debug_bounds("attached-file-1").is_some());
        while outgoing.try_recv().is_ok() {}
        cx.simulate_input("read these");
        cx.simulate_keystrokes("enter");
        draw_settled(cx);
        let sent = sent_texts(&outgoing);
        assert_eq!(
            sent,
            [format!(
                "read these\n\n{}\n{}",
                notes.display(),
                plan.display()
            )]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Copying a file in the file manager and pasting it attaches the file.
    #[gpui::test]
    fn a_pasted_file_is_attached_rather_than_refused(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, status) = attach(cx);
        let root = crate::store::tests::scratch_root("composer-pasted-file");
        std::fs::create_dir_all(&root).unwrap();
        let notes = root.join("notes.txt");
        std::fs::write(&notes, "notes").unwrap();
        let (composer, cx) = composer_on(&store, &incoming, status, cx);
        cx.update(|window, cx| {
            let item = gpui::ClipboardItem {
                entries: vec![gpui::ClipboardEntry::ExternalPaths(ExternalPaths(
                    vec![notes.clone()].into(),
                ))],
            };
            composer.update(cx, |composer, cx| {
                assert!(composer.paste_images(&item, window, cx));
            });
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("attached-file-0").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A remote client uploads the file in chunks and the message carries the
    /// host's path. Until the last chunk lands a send cannot lose the file; a
    /// failed chunk holds the message until the retry goes through.
    #[gpui::test]
    fn a_remote_file_uploads_in_chunks_and_a_failure_can_be_retried(cx: &mut TestAppContext) {
        use tcode_protocol::{Query, QueryResponse};
        let (store, incoming, outgoing, status) = attach_as(remote(), cx);
        let root = crate::store::tests::scratch_root("composer-remote-file");
        std::fs::create_dir_all(&root).unwrap();
        let notes = root.join("notes.txt");
        let content = vec![7u8; tcode_core::attachments::UPLOAD_CHUNK_BYTES + 3];
        std::fs::write(&notes, &content).unwrap();
        let (_composer, cx) = composer_on(&store, &incoming, status, cx);
        let host_path = PathBuf::from("/host/Documents/Tcode/2026-11-08/uploads/s/u/notes.txt");

        drop_paths(vec![notes], cx);
        let begun = answer_upload(&outgoing, &incoming, |_| {
            Ok(QueryResponse::UploadBegun(host_path.clone()))
        })
        .expect("the client asks the host for a path");
        assert!(matches!(
            begun,
            Query::BeginUpload { ref name, size, .. }
                if name == "notes.txt" && size == content.len() as u64
        ));
        draw_settled(cx);

        cx.simulate_input("read this");
        cx.simulate_keystrokes("enter");
        draw_settled(cx);
        let first = answer_upload(&outgoing, &incoming, |_| Err("connection lost".into()))
            .expect("the first chunk goes out");
        assert!(matches!(
            first,
            Query::WriteUpload { offset: 0, ref bytes, .. }
                if bytes.len() == tcode_core::attachments::UPLOAD_CHUNK_BYTES
        ));
        draw_settled(cx);
        assert!(
            cx.debug_bounds("attached-file-0").is_some(),
            "the file stays"
        );
        cx.simulate_keystrokes("enter");
        draw_settled(cx);
        assert_eq!(sent_texts(&outgoing), Vec::<String>::new());

        let retry = cx.debug_bounds("attached-file-retry-0").unwrap().center();
        cx.simulate_click(retry, gpui::Modifiers::none());
        draw_settled(cx);
        let mut received = Vec::new();
        let mut discarded = false;
        while let Some(query) = answer_upload(&outgoing, &incoming, |query| {
            Ok(match query {
                Query::BeginUpload { .. } => QueryResponse::UploadBegun(host_path.clone()),
                Query::WriteUpload { .. } => QueryResponse::UploadWritten,
                _ => QueryResponse::UploadDiscarded,
            })
        }) {
            match query {
                Query::WriteUpload { offset, bytes, .. } => {
                    assert_eq!(offset as usize, received.len());
                    received.extend(bytes);
                }
                Query::DiscardUpload { path, .. } => {
                    assert_eq!(path, host_path);
                    discarded = true;
                }
                _ => {}
            }
            draw_settled(cx);
        }
        assert!(discarded, "the failed attempt leaves nothing on the host");
        assert_eq!(received, content);

        let send = cx.debug_bounds("send-message").unwrap().center();
        cx.simulate_click(send, gpui::Modifiers::none());
        draw_settled(cx);
        assert_eq!(
            sent_texts(&outgoing),
            [format!("read this\n\n{}", host_path.display())]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A folder has no remote path to carry, and a file over the device's
    /// ceiling never leaves a link that cannot be trusted with it.
    #[gpui::test]
    fn a_remote_client_refuses_folders_and_files_over_the_link_ceiling(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, status) = attach_as(remote(), cx);
        let root = crate::store::tests::scratch_root("composer-remote-refused");
        std::fs::create_dir_all(&root).unwrap();
        let large = root.join("large.bin");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(tcode_core::attachments::DEFAULT_REMOTE_BYTES + 1)
            .unwrap();
        let (_composer, cx) = composer_on(&store, &incoming, status, cx);
        drop_paths(vec![root.clone(), large], cx);
        assert!(cx.debug_bounds("attached-file-0").is_none());
        assert_eq!(
            answer_upload(&outgoing, &incoming, |_| Err("unexpected".into())),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
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

    /// The prompt a rewind gives back goes into the composer of the client
    /// that asked for the rewind, ahead of what the user had typed, which it
    /// never replaces; another client's rewind leaves this composer alone.
    #[gpui::test]
    fn a_rewound_prompt_goes_before_the_draft_of_the_client_that_asked(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, status) = attach(cx);
        let session_id = status.session_id.clone();
        let topic = Topic::SessionStatus {
            session_id: session_id.clone(),
        };
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let own = store.read_with(cx, |store, _| store.client_id().to_owned());
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), Some(session_id.clone()), window, cx)
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        cx.simulate_input("and also this");
        for (client_id, text) in [
            ("another-client", "asked elsewhere"),
            (own.as_str(), "the restored prompt"),
        ] {
            replace(
                &incoming,
                topic.clone(),
                ServerEvent::RewindPrefill {
                    session_id: session_id.clone(),
                    client_id: client_id.to_owned(),
                    text: text.to_owned(),
                    attachments: Vec::new(),
                },
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "the restored prompt\n\nand also this"
        );
    }

    #[gpui::test]
    fn composer_history_survives_submission_and_destination_switches(cx: &mut TestAppContext) {
        for is_draft in [false, true] {
            let (store, incoming, outgoing, mut status) = attach(cx);
            let session_id = Some(status.session_id.clone());
            status.draft = is_draft;
            status.project_id = Some("project-a".into());
            let mut restored_status = status.clone();
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            let (composer, cx) = cx.add_window_view(|window, cx| {
                Composer::new(store.clone(), session_id.clone(), window, cx)
            });
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                composer.update(cx, |composer, cx| composer.focus(window, cx));
            });
            cx.simulate_input("Keep this prompt");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let send = cx.debug_bounds("send-message").expect("send button");
            cx.simulate_click(send.center(), Modifiers::default());
            cx.update(|window, cx| {
                composer.update(cx, |composer, cx| composer.focus(window, cx));
                window.draw(cx).clear(cx);
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            assert!(
                std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                    tcode_protocol::decode_client_line(&line).unwrap().payload,
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::SendTurn { .. }
                    )
                ))
            );
            cx.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-z"
            } else {
                "ctrl-z"
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "Keep this prompt"
            );
            status.session_id = "other-thread".into();
            status.project_id = Some("project-b".into());
            store.update(cx, |store, cx| {
                store.show_alone(status.session_id.clone(), cx)
            });
            composer.update(cx, |composer, cx| {
                composer.set_session(Some(status.session_id.clone()), cx)
            });
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| {
                store.drain_host_events_for_test(cx);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            cx.update(|window, cx| composer.update(cx, |composer, cx| composer.focus(window, cx)));
            cx.simulate_input("Other destination draft");
            let outgoing = composer.read_with(cx, |composer, _| composer.input.clone());
            cx.update(|window, cx| {
                outgoing.update(cx, |input, cx| {
                    use gpui::EntityInputHandler as _;
                    input.replace_and_mark_text_in_range(None, "，你好", None, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            if is_draft {
                restored_status.session_id = "reopened-project-draft".into();
            }
            store.update(cx, |store, cx| {
                store.show_alone(restored_status.session_id.clone(), cx)
            });
            composer.update(cx, |composer, cx| {
                composer.set_session(Some(restored_status.session_id.clone()), cx)
            });
            replace(
                &incoming,
                Topic::SessionStatus {
                    session_id: restored_status.session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(restored_status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                composer.update(cx, |composer, cx| composer.focus(window, cx));
                outgoing.update(cx, |input, cx| {
                    use gpui::EntityInputHandler as _;
                    assert_eq!(input.marked_text_range(window, cx), None);
                    assert_eq!(input.value(), "Other destination draft，你好");
                });
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "Keep this prompt"
            );
            cx.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-shift-z"
            } else {
                "ctrl-y"
            });
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                ""
            );
            for (destination, expected) in [
                (status, "Other destination draft，你好"),
                (restored_status, ""),
            ] {
                store.update(cx, |store, cx| {
                    store.show_alone(destination.session_id.clone(), cx)
                });
                composer.update(cx, |composer, cx| {
                    composer.set_session(Some(destination.session_id.clone()), cx)
                });
                replace(
                    &incoming,
                    Topic::SessionStatus {
                        session_id: destination.session_id.clone(),
                    },
                    ServerEvent::SessionStatusReplaced(Box::new(destination)),
                );
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert_eq!(
                    composer.read_with(cx, |composer, cx| composer.draft(cx)),
                    expected
                );
            }
        }
    }

    /// A desktop composer folds its controls into the overflow menu once its
    /// row is laid out narrower than they fit, and unfolds them once wider,
    /// in the frames that follow the resize.
    #[gpui::test]
    fn the_control_row_folds_with_the_width_it_is_laid_out_at(cx: &mut TestAppContext) {
        let (store, _incoming, _outgoing, status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        let resize = |width: f32, cx: &mut gpui::VisualTestContext| {
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            crate::test_frames::draw_settled(cx);
            cx.debug_bounds("overflow-controls").is_some()
        };
        assert!(!resize(1200., cx), "a wide row folded its controls");
        assert!(resize(400., cx), "a narrow row kept its controls");
        assert!(!resize(1200., cx), "a row widened again kept them folded");
    }

    /// A running turn widens the desktop action to spinner, steer and Stop;
    /// at every width the row is laid out at, Stop stays inside the window and
    /// clear of a model name too long for the row.
    #[gpui::test]
    fn a_running_turn_keeps_stop_on_screen_at_every_width(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.activity.turn_running = true;
        status.steering_supported = true;
        status.requested_model = Some("an-unlisted-model-with-a-long-identifier".into());
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        for width in (360..=1200).step_by(20) {
            let width = width as f32;
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            for _ in 0..100 {
                cx.run_until_parked();
                if cx.update(|window, cx| window.simulate_next_frame(cx)) == 0 {
                    break;
                }
            }
            let stop = cx
                .debug_bounds("stop-turn")
                .unwrap_or_else(|| panic!("no Stop at {width}px"));
            assert!(
                f32::from(stop.right()) <= width,
                "Stop is clipped at {width}px: its right edge is at {:?}",
                stop.right()
            );
            let picker = cx.debug_bounds("model-picker").unwrap();
            assert!(
                picker.right() <= stop.left(),
                "the model picker runs under Stop at {width}px: {picker:?} vs {stop:?}"
            );
        }
    }

    /// A turn starting widens the desktop send control into its spinner and
    /// Stop: they open along the row as Send narrows away, so Stop arrives
    /// from the right rather than appearing in its place.
    #[gpui::test]
    fn a_starting_turn_widens_send_into_stop(cx: &mut TestAppContext) {
        use crate::test_frames::{draw, draw_settled};
        let (store, incoming, _outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(800.)));
        draw_settled(cx);
        assert!(cx.debug_bounds("send-message").is_some());
        status.activity.turn_running = true;
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let opening = cx.debug_bounds("stop-turn").expect("Stop opens").left();
        assert!(
            cx.debug_bounds("send-message").is_some(),
            "Send is narrowing away"
        );
        // The spinner turns for as long as the turn runs, so the window never
        // rests; time enough for the row to settle has passed.
        cx.executor().advance_clock(crate::edge_back::SETTLE * 2);
        draw(cx);
        draw(cx);
        let open = cx.debug_bounds("stop-turn").unwrap().left();
        assert!(
            opening > open,
            "Stop came in from the right: {opening:?} then {open:?}"
        );
        assert!(cx.debug_bounds("send-message").is_none());
    }

    /// A value the provider reports as unavailable is shown but never sent.
    #[gpui::test]
    fn unavailable_permission_rows_cannot_be_chosen(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let mut descriptor = agent::permission_control(status.provider).unwrap();
        let OptionDescriptor::Select { id, options, .. } = &mut descriptor else {
            panic!("the draft's provider publishes a select permission control");
        };
        let id = id.clone();
        options[1].unavailable = Some("Not available for this account".into());
        let chosen = options[0].value.clone();
        status
            .provider_option_descriptors
            .retain(|existing| !matches!(existing, OptionDescriptor::Select { id: existing, .. } if *existing == id));
        status.provider_option_descriptors.push(descriptor);
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, cx| _ = window.draw(cx));
        while outgoing.try_recv().is_ok() {}
        let sent = || {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::SetActiveOption { id, value, .. },
                        ) => Some((id, value)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };
        let click = |cx: &mut gpui::VisualTestContext, selector: &'static str| {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector}"));
            cx.simulate_click(bounds.center(), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
        };
        click(cx, "permission-chip");
        click(cx, "permission-row-1");
        cx.run_until_parked();
        assert_eq!(sent(), []);
        assert!(
            cx.debug_bounds("permission-pane").is_some(),
            "the menu stays open"
        );
        click(cx, "permission-row-0");
        cx.run_until_parked();
        assert_eq!(sent(), [(id, Some(serde_json::Value::String(chosen)))]);
    }

    /// The model picker's current row is highlighted with the same inset
    /// from its column on both sides: clear of the rail's divider on the
    /// left as much as of the popover's edge on the right.
    #[gpui::test]
    fn the_current_model_is_highlighted_with_an_even_inset(cx: &mut TestAppContext) {
        let (store, incoming, _outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let models = ["first", "chosen", "last"].map(|id| agent::ModelSpec {
            id: id.into(),
            display_name: id.into(),
            is_default: false,
            options: Vec::new(),
        });
        status.requested_model = Some("chosen".into());
        replace(
            &incoming,
            Topic::Providers,
            ServerEvent::ProvidersReplaced(tcode_protocol::ProvidersStatus {
                model_catalogs: [(status.provider, models.to_vec())].into_iter().collect(),
                ..Default::default()
            }),
        );
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        crate::test_frames::draw_settled(cx);
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), Modifiers::default());
        crate::test_frames::draw_settled(cx);
        let column = cx.debug_bounds("model-picker-list").expect("model list");
        let current = cx.debug_bounds("model-row-current").expect("current model");
        let left = current.left() - column.left();
        let right = column.right() - current.right();
        assert!(
            left > px(0.),
            "the highlight touches the rail: {current:?} in {column:?}"
        );
        assert_eq!(left, right, "{current:?} in {column:?}");
    }

    /// A draft on the built-in Claude account of a host with a second account
    /// of that CLI, which lists the same two models.
    struct TwoAccounts {
        root: std::path::PathBuf,
        host: tcode_runtime::pipe::SpawnedHost,
        driver: crate::store::tests::LocalHostDriver,
        store: gpui::Entity<WorkspaceStore>,
        session_id: String,
        launches: smol::channel::Receiver<agent::SessionOptions>,
    }

    fn two_accounts(cx: &mut TestAppContext, name: &str) -> TwoAccounts {
        use agent::ProviderKind;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root(name);
        let project = tcode_core::project::Project::from_root(root.clone());
        let disk = tcode_services::store::SessionStore::open_at(root.clone()).unwrap();
        disk.upsert_project(&project).unwrap();
        let host =
            tcode_runtime::pipe::spawn_host(disk, tcode_runtime::pipe::HostServices::default())
                .unwrap();
        let scripted = tcode_runtime::app::scripted_provider(ProviderKind::ClaudeCode);
        let (launcher, launches, project_id, cwd) = (
            scripted.launcher.clone(),
            scripted.launches.clone(),
            project.id.clone(),
            root.clone(),
        );
        let session_id = smol::block_on(host.update_state_for_test(move |state, cx| {
            let spec = |id: &str| agent::ModelSpec {
                id: id.into(),
                display_name: id.into(),
                is_default: false,
                options: Vec::new(),
            };
            state.providers.model_catalogs.insert(
                ProviderKind::ClaudeCode,
                vec![spec("first-model"), spec("shared-model")],
            );
            state.settings.profiles.insert(
                "second-account".to_owned(),
                tcode_core::settings::ProviderProfile {
                    kind: ProviderKind::ClaudeCode,
                    settings: tcode_core::settings::ProviderSettings {
                        home_path: Some("/profile-homes/second-account".into()),
                        ..Default::default()
                    },
                },
            );
            state.set_provider_launcher_for_test(launcher);
            state.start_draft(project_id, cwd, cx)
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &store, "the draft's status", |cx| {
            store.read_with(cx, |store, cx| {
                store
                    .composer_state_for(Some(&session_id), cx)
                    .active_model
                    .is_some()
            })
        });
        TwoAccounts {
            root,
            host,
            driver,
            store,
            session_id,
            launches,
        }
    }

    fn show_rail(composer: &Entity<Composer>, rail: PickerRail, cx: &mut gpui::VisualTestContext) {
        composer.update(cx, |composer, cx| {
            composer.picker_rail = Some(rail);
            cx.notify();
        });
        crate::test_frames::draw_settled(cx);
    }

    fn open_picker(cx: &mut gpui::VisualTestContext) {
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), Modifiers::default());
        crate::test_frames::draw_settled(cx);
    }

    /// Two accounts of one CLI list the same ids. A model picked under the
    /// second reads as selected there alone, and the thread runs on it.
    #[gpui::test]
    fn a_model_is_selected_under_the_account_it_was_picked_on(cx: &mut TestAppContext) {
        use crate::test_frames::draw_settled;
        use agent::ProviderKind;

        let TwoAccounts {
            root,
            host,
            mut driver,
            store,
            session_id,
            launches,
        } = two_accounts(cx, "account-selection");
        let session = Some(session_id.clone());
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), session, window, cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        draw_settled(cx);

        open_picker(cx);
        show_rail(&composer, PickerRail::Profile("second-account".into()), cx);
        cx.simulate_keystrokes("secondary-2");
        draw_settled(cx);
        driver.wait_until(cx, &store, "the pick to reach the thread", |cx| {
            store.read_with(cx, |store, cx| {
                store
                    .composer_state_for(Some(&session_id), cx)
                    .active_model
                    .is_some_and(|active| {
                        active.model.as_deref() == Some("shared-model")
                            && active.profile_id.as_deref() == Some("second-account")
                    })
            })
        });
        draw_settled(cx);

        open_picker(cx);
        assert!(
            cx.debug_bounds("model-row-current").is_some(),
            "the account the model was picked on shows it selected"
        );
        show_rail(&composer, PickerRail::Profile("claude".into()), cx);
        assert!(
            cx.debug_bounds("model-row-current").is_none(),
            "the other account lists the same id, unselected"
        );
        cx.simulate_keystrokes("escape");
        draw_settled(cx);

        store.update(cx, |store, _| {
            store.send_turn_for(session_id.clone(), "hello".into(), Vec::new())
        });
        driver.wait_until(cx, &store, "the provider launch", |_| !launches.is_empty());
        let launch = launches.try_recv().unwrap();
        assert_eq!(launch.model.as_deref(), Some("shared-model"));
        assert!(
            launch
                .launch_env
                .pairs(ProviderKind::ClaudeCode)
                .contains(&(
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    "/profile-homes/second-account".to_owned()
                )),
            "the thread launches on the second account"
        );
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The star on a second account's row lights that row alone: the first
    /// account's row of the same id stays dark, Favorites lists the one row,
    /// and its shortcut picks it on the account it was starred under.
    #[gpui::test]
    fn a_favorite_under_the_second_account_stars_only_that_row(cx: &mut TestAppContext) {
        use crate::test_frames::draw_settled;

        let TwoAccounts {
            root,
            host,
            mut driver,
            store,
            session_id,
            ..
        } = two_accounts(cx, "account-favorites");
        let session = Some(session_id.clone());
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), session, window, cx));
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        draw_settled(cx);

        open_picker(cx);
        show_rail(&composer, PickerRail::Profile("second-account".into()), cx);
        let star = cx
            .debug_bounds("model-fav-1")
            .expect("the second row's star");
        cx.simulate_click(star.center(), Modifiers::default());
        driver.wait_until(cx, &store, "the star to reach the host", |cx| {
            store.read_with(cx, |store, _| !store.settings().favorite_models.is_empty())
        });
        draw_settled(cx);

        let starred = |profile: &str, cx: &gpui::VisualTestContext| {
            store.read_with(cx, |store, _| {
                store
                    .picker_models_for_profile(profile)
                    .into_iter()
                    .filter(|model| model.favorite)
                    .map(|model| model.id)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(starred("second-account", cx), ["shared-model"]);
        assert!(starred("claude", cx).is_empty());

        show_rail(&composer, PickerRail::Favorites, cx);
        assert!(cx.debug_bounds("model-fav-0").is_some());
        assert!(
            cx.debug_bounds("model-fav-1").is_none(),
            "Favorites lists the starred row, not its namesake on the other account"
        );
        // Focus went with the star's button when its row moved to the top.
        cx.update(|window, cx| {
            let search = composer.read(cx).model_search_focus_handle(cx);
            search.focus(window, cx);
        });
        cx.simulate_keystrokes("secondary-1");
        driver.wait_until(cx, &store, "the favourite's pick", |cx| {
            store.read_with(cx, |store, cx| {
                store
                    .composer_state_for(Some(&session_id), cx)
                    .active_model
                    .is_some_and(|active| {
                        active.model.as_deref() == Some("shared-model")
                            && active.profile_id.as_deref() == Some("second-account")
                    })
            })
        });
        driver.shutdown(cx, &store, &host);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_model_search_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        let (store, incoming, _outgoing, status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.update(|window, cx| _ = window.draw(cx));
        let picker = cx.debug_bounds("model-picker").expect("model picker");
        cx.simulate_click(picker.center(), Modifiers::default());
        crate::settings::assert_follows_language_switch(cx, "composer.search_models", |cx| {
            composer.read_with(cx, |composer, cx| {
                let search = composer.model_search.read(cx);
                search.presentation().placeholder().to_string()
            })
        });
    }

    #[gpui::test]
    fn holding_queue_reveals_choices_and_release_height_picks_steer_queue_or_nothing(
        cx: &mut TestAppContext,
    ) {
        use gpui::{TouchEvent, TouchId, TouchPhase, point};

        let (store, incoming, outgoing, mut status) = attach(cx);
        status.activity.turn_running = true;
        status.steering_supported = true;
        let mut unsupported = status.clone();
        unsupported.steering_supported = false;
        // A phone sends only once its screen is current, so every baseline
        // topic is answered.
        let session_id = status.session_id.clone();
        let baseline = [
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: Vec::new(),
                }),
            ),
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(tcode_core::settings::Settings::default()),
            ),
            (
                Topic::SessionStatus {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::SessionPlan {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                    session_id: session_id.clone(),
                    steps: Vec::new(),
                    explanation: None,
                }),
            ),
            (
                Topic::SessionEvents {
                    session_id: session_id.clone(),
                },
                ServerEvent::SessionSnapshot {
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                },
            ),
        ];
        for (topic, event) in baseline {
            replace(&incoming, topic, event);
        }
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), Some(session_id.clone()), window, cx)
        });
        composer.update(cx, |composer, cx| composer.set_compact(true, cx));
        cx.simulate_resize(gpui::size(px(393.), px(800.)));
        let touch = |cx: &mut gpui::VisualTestContext, phase, position| {
            cx.simulate_event(TouchEvent {
                id: TouchId(1),
                phase,
                position,
                predicted_position: None,
                force: None,
            });
            cx.update(|window, cx| _ = window.draw(cx));
        };
        let sent =
            |cx: &mut gpui::VisualTestContext| {
                cx.executor().advance_clock(Duration::from_millis(50));
                cx.run_until_parked();
                let mut sent = Vec::new();
                while let Ok(line) = outgoing.try_recv() {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::Steer { text, .. },
                        ) => sent.push(("steer", text)),
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::SendTurn { text, .. },
                        ) => sent.push(("queue", text)),
                        _ => {}
                    }
                }
                sent
            };
        assert!(sent(cx).is_empty());

        let mut upper_target = None;
        for (gesture, expected) in [
            ("hold", "queue"),
            ("hold-slide", "steer"),
            ("slide", "steer"),
            ("direct-slide", "steer"),
            ("lower", "queue"),
            ("tap", "queue"),
            ("sideways", "queue"),
            ("hold-sideways", "queue"),
            ("steer-sideways", "steer"),
        ] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_input_text(gesture.into(), window, cx)
            });
            cx.update(|window, cx| _ = window.draw(cx));
            let button = cx
                .debug_bounds("steer-turn")
                .expect("the Queue button is shown");
            let center = button.center();
            touch(cx, TouchPhase::Started, center);
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            let mut lift = center;
            if matches!(gesture, "hold" | "hold-slide" | "lower" | "hold-sideways") {
                cx.executor()
                    .advance_clock(gpui::GestureTuning::default().long_press_duration);
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            if matches!(gesture, "slide" | "steer-sideways") {
                touch(
                    cx,
                    TouchPhase::Moved,
                    point(center.x, button.top() - px(1.)),
                );
            }
            if gesture == "direct-slide" {
                lift = upper_target.expect("the previous gesture measured Steer");
                touch(cx, TouchPhase::Moved, lift);
            }
            if gesture == "sideways" {
                lift = point(center.x - px(120.), center.y);
                touch(cx, TouchPhase::Moved, lift);
            }
            if gesture != "tap" && gesture != "sideways" {
                cx.update(|window, cx| _ = window.draw(cx));
                let upper = cx
                    .debug_bounds("steer-gesture-steer")
                    .expect("holding reveals the upper Steer button");
                let lower = cx
                    .debug_bounds("steer-gesture-queue")
                    .expect("holding reveals the lower Queue button");
                assert_eq!(lower, button, "Queue stays under the finger");
                assert_eq!(upper.center().x, lower.center().x);
                assert!(
                    upper.bottom() <= lower.top(),
                    "Steer is directly above Queue"
                );
                upper_target = Some(upper.center());
                if gesture == "direct-slide" {
                    assert!(cx.debug_bounds("steer-gesture-steer-selected").is_some());
                } else if !matches!(gesture, "slide" | "steer-sideways") {
                    assert!(cx.debug_bounds("steer-gesture-queue-selected").is_some());
                }
                match gesture {
                    "hold-sideways" => {
                        lift = point(center.x - px(120.), lower.top() - px(4.));
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-queue-selected").is_some(),
                            "level with Queue queues wherever the finger is horizontally"
                        );
                    }
                    "steer-sideways" => {
                        lift = point(center.x - px(120.), upper.top() - px(40.));
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-steer-selected").is_some(),
                            "anywhere above the gap under Steer steers"
                        );
                    }
                    "slide" | "hold-slide" | "lower" => {
                        lift = upper.center();
                        touch(cx, TouchPhase::Moved, lift);
                        assert!(
                            cx.debug_bounds("steer-gesture-steer-selected").is_some(),
                            "{gesture}: moving onto Steer highlights it"
                        );
                        if gesture == "lower" {
                            lift = lower.center();
                            touch(cx, TouchPhase::Moved, lift);
                            assert!(cx.debug_bounds("steer-gesture-queue-selected").is_some());
                        }
                    }
                    _ => {}
                }
            }
            assert!(
                sent(cx).is_empty(),
                "nothing is sent before the finger lifts"
            );
            touch(cx, TouchPhase::Ended, lift);
            assert_eq!(sent(cx), vec![(expected, gesture.to_string())], "{gesture}");
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            assert!(
                composer
                    .read_with(cx, |composer, cx| composer.draft(cx))
                    .is_empty()
            );
        }

        for gesture in ["down", "hold-down", "steer-down", "cancel"] {
            composer.update_in(cx, |composer, window, cx| {
                composer.set_input_text("kept".into(), window, cx)
            });
            cx.update(|window, cx| _ = window.draw(cx));
            let button = cx.debug_bounds("steer-turn").unwrap();
            touch(cx, TouchPhase::Started, button.center());
            if gesture == "hold-down" {
                cx.executor()
                    .advance_clock(gpui::GestureTuning::default().long_press_duration);
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            let mut lift = button.center();
            if matches!(gesture, "steer-down" | "cancel") {
                touch(
                    cx,
                    TouchPhase::Moved,
                    point(button.center().x, button.top() - px(1.)),
                );
                let upper = cx.debug_bounds("steer-gesture-steer").unwrap();
                touch(cx, TouchPhase::Moved, upper.center());
                lift = upper.center();
            }
            let away = point(button.center().x - px(120.), button.bottom() + px(1.));
            if gesture == "cancel" {
                touch(cx, TouchPhase::Cancelled, lift);
            } else {
                touch(cx, TouchPhase::Moved, away);
                if gesture != "down" {
                    assert!(cx.debug_bounds("steer-gesture-steer").is_some());
                    assert!(cx.debug_bounds("steer-gesture-steer-selected").is_none());
                    assert!(cx.debug_bounds("steer-gesture-queue-selected").is_none());
                }
                touch(cx, TouchPhase::Ended, away);
            }
            assert!(sent(cx).is_empty(), "{gesture} sends nothing");
            assert!(cx.debug_bounds("steer-gesture-steer").is_none());
            assert!(cx.debug_bounds("steer-gesture-queue").is_none());
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                "kept"
            );
        }

        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: unsupported.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(unsupported)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        let button = cx.debug_bounds("steer-turn").unwrap();
        touch(cx, TouchPhase::Started, button.center());
        cx.executor()
            .advance_clock(gpui::GestureTuning::default().long_press_duration);
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("steer-gesture-steer").is_none());
        assert!(cx.debug_bounds("steer-gesture-queue").is_none());
        touch(cx, TouchPhase::Cancelled, button.center());
        assert!(sent(cx).is_empty());
    }

    /// The background work the host lists shows above the field with a stop
    /// for each piece and for all of it, while no turn runs.
    #[gpui::test]
    fn background_tasks_stop_from_the_composer(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.background_tasks = vec![tcode_protocol::BackgroundTaskStatus {
            task: agent::BackgroundTask {
                id: "bl5ti808a".into(),
                name: "npm run dev".into(),
                kind: agent::BackgroundKind::Command,
                item_id: None,
                started_at: 1,
                started_reported: false,
            },
            listed: true,
            stopping_since: None,
            ended: false,
        }];
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.update(|window, cx| _ = window.draw(cx));
        while outgoing.try_recv().is_ok() {}
        for selector in ["background-stop-bl5ti808a", "background-stop-all"] {
            let button = cx.debug_bounds(selector).expect("stop shown");
            cx.simulate_click(button.center(), Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
        }
        cx.run_until_parked();
        let stops: Vec<_> = std::iter::from_fn(|| outgoing.try_recv().ok())
            .filter_map(
                |line| match tcode_protocol::decode_client_line(&line).unwrap().payload {
                    tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::StopBackgroundTask { task_id, .. },
                    ) => Some(task_id),
                    _ => None,
                },
            )
            .collect();
        assert_eq!(stops, [Some("bl5ti808a".to_owned()), None]);
    }

    /// A stop the harness has not answered for a while offers to kill its
    /// process; a fresh one does not, and the offer itself only asks.
    #[gpui::test]
    fn an_unanswered_stop_offers_force_terminate_behind_a_confirmation(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let topic = Topic::SessionStatus {
            session_id: status.session_id.clone(),
        };
        status.activity.turn_running = true;
        status.stopping = true;
        status.stopping_since = Some(chrono::Utc::now().timestamp_millis() as u64);
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let composer =
                cx.new(|cx| Composer::new(store.clone(), session_id.clone(), window, cx));
            gpui_base::Root::new(composer, window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("force-terminate").is_none());

        status.stopping_since = Some(1);
        replace(
            &incoming,
            topic,
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        while outgoing.try_recv().is_ok() {}
        let button = cx
            .debug_bounds("force-terminate")
            .expect("offered after the wait");
        cx.simulate_click(button.center(), Modifiers::default());
        cx.run_until_parked();
        assert!(cx.update(|window, cx| crate::overlay::dialog_open(window, cx)));
        assert!(
            !std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                tcode_protocol::decode_client_line(&line).unwrap().payload,
                tcode_protocol::ClientPayload::Command(
                    tcode_protocol::Command::ForceTerminate { .. }
                )
            )),
            "the process is killed only once the user confirms"
        );
    }

    #[gpui::test]
    fn queue_edit_waits_for_host_removal_and_in_flight_actions_are_disabled(
        cx: &mut TestAppContext,
    ) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
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
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        // Each step is drawn once; with motion reduced it is drawn at rest.
        cx.update(|_, cx| cx.set_reduce_motion(true));
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

    /// Dragging a queued row onto another asks the host to send it in that
    /// row's place; letting go where it started asks nothing.
    /// A queued message the host sends folds away and the one after it rises
    /// into its place; the strip folds away with its last row. With motion
    /// reduced both are gone at once.
    #[gpui::test]
    fn a_sent_queued_row_folds_away_and_the_strip_with_the_last(cx: &mut TestAppContext) {
        use crate::test_frames::{draw, draw_settled};
        let (store, incoming, _outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        let queued = |ids: &[u64]| {
            ids.iter()
                .map(|&id| tcode_protocol::QueuedMessageStatus {
                    id,
                    editable: true,
                    delivery_key: None,
                    text: format!("Queued {id}"),
                    fire_at_unix_secs: None,
                })
                .collect::<Vec<_>>()
        };
        let topic = Topic::SessionStatus {
            session_id: status.session_id.clone(),
        };
        status.queued_messages = queued(&[1, 2]);
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        draw_settled(cx);
        let top = |id: u64, cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds(format!("queued-row-{id}").leak())
                .map(|row| row.top())
        };
        let card =
            |cx: &mut gpui::VisualTestContext| cx.debug_bounds("composer-card").unwrap().top();
        let (first, second) = (top(1, cx).unwrap(), top(2, cx).unwrap());
        let card_top = card(cx);
        let mut send = |ids: &[u64], cx: &mut gpui::VisualTestContext| {
            status.queued_messages = queued(ids);
            replace(
                &incoming,
                topic.clone(),
                ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
            );
            cx.run_until_parked();
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            draw(cx);
            cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
            draw(cx);
        };

        send(&[2], cx);
        assert!(top(1, cx).is_some(), "the sent row is still folding away");
        let rising = top(2, cx).unwrap();
        assert!(
            rising > first && rising < second,
            "the row left rises as the one above it folds: {rising:?} from {second:?} to {first:?}"
        );
        draw_settled(cx);
        assert_eq!(top(1, cx), None);
        assert_eq!(top(2, cx), Some(first));

        send(&[], cx);
        assert!(
            top(2, cx).is_some(),
            "the last row folds away with its strip"
        );
        draw_settled(cx);
        assert_eq!(top(2, cx), None);
        assert!(
            card(cx) < card_top,
            "the card takes the room the strip gave back"
        );

        cx.update(|_, cx| cx.set_reduce_motion(true));
        status.queued_messages = queued(&[3]);
        replace(
            &incoming,
            topic.clone(),
            ServerEvent::SessionStatusReplaced(Box::new(status.clone())),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        assert!(top(3, cx).is_some());
        status.queued_messages.clear();
        replace(
            &incoming,
            topic,
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        draw(cx);
        assert_eq!(top(3, cx), None, "with motion reduced it is gone at once");
    }

    #[gpui::test]
    fn dragging_a_queued_row_moves_it_in_the_host_queue(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.queued_messages = (1..=3)
            .map(|id| tcode_protocol::QueuedMessageStatus {
                id,
                editable: true,
                delivery_key: None,
                text: format!("Queued {id}"),
                fire_at_unix_secs: None,
            })
            .collect();
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| _ = window.draw(cx));
        draw(cx);
        let left = gpui::MouseButton::Left;
        let drag = |cx: &mut gpui::VisualTestContext, id: u64, to: gpui::Point<gpui::Pixels>| {
            let start = cx
                .debug_bounds(format!("queued-row-{id}").leak())
                .unwrap()
                .center();
            cx.simulate_mouse_down(start, left, Modifiers::default());
            cx.simulate_mouse_move(
                start + gpui::point(px(0.), px(8.)),
                left,
                Modifiers::default(),
            );
            draw(cx);
            cx.simulate_mouse_move(to, left, Modifiers::default());
            draw(cx);
            cx.simulate_mouse_up(to, left, Modifiers::default());
            draw(cx);
        };
        let moves = |outgoing: &async_channel::Receiver<String>| {
            std::iter::from_fn(|| outgoing.try_recv().ok())
                .filter_map(|line| {
                    match tcode_protocol::decode_client_line(&line).unwrap().payload {
                        tcode_protocol::ClientPayload::Command(
                            tcode_protocol::Command::MoveQueued { id, before, .. },
                        ) => Some((id, before)),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        };

        let first = cx.debug_bounds("queued-row-1").unwrap();
        drag(cx, 3, gpui::point(first.center().x, first.top() + px(2.)));
        assert_eq!(moves(&outgoing), [(3, Some(1))]);

        let last = cx.debug_bounds("queued-row-3").unwrap();
        drag(cx, 1, gpui::point(last.center().x, last.bottom() - px(2.)));
        assert_eq!(moves(&outgoing), [(1, None)]);

        let second = cx.debug_bounds("queued-row-2").unwrap().center();
        drag(cx, 2, second);
        assert!(moves(&outgoing).is_empty(), "a row dropped in place stays");
    }

    /// A queued row lifts off its place as it is dragged, and once let go its
    /// ghost travels back into the row's slot rather than vanishing, the row
    /// showing again as it arrives; with motion reduced it is there at once.
    #[gpui::test]
    fn a_dragged_queued_row_settles_back_into_its_slot(cx: &mut TestAppContext) {
        use crate::test_frames::{draw, draw_settled};
        let (store, incoming, _outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.queued_messages = (1..=3)
            .map(|id| tcode_protocol::QueuedMessageStatus {
                id,
                editable: true,
                delivery_key: None,
                text: format!("Queued {id}"),
                fire_at_unix_secs: None,
            })
            .collect();
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let composer =
                cx.new(|cx| Composer::new(store.clone(), session_id.clone(), window, cx));
            gpui_base::Root::new(composer, window, cx)
        });
        cx.simulate_resize(gpui::size(px(1024.), px(768.)));
        draw_settled(cx);
        let left = gpui::MouseButton::Left;
        let none = Modifiers::default();
        let ghost = |cx: &mut gpui::VisualTestContext| cx.debug_bounds("drag-ghost-queued-1");
        let row = cx.debug_bounds("queued-row-1").unwrap();
        let last = cx.debug_bounds("queued-row-3").unwrap();

        let start = row.center();
        cx.simulate_mouse_down(start, left, none);
        cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
        draw(cx);
        let lifted = ghost(cx).expect("the row lifts as the drag starts");
        assert_eq!(lifted.origin, row.origin, "it lifts from where the row is");
        assert_eq!(lifted.size.width, row.size.width);

        let over = gpui::point(last.center().x, last.bottom() - px(2.));
        cx.simulate_mouse_move(over, left, none);
        cx.executor().advance_clock(crate::edge_back::SETTLE);
        draw(cx);
        let carried = ghost(cx).unwrap();
        assert!(
            carried.top() < lifted.top() + (over.y - start.y - px(8.)),
            "it follows the pointer, risen off the page"
        );

        cx.simulate_mouse_up(over, left, none);
        draw(cx);
        assert_eq!(
            ghost(cx).map(|ghost| ghost.origin),
            Some(carried.origin),
            "let go, it starts back from where it was"
        );
        cx.executor().advance_clock(crate::edge_back::SETTLE / 2);
        draw(cx);
        let returning = ghost(cx).unwrap().top();
        assert!(
            returning < carried.top() && returning > row.top(),
            "it is on its way back"
        );
        draw_settled(cx);
        assert_eq!(ghost(cx), None, "it has settled into the row");
        assert_eq!(cx.debug_bounds("queued-row-1").unwrap(), row);

        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.simulate_mouse_down(start, left, none);
        cx.simulate_mouse_move(start + gpui::point(px(0.), px(8.)), left, none);
        draw(cx);
        cx.simulate_mouse_move(over, left, none);
        draw(cx);
        assert!(ghost(cx).is_some());
        cx.simulate_mouse_up(over, left, none);
        draw(cx);
        assert_eq!(ghost(cx), None, "with motion reduced it is back at once");
    }

    /// Choosing a thread from the `@` menu references it: the host is asked to
    /// add the reference and the draft keeps no mention text.
    #[gpui::test]
    fn an_at_thread_is_referenced_rather_than_written_into_the_draft(cx: &mut TestAppContext) {
        let (store, incoming, outgoing, mut status) = attach(cx);
        let session_id = Some(status.session_id.clone());
        status.draft = false;
        status.control = Some(tcode_protocol::ControlStatus {
            grant: tcode_core::control::ControlGrant::default_grant(),
            source: tcode_core::control::ControlGrantSource::Default,
            pending_requests: Vec::new(),
            references: Vec::new(),
            self_settle: None,
            tools_offered: true,
        });
        let mut current =
            tcode_core::project::SessionMeta::new(status.provider, std::env::temp_dir(), None);
        current.id = status.session_id.clone();
        current.title = "Current".into();
        let mut other = current.clone();
        other.id = "release-thread".into();
        other.title = "Release notes".into();
        replace(
            &incoming,
            Topic::Index,
            ServerEvent::IndexSnapshot(tcode_protocol::IndexSnapshot {
                summary: Default::default(),
                sessions: vec![current, other],
                projects: Vec::new(),
            }),
        );
        replace(
            &incoming,
            Topic::SessionStatus {
                session_id: status.session_id.clone(),
            },
            ServerEvent::SessionStatusReplaced(Box::new(status)),
        );
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new(store.clone(), session_id.clone(), window, cx)
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            composer.update(cx, |composer, cx| composer.focus(window, cx));
        });
        cx.simulate_input("@release");
        // The menu has folded open by the time it is clicked.
        crate::test_frames::draw_settled(cx);
        composer.read_with(cx, |composer, cx| {
            assert_eq!(composer.menu_rows(cx).0[0].primary, "Release notes");
        });
        let row = cx.debug_bounds("menu-row-0").expect("thread row");
        cx.simulate_click(row.center(), Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        cx.run_until_parked();
        assert!(
            std::iter::from_fn(|| outgoing.try_recv().ok()).any(|line| matches!(
                tcode_protocol::decode_client_line(&line).unwrap().payload,
                tcode_protocol::ClientPayload::Command(
                    tcode_protocol::Command::AddThreadReference { thread_id, .. }
                ) if thread_id == "release-thread"
            )),
            "no AddThreadReference was sent"
        );
    }
}
