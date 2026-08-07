//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel (see docs/DESIGN.md "Composer").

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{
    ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalOptionKind, ApprovalRequest,
    FileChangeKind, InteractionMode, ModelSpec, OptionDescriptor, ProviderCommand,
    ProviderCommandKind, ProviderKind, TokenUsage, UserInputQuestion,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone as _};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, ClipboardEntry, Context, Entity,
    EventEmitter, ExternalPaths, Focusable as _, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, PathBuilder, Pixels, Render, Role, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, Window, canvas, div, img, point, prelude::FluentBuilder as _,
    px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName, Sizable as _,
    StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::PopoverState,
    spinner::Spinner,
    v_flex,
};

use crate::attachments::attach_error_message;
use crate::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::context_meter;
use crate::palette::fuzzy_score;
use crate::provider_card::{CLAUDE_BRAND_COLOR, provider_glyph};
use crate::settings::provider_label;
use crate::shortcut::format_secondary_shortcut;
use crate::store::WorkspaceStore;
use crate::workspace_walk::filter_entries;
use tcode_core::attachments::validate_attachment;
use tcode_core::ui::{ConversationDestination, WorkspaceMode};
use tcode_protocol::{Command, PathEntry};
use tcode_runtime::app::mime_from_path;

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

/// One selectable model in the picker (a catalog [`ModelSpec`] row).
#[derive(Clone)]
struct ModelRow {
    /// Provider-native model id (the favorites key + selection value), or the
    /// ACP agent's registry id when `acp` is set.
    id: String,
    /// Display name.
    name: String,
    provider: ProviderKind,
    /// Which provider profile this row belongs to (`None` = the built-in profile
    /// for `provider`). Lets a native provider expose several profiles — e.g.
    /// official Claude and a third-party endpoint — in one rail.
    profile_id: Option<String>,
    /// This row starts a session with an installed ACP agent rather than
    /// selecting a model (ACP agents own their model list).
    acp: bool,
}

/// Queue-strip rows are single-line: collapse whitespace and clip long messages
/// (the full text is still what gets sent).
fn truncate_queued(text: &str) -> String {
    const MAX: usize = 80;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX {
        return normalized;
    }
    let clipped: String = normalized.chars().take(MAX).collect();
    format!("{clipped}…")
}

/// The provider glyph tinted with the accent configured on its Settings →
/// Providers card, falling back to the provider's own brand tint.
fn tinted_provider_glyph(provider: ProviderKind, store: &WorkspaceStore) -> Icon {
    let glyph = provider_glyph(provider);
    let profile_id = tcode_core::settings::Settings::builtin_profile_id(provider);
    match store.provider_profile_accent(profile_id) {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}

/// A profile's rail glyph: the kind's glyph tinted with the profile's own accent
/// (so a third-party profile can be told apart from the built-in at a glance).
fn tinted_profile_glyph(profile_id: &str, store: &WorkspaceStore) -> Icon {
    let glyph = provider_glyph(store.provider_profile_kind(profile_id));
    match store.provider_profile_accent(profile_id) {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}

/// The three approval modes in display order, each with its label, one-line
/// description (exact UI copy), and chip icon (lock → pencil → unlock).
const APPROVAL_MODES: [(ApprovalMode, &str, &str, &str); 3] = [
    (
        ApprovalMode::Supervised,
        "approval.supervised",
        "approval.supervised_description",
        "icons/lock.svg",
    ),
    (
        ApprovalMode::AutoAcceptEdits,
        "approval.auto_edits",
        "approval.auto_edits_description",
        "icons/pencil.svg",
    ),
    (
        ApprovalMode::FullAccess,
        "approval.full_access",
        "approval.full_access_description",
        "icons/unlock.svg",
    ),
];

fn approval_mode_meta(mode: ApprovalMode) -> (String, &'static str) {
    // ReadOnly is dispatch-only for now. A selected child still needs a stable
    // chip, but it must not add a fourth choice to the user-facing picker.
    let mode = match mode {
        ApprovalMode::ReadOnly => ApprovalMode::Supervised,
        mode => mode,
    };
    let (_, label_key, _, icon) = APPROVAL_MODES
        .iter()
        .find(|(m, ..)| *m == mode)
        .expect("every ApprovalMode is present in APPROVAL_MODES");
    (crate::tr!(*label_key).into_owned(), icon)
}

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

/// The minimal `/`-command set this slice handles (S1 §7).
enum SlashCommand {
    Plan,
    Default,
    Model,
}

/// Strip a leading `/orchestrate` command token while rejecting lookalikes
/// such as `/orchestrated`.
fn strip_orchestrate_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/orchestrate")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// User-facing validation failures for a syntactically recognized `/later`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaterError {
    MissingTime,
    MissingMessage,
    InvalidTime,
}

/// Parse a scheduled-message command against an injectable local clock. A
/// non-command returns `None`, keeping provider commands and lookalikes on the
/// ordinary submit path; recognized but malformed commands return an error.
fn parse_later(text: &str, now: DateTime<Local>) -> Option<Result<(u64, String), LaterError>> {
    let text = text.trim();
    let rest = text.strip_prefix("/later")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Some(Err(LaterError::MissingTime));
    }
    let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let time_spec = &rest[..split];
    let message = rest[split..].trim();
    if message.is_empty() {
        return Some(Err(LaterError::MissingMessage));
    }

    let fire_at = if let Some((hour, minute)) = parse_wall_clock(time_spec) {
        let today = local_occurrence(now.date_naive(), hour, minute);
        let candidate = today.filter(|candidate| *candidate > now).or_else(|| {
            now.date_naive()
                .succ_opt()
                .and_then(|date| local_occurrence(date, hour, minute))
        });
        candidate.ok_or(LaterError::InvalidTime)
    } else {
        parse_relative(time_spec)
            .and_then(|duration| now.checked_add_signed(duration))
            .ok_or(LaterError::InvalidTime)
    };
    Some(fire_at.and_then(|fire_at| {
        u64::try_from(fire_at.timestamp())
            .map(|timestamp| (timestamp, message.to_string()))
            .map_err(|_| LaterError::InvalidTime)
    }))
}

fn parse_wall_clock(spec: &str) -> Option<(u32, u32)> {
    let (hour, minute) = spec.split_once(':')?;
    if hour.is_empty()
        || hour.len() > 2
        || minute.len() != 2
        || !hour.bytes().all(|byte| byte.is_ascii_digit())
        || !minute.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hour = hour.parse().ok()?;
    let minute = minute.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

fn local_occurrence(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .earliest()
}

fn parse_relative(spec: &str) -> Option<chrono::Duration> {
    let (digits, unit_seconds) = if let Some(digits) = spec.strip_suffix("min") {
        (digits, 60_i64)
    } else if let Some(digits) = spec.strip_suffix('s') {
        (digits, 1_i64)
    } else {
        (spec.strip_suffix('h')?, 3_600_i64)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let count: i64 = digits.parse().ok()?;
    if count < 1 {
        return None;
    }
    count
        .checked_mul(unit_seconds)
        .map(chrono::Duration::seconds)
}

/// Compact queue-strip countdown: hours retain an hour column, shorter waits
/// use minutes and seconds.
fn format_countdown(secs: u64) -> String {
    let hours = secs / 3_600;
    let minutes = (secs % 3_600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Recognize a standalone `/plan`, `/default`, or `/model` message (T3 strips
/// the command and switches mode / opens the picker instead of sending it).
fn slash_command(text: &str) -> Option<SlashCommand> {
    match text.trim() {
        "/plan" => Some(SlashCommand::Plan),
        "/default" => Some(SlashCommand::Default),
        "/model" => Some(SlashCommand::Model),
        _ => None,
    }
}

/// The stable destination for unsent composer text. Draft session ids are
/// deliberately excluded: opening a project's New thread page allocates a new
/// transient session id each time.
pub(crate) type ComposerDestination = ConversationDestination;

pub(crate) fn composer_destination(
    is_draft: bool,
    session_id: &str,
    project_id: Option<&str>,
) -> Option<ComposerDestination> {
    if is_draft {
        project_id.map(|id| ComposerDestination::ProjectDraft(id.to_string()))
    } else {
        Some(ComposerDestination::Thread(session_id.to_string()))
    }
}

/// In-memory text drafts plus the destination currently represented by the
/// shared [`InputState`]. Keeping switching in this small state machine makes
/// it explicit that the outgoing value is saved before the incoming one is
/// restored.
#[derive(Default)]
struct ComposerTextCache {
    current: Option<ComposerDestination>,
    drafts: HashMap<ComposerDestination, String>,
}

impl ComposerTextCache {
    /// Change destinations, returning the text that should replace the shared
    /// input. `None` means the input already represents this destination.
    fn switch_to(
        &mut self,
        incoming: Option<ComposerDestination>,
        outgoing_text: &str,
    ) -> Option<String> {
        if self.current == incoming {
            return None;
        }

        if let Some(outgoing) = self.current.take() {
            if outgoing_text.is_empty() {
                self.drafts.remove(&outgoing);
            } else {
                self.drafts.insert(outgoing, outgoing_text.to_string());
            }
        }

        self.current = incoming.clone();
        Some(
            incoming
                .and_then(|key| self.drafts.get(&key).cloned())
                .unwrap_or_default(),
        )
    }

    fn clear_current(&mut self) {
        if let Some(current) = self.current.as_ref() {
            self.drafts.remove(current);
        }
    }
}

/// A pending image attachment: validated, persisted to the session attachments
/// dir, and shown in the composer thumbnail strip. Kept per active session.
#[derive(Clone)]
struct PendingImage {
    /// On-disk path of the persisted copy (also the thumbnail image source).
    path: PathBuf,
    /// Display name.
    name: String,
}

enum PendingImageSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

enum AddImageError {
    Attachment(tcode_core::attachments::AttachError),
    Persist(std::io::Error),
}

/// Which glyph a trigger-menu row shows.
#[derive(Clone, Copy)]
enum MenuIcon {
    File,
    Folder,
    Command,
    /// Skill rows (`$`), populated from the provider's skills feed.
    Skill,
}

/// What accepting a trigger-menu row does.
#[derive(Clone)]
enum MenuAccept {
    /// Insert the serialized `[basename](path)` mention for this relative path.
    InsertPath(String),
    /// Insert `$<name> ` for this provider skill.
    InsertSkill(String),
    /// Insert `/<name> ` for this provider slash command.
    InsertCommand(String),
    /// Insert the native orchestration command without consuming following text.
    InsertOrchestrate,
    /// Insert the scheduled-message command prefix.
    InsertLater,
    /// Strip the `/model` command and open the model picker.
    OpenModelPicker,
    /// Strip the command and switch interaction mode.
    SetMode(InteractionMode),
}

/// One selectable row in a trigger (`@`/`/`/`$`) menu.
#[derive(Clone)]
struct MenuRow {
    /// Bold primary text (basename / command label).
    primary: String,
    /// Muted secondary text (parent path / description).
    secondary: String,
    icon: MenuIcon,
    accept: MenuAccept,
    /// The group this row belongs to (T3 §5: the `/` menu is grouped
    /// `Built-in` / `Provider`; the `$` menu is a single `Skills` group). A
    /// header is rendered above the first row of each group. `None` = ungrouped
    /// (the `@` file menu).
    group: Option<&'static str>,
}

/// Select and rank one provider-command kind with the same fuzzy subsequence
/// matcher as the command palette. Empty queries preserve provider order;
/// non-empty queries put the strongest matches first and never truncate.
fn filter_provider_commands<'a>(
    commands: &'a [ProviderCommand],
    kind: ProviderCommandKind,
    query: &str,
) -> Vec<&'a ProviderCommand> {
    let mut matched: Vec<(i32, usize, &ProviderCommand)> = commands
        .iter()
        .enumerate()
        .filter(|(_, command)| command.kind == kind)
        .filter_map(|(index, command)| {
            fuzzy_score(query, &command.name).map(|score| (score, index, command))
        })
        .collect();
    if !query.is_empty() {
        matched.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    matched.into_iter().map(|(_, _, command)| command).collect()
}

/// Guess a file extension for a persisted attachment from its MIME type,
/// falling back to the source name's extension, then `png`.
fn image_extension(mime: &str, name: &str) -> String {
    let from_mime = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "",
    };
    if !from_mime.is_empty() {
        return from_mime.to_string();
    }
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string())
}

fn is_wire_ready_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn transcode_image_to_png(bytes: &[u8]) -> image::ImageResult<Vec<u8>> {
    let image = image::load_from_memory(bytes)?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    Ok(png.into_inner())
}

pub struct Composer {
    workspace_store: Entity<WorkspaceStore>,
    input: Entity<InputState>,
    /// Dedicated free-form answer field shown inside an agent question card.
    /// Keeping it separate from the turn composer makes the pending question
    /// and the destination of typed text unambiguous.
    user_input_custom: Entity<InputState>,
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
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(crate::tr!("composer.placeholder"))
        });
        let model_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!("composer.search_models"))
        });
        let user_input_custom = cx.new(|cx| {
            InputState::new(window, cx)
                .submit_on_enter(true)
                .placeholder(crate::tr!("userinput.custom_placeholder"))
        });

        let subscriptions = vec![
            // Re-render when app state changes (e.g. the provider's commands /
            // skills feed arrives after session start, feeding the `/`+`$` menus).
            cx.observe(&workspace_store, |_, _, cx| cx.notify()),
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
        let cursor = prefill.len();
        self.input.update(cx, |state, cx| {
            state.set_value(prefill, window, cx);
            state.set_selected_range(cursor..cursor, cx);
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
                .composer_terminal_contexts()
                .is_empty()
            || !self.workspace_store.read(cx).review_comments().is_empty()
    }

    /// Send the composer's contents. `steer` is set by the ⌘/Ctrl+Enter gesture:
    /// inject into the running turn rather than queue behind it. It is a no-op
    /// when no turn is running (steering just sends), and degrades to
    /// queueing (with a notice) on providers that cannot steer.
    fn submit(
        &mut self,
        input: &Entity<InputState>,
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
        let terminal_contexts = self.workspace_store.read(cx).composer_terminal_contexts();
        if !self.has_sendable_content(cx) {
            return;
        }
        if !self.workspace_store.read(cx).composer_has_active_session() {
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
                store.dispatch(Command::ScheduleTurn {
                    text: message,
                    attachment_paths,
                    fire_at_unix_secs,
                });
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
                SlashCommand::Plan => self.workspace_store.update(cx, |store, _cx| {
                    store.dispatch(Command::SetInteractionMode {
                        mode: InteractionMode::Plan,
                    })
                }),
                SlashCommand::Default => self.workspace_store.update(cx, |store, _cx| {
                    store.dispatch(Command::SetInteractionMode {
                        mode: InteractionMode::Build,
                    })
                }),
                SlashCommand::Model => {
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
        if let Some((from, to)) = self.workspace_store.read(cx).composer_relay_confirmation() {
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
                        DialogButtonProps::default()
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
        input: &Entity<InputState>,
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
            let command = if relay {
                Command::ConfirmRelayAndSend {
                    text: sent_text,
                    attachment_paths,
                }
            } else if orchestrate {
                Command::OrchestrateTurn {
                    text: sent_text,
                    attachment_paths,
                }
            } else if steer {
                Command::Steer {
                    text: sent_text,
                    attachment_paths,
                }
            } else {
                Command::SendTurn {
                    text: sent_text,
                    attachment_paths,
                }
            };
            store.dispatch(command);
        });
        cx.emit(ComposerEvent::Submitted);
        cx.notify();
    }

    // -- inline triggers (@ mentions / $ skills / commands) ----------------

    /// Whether a trigger menu should currently be shown.
    fn menu_visible(&self) -> bool {
        self.active_trigger.is_some() && !self.menu_dismissed
    }

    /// Recompute the active trigger from the input text + cursor, resetting the
    /// highlight (and un-dismissing) when the trigger identity changes, and
    /// lazily loading the workspace listing for `@`-mentions.
    fn recompute_trigger(&mut self, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let trigger = detect_composer_trigger(&text, cursor);
        let key = trigger
            .as_ref()
            .map(|t| format!("{:?}\u{1}{}", t.kind, t.query));
        if key != self.menu_last_key {
            self.menu_highlight = 0;
            self.menu_dismissed = false;
            self.menu_last_key = key;
        }
        if matches!(trigger.as_ref().map(|t| t.kind), Some(TriggerKind::Path)) {
            self.ensure_workspace(cx);
        }
        self.active_trigger = trigger;
    }

    /// Load the workspace file/folder listing for the active session cwd in the
    /// background (gitignore-respected), the first time a mention menu opens.
    fn ensure_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.workspace_store.read(cx).composer_active_cwd() else {
            return;
        };
        if self.workspace_loading || self.workspace.as_ref().is_some_and(|(c, _)| *c == cwd) {
            return;
        }
        self.workspace_loading = true;
        let store = self.workspace_store.clone();
        let walked = store.update(cx, |store, cx| store.list_active_workspace(cx));
        cx.spawn(async move |this, cx| {
            let walked = walked.await;
            let _ = this.update(cx, |this, cx| {
                this.workspace = Some((cwd, walked));
                this.workspace_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Build the rows for the currently active trigger menu, plus its empty-state
    /// copy and whether it is still loading.
    fn menu_rows(&self, cx: &App) -> (Vec<MenuRow>, String, bool) {
        let Some(trigger) = self.active_trigger.as_ref() else {
            return (Vec::new(), String::new(), false);
        };
        match trigger.kind {
            TriggerKind::Path => {
                let entries = self
                    .workspace
                    .as_ref()
                    .map(|(_, e)| e.as_slice())
                    .unwrap_or(&[]);
                let rows = filter_entries(entries, &trigger.query, FILE_MENU_ROW_CAP)
                    .into_iter()
                    .map(|e| MenuRow {
                        primary: e.basename.clone(),
                        secondary: e.parent.clone(),
                        icon: if e.is_dir {
                            MenuIcon::Folder
                        } else {
                            MenuIcon::File
                        },
                        accept: MenuAccept::InsertPath(e.rel_path.clone()),
                        group: None,
                    })
                    .collect();
                let loading = self.workspace_loading && self.workspace.is_none();
                (rows, crate::tr!("composer.no_files").into_owned(), loading)
            }
            TriggerKind::Skill => {
                // Provider-native skills (Claude `skills` / Codex `skills/list`),
                // fuzzily filtered by the `$` query with no item cap.
                let commands = self.workspace_store.read(cx).composer_provider_commands();
                let rows =
                    filter_provider_commands(&commands, ProviderCommandKind::Skill, &trigger.query)
                        .into_iter()
                        .map(|c| MenuRow {
                            primary: format!("${}", c.name),
                            secondary: c
                                .description
                                .clone()
                                .unwrap_or_else(|| crate::tr!("composer.run_skill").into_owned()),
                            icon: MenuIcon::Skill,
                            accept: MenuAccept::InsertSkill(c.name.clone()),
                            group: Some("composer.group_skills"),
                        })
                        .collect();
                (rows, crate::tr!("composer.no_skills").into_owned(), false)
            }
            TriggerKind::SlashCommand | TriggerKind::SlashModel => {
                let builtins: [(&str, Option<&str>, &str, MenuAccept); 5] = [
                    (
                        "model",
                        None,
                        "composer.cmd_model_desc",
                        MenuAccept::OpenModelPicker,
                    ),
                    (
                        "plan",
                        None,
                        "composer.cmd_plan_desc",
                        MenuAccept::SetMode(InteractionMode::Plan),
                    ),
                    (
                        "default",
                        None,
                        "composer.cmd_default_desc",
                        MenuAccept::SetMode(InteractionMode::Build),
                    ),
                    (
                        "orchestrate",
                        Some("composer.cmd_orchestrate_label"),
                        "composer.cmd_orchestrate_desc",
                        MenuAccept::InsertOrchestrate,
                    ),
                    (
                        "later",
                        None,
                        "composer.cmd_later_desc",
                        MenuAccept::InsertLater,
                    ),
                ];
                let mut rows: Vec<MenuRow> = builtins
                    .into_iter()
                    .filter(|(name, _, _, _)| fuzzy_score(&trigger.query, name).is_some())
                    .map(|(name, label, desc, accept)| MenuRow {
                        primary: format!(
                            "/{}",
                            label
                                .map(|key| crate::tr!(key).into_owned())
                                .unwrap_or_else(|| name.to_string())
                        ),
                        secondary: crate::tr!(desc).into_owned(),
                        icon: MenuIcon::Command,
                        accept,
                        group: Some("composer.group_builtin"),
                    })
                    .collect();
                // Provider-native slash commands (Claude `slash_commands`), shown
                // after the built-in group, fuzzily filtered without truncation.
                let commands = self.workspace_store.read(cx).composer_provider_commands();
                rows.extend(
                    filter_provider_commands(
                        &commands,
                        ProviderCommandKind::Command,
                        &trigger.query,
                    )
                    .into_iter()
                    .map(|c| MenuRow {
                        primary: format!("/{}", c.name),
                        secondary: c.description.clone().unwrap_or_else(|| {
                            crate::tr!("composer.run_provider_command").into_owned()
                        }),
                        icon: MenuIcon::Command,
                        accept: MenuAccept::InsertCommand(c.name.clone()),
                        group: Some("composer.group_provider"),
                    }),
                );
                (rows, crate::tr!("composer.no_command").into_owned(), false)
            }
        }
    }

    /// Replace the active trigger's text range in the input with `replacement`.
    fn replace_trigger(&mut self, replacement: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(trigger) = self.active_trigger.clone() else {
            return;
        };
        let replacement = replacement.to_string();
        self.input.update(cx, |state, cx| {
            state.set_selected_range(trigger.range.clone(), cx);
            state.replace(replacement.clone(), window, cx);
        });
    }

    /// Accept the trigger-menu row at `index`.
    fn accept_menu(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (rows, _, _) = self.menu_rows(cx);
        let Some(row) = rows.get(index).cloned() else {
            return;
        };
        match &row.accept {
            MenuAccept::InsertPath(path) => {
                let link = format!("{} ", serialize_composer_file_link(path));
                self.replace_trigger(&link, window, cx);
            }
            MenuAccept::InsertSkill(name) => self.replace_trigger(&format!("${name} "), window, cx),
            MenuAccept::InsertCommand(name) => {
                self.replace_trigger(&format!("/{name} "), window, cx)
            }
            MenuAccept::InsertOrchestrate => self.replace_trigger("/orchestrate ", window, cx),
            MenuAccept::InsertLater => self.replace_trigger("/later ", window, cx),
            MenuAccept::OpenModelPicker => {
                self.replace_trigger("", window, cx);
                self.model_picker_token = self.model_picker_token.wrapping_add(1);
            }
            MenuAccept::SetMode(mode) => {
                let mode = *mode;
                self.replace_trigger("", window, cx);
                self.workspace_store.update(cx, |store, _cx| {
                    store.dispatch(Command::SetInteractionMode { mode })
                });
            }
        }
        self.active_trigger = None;
        self.menu_dismissed = true;
        cx.notify();
    }

    // -- image attachments --------------------------------------------------

    /// Reset the thumbnail strip when the active session changes (its pending
    /// images belong to a specific session).
    fn sync_images_session(&mut self, cx: &mut Context<Self>) {
        let id = self.workspace_store.read(cx).active_session_id();
        if id != self.images_session {
            self.images_session = id;
            self.pending_images.clear();
            self.image_load_generation = self.image_load_generation.wrapping_add(1);
            self.pending_image_loads = 0;
        }
    }

    /// Validate, decode/transcode, and persist an image off the main thread.
    /// Completion appends only while the same session still owns the strip.
    fn add_image(
        &mut self,
        name: String,
        mime: String,
        source: PendingImageSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.sync_images_session(cx);
        let initial_size = match &source {
            PendingImageSource::Bytes(bytes) => bytes.len() as u64,
            PendingImageSource::Path(_) => 0,
        };
        if let Err(err) = validate_attachment(
            &name,
            &mime,
            initial_size,
            self.pending_images.len() + self.pending_image_loads,
        ) {
            window.push_notification(Notification::error(attach_error_message(&err)), cx);
            return false;
        }
        let session_id = self.workspace_store.read(cx).active_session_id();
        let attachments_dir = self.workspace_store.read(cx).composer_attachments_dir();
        let current_count = self.pending_images.len() + self.pending_image_loads;
        self.pending_image_loads += 1;
        let generation = self.image_load_generation;
        let result_name = name.clone();
        let workspace_store = self.workspace_store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let bytes = match source {
                        PendingImageSource::Bytes(bytes) => bytes,
                        PendingImageSource::Path(path) => {
                            let size = std::fs::metadata(&path)
                                .map_err(AddImageError::Persist)?
                                .len();
                            validate_attachment(&name, &mime, size, current_count)
                                .map_err(AddImageError::Attachment)?;
                            std::fs::read(path).map_err(AddImageError::Persist)?
                        }
                    };
                    validate_attachment(&name, &mime, bytes.len() as u64, current_count)
                        .map_err(AddImageError::Attachment)?;
                    let (mime, bytes) = if is_wire_ready_image(&mime) {
                        (mime, bytes)
                    } else {
                        let bytes = transcode_image_to_png(&bytes).map_err(|_| {
                            AddImageError::Attachment(
                                tcode_core::attachments::AttachError::UnsupportedType {
                                    name: name.clone(),
                                },
                            )
                        })?;
                        ("image/png".to_string(), bytes)
                    };
                    let dir = attachments_dir.ok_or_else(|| {
                        AddImageError::Persist(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no active session",
                        ))
                    })?;
                    let ext = image_extension(&mime, &name);
                    Ok::<_, AddImageError>((dir, bytes, ext))
                })
                .await;
            let result = match prepared {
                Ok((dir, bytes, ext)) => workspace_store
                    .update(cx, |store, cx| {
                        store.save_attachment_to_dir(dir, bytes, ext, cx)
                    })
                    .await
                    .map_err(AddImageError::Persist),
                Err(error) => Err(error),
            };
            let _ = this.update_in(cx, |composer, window, cx| {
                if composer.image_load_generation != generation
                    || composer.images_session != session_id
                    || composer.workspace_store.read(cx).active_session_id() != session_id
                {
                    return;
                }
                composer.pending_image_loads = composer.pending_image_loads.saturating_sub(1);
                match result {
                    Ok(path) => {
                        composer.pending_images.push(PendingImage {
                            path,
                            name: result_name,
                        });
                        cx.notify();
                    }
                    Err(AddImageError::Attachment(err)) => window
                        .push_notification(Notification::error(attach_error_message(&err)), cx),
                    Err(AddImageError::Persist(err)) => window.push_notification(
                        Notification::error(
                            crate::tr!("errors.persist_event", error = err).into_owned(),
                        ),
                        cx,
                    ),
                }
            });
        })
        .detach();
        true
    }

    fn add_image_bytes(
        &mut self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.add_image(name, mime, PendingImageSource::Bytes(bytes), window, cx)
    }

    /// Add an image from a dropped file path.
    fn add_image_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let mime = mime_from_path(&path);
        self.add_image(name, mime, PendingImageSource::Path(path), window, cx)
    }

    /// Pull an image off the clipboard (⌘V with image content), if present.
    fn paste_clipboard_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut accepted_image = false;
        if let Some(item) = cx.read_from_clipboard() {
            for entry in &item.entries {
                match entry {
                    ClipboardEntry::Image(image) => {
                        let mime = image.format().mime_type().to_string();
                        let bytes = image.bytes().to_vec();
                        accepted_image |= self.add_image_bytes(
                            "pasted-image".to_string(),
                            mime,
                            bytes,
                            window,
                            cx,
                        );
                    }
                    ClipboardEntry::ExternalPaths(paths) => {
                        for path in paths.paths() {
                            if mime_from_path(path).starts_with("image/") {
                                accepted_image |= self.add_image_path(path.clone(), window, cx);
                            }
                        }
                    }
                    ClipboardEntry::String(_) => {}
                }
            }
        }
        if !accepted_image && let Some((mime, bytes)) = crate::pasteboard::read_pasteboard_image() {
            self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
        }
    }

    fn remove_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_images.len() {
            let removed = self.pending_images.remove(index);
            self.workspace_store
                .update(cx, |store, cx| store.remove_user_file(removed.path, cx))
                .detach();
            cx.notify();
        }
    }

    /// The active session's pending user-input request, if any.
    fn pending_user_input(&self, cx: &App) -> Option<(String, Vec<UserInputQuestion>)> {
        self.workspace_store.read(cx).composer_pending_user_input()
    }

    /// The rail the picker shows: an explicit user choice, else Favorites when
    /// any favorites exist (S1 §2), else the active session's profile.
    fn rail_for(
        &self,
        provider: ProviderKind,
        agent_id: Option<&str>,
        profile_id: Option<&str>,
        has_favorites: bool,
    ) -> PickerRail {
        if let Some(rail) = self.picker_rail.clone() {
            return rail;
        }
        match (provider, agent_id) {
            (ProviderKind::Acp, Some(id)) => PickerRail::Acp(id.to_string()),
            _ if has_favorites => PickerRail::Favorites,
            // A native session's rail is its selected profile, defaulting to the
            // built-in profile for the provider.
            _ => PickerRail::Profile(profile_id.map(str::to_string).unwrap_or_else(|| {
                tcode_core::settings::Settings::builtin_profile_id(provider).to_string()
            })),
        }
    }

    // -- control-row popovers ----------------------------------------------

    /// The model-picker button + popover (anchored above, ~360px).
    fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let Some(active_model) = store.composer_active_model() else {
            return div().into_any_element();
        };
        let provider = active_model.provider;
        let current_model = active_model.model;
        let acp_agent_id = active_model.acp_agent_id;
        let active_profile = active_model.profile_id;
        let catalog = store.provider_model_catalog(provider);
        // The picker honors the provider card's Models section: hidden models
        // are gone, custom slugs are present, and the persisted order (plus
        // favorites-first) decides the sequence. When a third-party profile is
        // active, resolve against *its* card so its custom models are named.
        let resolved = match &active_profile {
            Some(id) => store.picker_models_for_profile(id),
            None => store.composer_picker_models(provider),
        };
        let display = current_model_name_resolved(&resolved, &catalog, current_model.as_deref());

        // Build the filtered row list for the current frame. Favorites open
        // first when any exist (S1 §2).
        let query = self.model_search.read(cx).value().to_lowercase();
        let has_favorites = PICKER_PROVIDER_KINDS
            .into_iter()
            .flat_map(|p| store.composer_picker_models(p))
            .any(|m| m.favorite);
        let rail = self.rail_for(
            provider,
            acp_agent_id.as_deref(),
            active_profile.as_deref(),
            has_favorites,
        );
        let all_rows: Vec<ModelRow> = match &rail {
            PickerRail::Favorites => PICKER_PROVIDER_KINDS
                .into_iter()
                .flat_map(|p| {
                    store
                        .composer_picker_models(p)
                        .into_iter()
                        .filter(|m| m.favorite)
                        .map(move |m| ModelRow {
                            id: m.id,
                            name: m.name,
                            provider: p,
                            profile_id: None,
                            acp: false,
                        })
                })
                .collect(),
            // Each profile is its own rail and lists only its own models: the
            // built-in profiles show the official catalog; a third-party profile
            // (e.g. Klaude Kode → Kimi) shows only the models added to its card.
            PickerRail::Profile(id) => {
                let kind = store.provider_profile_kind(id);
                let is_builtin = tcode_core::settings::Settings::is_builtin_profile_id(id);
                let profile_id = id.clone();
                store
                    .picker_models_for_profile(id)
                    .into_iter()
                    .map(move |m| ModelRow {
                        id: m.id,
                        name: m.name,
                        provider: kind,
                        profile_id: (!is_builtin).then(|| profile_id.clone()),
                        acp: false,
                    })
                    .collect()
            }
            // One row: "use this agent". Its models arrive as ProviderOptions
            // once the session starts and render in the traits picker.
            PickerRail::Acp(id) => store
                .installed_acp_agent(id)
                .into_iter()
                .map(|agent| ModelRow {
                    id: agent.id,
                    name: agent.name,
                    provider: ProviderKind::Acp,
                    profile_id: None,
                    acp: true,
                })
                .collect(),
        };
        let rows: Vec<ModelRow> = all_rows
            .into_iter()
            .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
            .collect();
        // Only the built-in profiles have a probed catalog that can still be
        // loading; a third-party profile shows its own slugs immediately.
        let loading = store.composer_models_loading(provider)
            && matches!(&rail, PickerRail::Profile(id) if tcode_core::settings::Settings::is_builtin_profile_id(id))
            && rows.is_empty()
            && query.is_empty();

        let composer = cx.entity();
        let store_entity = self.workspace_store.clone();
        let model_search = self.model_search.clone();
        let pending_restart = store.composer_model_pending_restart();
        // On an ACP rail the "selected" row is the agent itself.
        let selected = match provider {
            ProviderKind::Acp => acp_agent_id.clone(),
            _ => current_model.clone(),
        };
        let acp_rail_agents: Vec<(String, String)> = store
            .settings_installed_acp_agents()
            .into_iter()
            .filter(|agent| agent.enabled)
            .map(|agent| (agent.id.clone(), agent.name.clone()))
            .collect();

        let trigger = Button::new("model-picker").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .child(tinted_provider_glyph(provider, store).small())
                .child(div().font_medium().child(display))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .xsmall()
                        .text_color(cx.theme().muted_foreground),
                ),
        );

        crate::material::overlay_popover(("model-picker-popover", self.model_picker_token))
            .anchor(Anchor::BottomLeft)
            .default_open(self.model_picker_token > 0)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let rows = rows.clone();
                let store_entity = store_entity.clone();
                let model_search = model_search.clone();
                let composer = composer.clone();
                let selected = selected.clone();
                let popover = cx.entity();
                let rail = rail.clone();
                let acp_rail_agents = acp_rail_agents.clone();
                render_model_pane(
                    &rows,
                    &selected,
                    rail,
                    &acp_rail_agents,
                    pending_restart,
                    loading,
                    &store_entity,
                    &model_search,
                    &composer,
                    &popover,
                    cx,
                )
            })
            .into_any_element()
    }

    /// The traits chip ("High · 200k") + descriptor popover. Empty element when
    /// the current model has no descriptors.
    fn render_traits_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.read(cx);
        // Native providers describe their options through the model catalog; ACP
        // agents push theirs over the wire (`AgentEvent::ProviderOptions`). Both
        // arrive as `OptionDescriptor`s and render through this one picker.
        let descriptors = store.composer_active_option_descriptors();
        if descriptors.is_empty() {
            return div().into_any_element();
        }
        let spec = match store.composer_active_model_spec() {
            Some(spec) => spec,
            None => ModelSpec {
                id: String::new(),
                display_name: String::new(),
                is_default: false,
                options: descriptors,
            },
        };
        let selections = store.composer_active_option_selections();
        let ultrathink_armed = store.composer_ultrathink_armed();
        let Some(label) = traits_chip_label(&spec, &selections, ultrathink_armed) else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        // The reasoning section is locked while the prompt text itself contains
        // "ultrathink" (T3).
        let locked = self
            .input
            .read(cx)
            .value()
            .to_lowercase()
            .contains("ultrathink");
        let pending_restart = store.composer_options_pending_restart();

        let trigger = Button::new("traits-chip").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let store_entity = self.workspace_store.clone();
        crate::material::overlay_popover("traits-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_traits_pane(
                    &spec,
                    &selections,
                    ultrathink_armed,
                    locked,
                    pending_restart,
                    &store_entity,
                    &cx.entity(),
                    cx,
                )
            })
            .into_any_element()
    }

    /// The Build/Plan interaction-mode chip (S1 §4).
    fn render_mode_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.workspace_store.read(cx).composer_interaction_mode();
        let muted = cx.theme().muted_foreground;
        let (icon, label, tooltip) = match mode {
            InteractionMode::Build => (
                "icons/box.svg",
                crate::tr!("composer.build"),
                crate::tr!("composer.build_tooltip"),
            ),
            InteractionMode::Plan => (
                "icons/ruler.svg",
                crate::tr!("composer.plan"),
                crate::tr!("composer.plan_tooltip"),
            ),
        };
        Button::new("mode-chip")
            .ghost()
            .compact()
            .tooltip(tooltip)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(Icon::empty().path(icon).small().text_color(muted))
                    .child(label),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.workspace_store.update(cx, |store, _cx| {
                    store.dispatch(Command::ToggleInteractionMode)
                });
            }))
            .into_any_element()
    }

    /// The circular context-window meter (ring showing used%, red > 90%) + a
    /// hover/click popover (T3's `ContextWindowMeter`).
    fn render_context_meter(&self, cx: &mut Context<Self>) -> AnyElement {
        let (usage, provider) = self.workspace_store.read(cx).composer_context();
        let pct = usage.and_then(|u| context_meter::used_percentage(&u));
        let overloaded = pct.map(context_meter::is_overloaded).unwrap_or(false);
        let ring_color: Hsla = if overloaded {
            rgb(METER_RED).into()
        } else {
            rgb(METER_BLUE).into()
        };
        let mut track = cx.theme().muted_foreground;
        track.a = 0.35;

        let trigger = Button::new("context-meter").ghost().compact().child(
            div()
                .size(px(16.))
                .child(ring_canvas(pct.unwrap_or(0.0), ring_color, track)),
        );

        crate::material::overlay_popover("context-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| render_context_meter_pane(usage, provider, pct, cx))
            .into_any_element()
    }

    /// The approval-mode selector: a chip showing the current mode (icon +
    /// label) opening a popover of the three modes (icon + bold name + muted
    /// description, ✓ on the current one).
    fn render_permission_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.workspace_store.read(cx).composer_approval_mode();
        let native_approval_modes_enabled = self
            .workspace_store
            .read(cx)
            .composer_native_approval_modes_enabled();
        let (label, icon_path) = approval_mode_meta(current);
        let muted = cx.theme().muted_foreground;

        let trigger = Button::new("permission-chip").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(Icon::empty().path(icon_path).small().text_color(muted))
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let store_entity = self.workspace_store.clone();
        let pending_restart = self
            .workspace_store
            .read(cx)
            .composer_approval_pending_restart();
        crate::material::overlay_popover("permission-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_permission_pane(
                    current,
                    pending_restart,
                    native_approval_modes_enabled,
                    &store_entity,
                    &cx.entity(),
                    cx,
                )
            })
            .into_any_element()
    }

    /// The "⋯" overflow button + popover holding the context / permission /
    /// mode controls when the control row is too narrow to show them inline.
    fn render_overflow_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let usage = self.workspace_store.read(cx).composer_context().0;
        let muted = cx.theme().muted_foreground;
        let mode = self.workspace_store.read(cx).composer_approval_mode();
        let interaction = self.workspace_store.read(cx).composer_interaction_mode();
        let store_entity = self.workspace_store.clone();

        let trigger = Button::new("overflow-controls")
            .ghost()
            .compact()
            .tooltip(crate::tr!("composer.more_controls"))
            .child(Icon::new(IconName::Ellipsis).small().text_color(muted));

        crate::material::overlay_popover("overflow-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_overflow_pane(usage, mode, interaction, &store_entity, &cx.entity(), cx)
            })
            .into_any_element()
    }

    // -- trigger menu + image strip ----------------------------------------

    /// The floating `@`/`/`/`$` menu, rendered in-flow just above the composer
    /// card. `None` when no trigger is active (or it was dismissed).
    fn render_trigger_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.menu_visible() {
            return None;
        }
        let (rows, empty_text, loading) = self.menu_rows(cx);
        let muted = cx.theme().muted_foreground;
        let highlight = self.menu_highlight.min(rows.len().saturating_sub(1));

        let mut list = v_flex().w_full().p_1().gap_0p5();
        if rows.is_empty() {
            list = list.child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2p5()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(if loading {
                        crate::tr!("composer.searching").into_owned()
                    } else {
                        empty_text
                    }),
            );
        } else {
            // T3 §5: the `/` menu groups rows under `Built-in` / `Provider`
            // headers, and `$` under `Skills`. A header is emitted whenever the
            // group changes; headers are not selectable, so row indices (and the
            // arrow-key highlight) still index `rows` directly.
            let mut last_group: Option<&'static str> = None;
            for (index, row) in rows.iter().enumerate() {
                if let Some(group) = row.group
                    && last_group != Some(group)
                {
                    last_group = Some(group);
                    list = list.child(
                        div()
                            .flex_none()
                            .px_2()
                            .pt_1p5()
                            .pb_0p5()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(muted)
                            .child(crate::tr!(group).into_owned()),
                    );
                }
                let is_active = index == highlight;
                let icon = match row.icon {
                    MenuIcon::File => Icon::empty().path("icons/file.svg"),
                    MenuIcon::Folder => Icon::empty().path("icons/folder-closed.svg"),
                    MenuIcon::Command => Icon::empty().path("icons/box.svg"),
                    MenuIcon::Skill => Icon::empty().path("icons/ruler.svg"),
                };
                let accessible_label = crate::tr!(
                    "composer.trigger_option",
                    primary = row.primary.clone(),
                    secondary = row.secondary.clone()
                )
                .into_owned();
                list = list.child(
                    h_flex()
                        .id(("menu-row", index))
                        .role(Role::ListBoxOption)
                        .aria_label(accessible_label)
                        .aria_selected(is_active)
                        .when(is_active, |row| row.aria_active_descendant())
                        .flex_none()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(is_active, |s| s.bg(cx.theme().accent))
                        .hover(|s| s.bg(cx.theme().muted))
                        .child(icon.small().text_color(muted))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(13.))
                                .font_medium()
                                .child(row.primary.clone()),
                        )
                        .when(!row.secondary.is_empty(), |this| {
                            this.child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(13.))
                                    .text_color(muted)
                                    .child(row.secondary.clone()),
                            )
                        })
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.menu_highlight != index {
                                this.menu_highlight = index;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.accept_menu(index, window, cx);
                        })),
                );
            }
        }

        Some(
            div()
                .id("composer-trigger-menu")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.trigger_results"))
                .w_full()
                .max_h(px(288.))
                .overflow_y_scroll()
                // T3 overlay contour: popover fill + hairline border + shadow_xl
                // at the 14px overlay radius (this menu floats over the card).
                .rounded(crate::material::radius_overlay())
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .shadow_xl()
                .child(list)
                .into_any_element(),
        )
    }

    /// The 64px thumbnail strip above the control row (T3), when images are
    /// attached; each thumbnail opens an expanded preview and has a remove `x`.
    /// The queue strip shown ABOVE the card whenever messages are waiting for
    /// the running turn to finish: one row per queued message (truncated text),
    /// each with a send-now/steer button and an ✕ (drop it). Scheduled rows add
    /// a live countdown; rows are reorderable-by-removal only.
    fn render_queue_strip(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some((queued, can_steer, agent)) = self.workspace_store.read(cx).composer_queue()
        else {
            self.scheduled_countdown_tick = None;
            return None;
        };
        let has_scheduled = queued
            .iter()
            .any(|message| message.fire_at_unix_secs.is_some());
        if has_scheduled && self.scheduled_countdown_tick.is_none() {
            self.scheduled_countdown_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    smol::Timer::after(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !has_scheduled {
            // Dropping the task cancels its timer, so at most one repaint loop
            // exists and it stops as soon as the last scheduled row disappears.
            self.scheduled_countdown_tick = None;
        }
        if queued.is_empty() {
            return None;
        }

        let muted = cx.theme().muted_foreground;
        let mut strip = v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary.opacity(0.5))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(crate::tr!("composer.queued_count", count = queued.len())),
            );

        for message in queued {
            let id = message.id;
            let scheduled = message.fire_at_unix_secs.is_some();
            let steer_tooltip = if scheduled {
                crate::tr!("composer.send_now").into_owned()
            } else if can_steer {
                crate::tr!("composer.steer_queued").into_owned()
            } else {
                crate::tr!("composer.steer_unsupported_tooltip", agent = agent).into_owned()
            };
            let countdown = message.fire_at_unix_secs.map(|fire_at| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                format_countdown(fire_at.saturating_sub(now))
            });
            strip = strip.child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(cx.theme().foreground)
                            .child(truncate_queued(&message.text)),
                    )
                    .when_some(countdown, |row, countdown| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(px(12.))
                                .text_color(muted)
                                .child(countdown),
                        )
                    })
                    .child(
                        Button::new(("queue-steer", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowUp)
                            // Scheduled rows always support send-now: the
                            // runtime removes the deadline and uses the normal
                            // send/queue path even when native steering is absent.
                            .disabled(!scheduled && !can_steer)
                            .tooltip(steer_tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.dispatch(Command::SteerQueued { id })
                                });
                            })),
                    )
                    .child(
                        Button::new(("queue-drop", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(crate::tr!("composer.drop_queued"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspace_store.update(cx, |store, _cx| {
                                    store.dispatch(Command::DropQueued { id })
                                });
                            })),
                    ),
            );
        }
        Some(
            div()
                .id("queued-messages-scroll")
                .w_full()
                .max_h(px(180.))
                .overflow_y_scroll()
                .child(strip)
                .into_any_element(),
        )
    }

    fn render_image_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending_images.is_empty() {
            return None;
        }
        let mut row = h_flex().w_full().px_4().pt_3().gap_2().flex_wrap();
        for (index, image) in self.pending_images.iter().enumerate() {
            let path = image.path.clone();
            row = row.child(
                div()
                    .id(("thumb", index))
                    .relative()
                    .size(px(64.))
                    .rounded(crate::material::radius_card())
                    .overflow_hidden()
                    // Card radius + a T2 fill instead of a four-sided hard border.
                    .bg(cx.theme().secondary)
                    .cursor_pointer()
                    .child(
                        img(path)
                            .size(px(64.))
                            .rounded(crate::material::radius_card()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_preview(index, window, cx);
                    }))
                    .child(
                        div()
                            .id(("thumb-x", index))
                            .absolute()
                            .top(px(2.))
                            .right(px(2.))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(gpui::black().opacity(0.6))
                            .cursor_pointer()
                            .hover(|s| s.bg(gpui::black().opacity(0.85)))
                            .child(
                                Icon::new(IconName::Close)
                                    .xsmall()
                                    .text_color(gpui::white()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_image(index, cx);
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// Open the clicked thumbnail as a window-level lightbox (see
    /// [`crate::attachments::open_image_lightbox`]).
    fn open_image_preview(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(image) = self.pending_images.get(index) else {
            return;
        };
        crate::attachments::open_image_lightbox(image.path.clone(), image.name.clone(), window, cx);
    }

    // -- send / stop --------------------------------------------------------

    fn render_send_or_stop(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if turn_running {
            // Providers with native mid-turn steering keep a send button active
            // beside Stop while a turn runs.
            let steers = self.workspace_store.read(cx).composer_supports_steering();
            let has_text = !self.input.read(cx).value().trim().is_empty();
            let mut row = h_flex()
                .gap_2()
                .items_center()
                // Blue activity spinner.
                .child(Spinner::new().small().color(cx.theme().primary));
            if steers {
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
                    .size(px(36.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(bg)
                    .cursor_pointer()
                    .when(has_text, |s| s.hover(|s| s.opacity(0.9)))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(
                            crate::tr!("composer.steer_tooltip").into_owned(),
                        )
                        .build(window, cx)
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
                        this.workspace_store
                            .update(cx, |store, _cx| store.dispatch(Command::Interrupt));
                    })),
                )
                .into_any_element();
        }

        // Group C: while the first send is creating a worktree, show a disabled
        // "Preparing worktree…" pill instead of the send button.
        if self.workspace_store.read(cx).composer_preparing_worktree() {
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
            .composer_plan_ready_markdown()
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

    /// Whether a send right now continues planning rather than starting work.
    /// The plan stays implementable from either mode, but only Plan mode turns
    /// typed feedback into refinement.
    fn refines_the_plan(&self, cx: &App) -> bool {
        self.workspace_store.read(cx).composer_interaction_mode() == InteractionMode::Plan
    }

    /// The Implement split-button: primary "Implement" + a chevron menu with
    /// "Implement in a new thread" (S1 §5).
    fn render_implement_split(&self, cx: &mut Context<Self>) -> AnyElement {
        let primary = cx.theme().primary;
        let fg = cx.theme().primary_foreground;
        let store_main = self.workspace_store.clone();

        let chevron = crate::material::overlay_popover("implement-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("implement-menu-trigger")
                    .primary()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let app = cx.entity();
                let store = store_main.clone();
                let popover = cx.entity();
                v_flex()
                    .w(px(220.))
                    .p_1()
                    .child(
                        h_flex()
                            .id("implement-new-thread")
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(Icon::new(IconName::Plus).xsmall())
                            .child(crate::tr!("plan.implement_new_thread"))
                            .on_click(move |_, window, cx| {
                                store.update(cx, |store, _cx| {
                                    let Some(markdown) = store.composer_plan_ready_markdown()
                                    else {
                                        return;
                                    };
                                    let title = match tcode_core::session::plan_title(&markdown) {
                                        Some(title) => {
                                            crate::tr!("plan.implement_titled", title = title)
                                                .into_owned()
                                        }
                                        None => crate::tr!("plan.implement_untitled").into_owned(),
                                    };
                                    store.dispatch(Command::ImplementPlanInNewThread { title });
                                });
                                let _ = &app;
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    )
                    .into_any_element()
            });

        let store_impl = self.workspace_store.clone();
        h_flex()
            .flex_none()
            .h(px(32.))
            .items_center()
            .rounded(crate::material::radius_button())
            .bg(primary)
            .text_color(fg)
            .overflow_hidden()
            .child(
                h_flex()
                    .id("implement-main")
                    .h_full()
                    .px_3()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .font_medium()
                    .hover(|s| s.opacity(0.9))
                    .child(crate::tr!("plan.implement"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        store_impl.update(cx, |store, _cx| store.dispatch(Command::ImplementPlan));
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(fg).opacity(0.3))
            .child(chevron)
            .into_any_element()
    }

    /// The "Plan Ready" header strip shown atop the composer while a proposed
    /// plan awaits a decision (S1 §5).
    fn render_plan_ready_header(&self, title: String, cx: &mut Context<Self>) -> AnyElement {
        let store = self.workspace_store.clone();
        h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px_2()
                    .py(px(1.))
                    .rounded(px(4.))
                    .bg(cx.theme().primary)
                    .text_color(cx.theme().primary_foreground)
                    .text_size(px(11.))
                    .font_medium()
                    .child(crate::tr!("plan.ready")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(title),
            )
            .child(
                Button::new("dismiss-plan")
                    .ghost()
                    .compact()
                    .icon(IconName::Close)
                    .tooltip(crate::tr!("plan.dismiss"))
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, _cx| store.dispatch(Command::DismissPlan));
                    }),
            )
            .into_any_element()
    }

    // -- user-input question panel (S1 §7) ---------------------------------

    /// Keep the per-request question state in sync: reset the index/selections
    /// when a new request arrives (or the pending one resolves).
    fn sync_user_input_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.workspace_store.read(cx).composer_pending_user_input();
        let current_id = current.as_ref().map(|(id, _)| id.clone());
        if current_id != self.ui_request_id {
            self.ui_request_id = current_id;
            self.ui_question_index = 0;
            self.ui_selections.clear();
            let prefill = current
                .as_ref()
                .and_then(|(_, questions)| questions.first())
                .and_then(|question| question.prefill.as_deref())
                .unwrap_or_default();
            self.user_input_custom.update(cx, |state, cx| {
                state.set_value(prefill, window, cx);
            });
        }
    }

    fn render_user_input_panel(
        &self,
        request_id: String,
        questions: Vec<UserInputQuestion>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;
        let total = questions.len();
        let index = self.ui_question_index.min(total.saturating_sub(1));
        let Some(question) = questions.get(index).cloned() else {
            return div().into_any_element();
        };
        let multi = question.multi_select;
        let selected = self
            .ui_selections
            .get(&question.id)
            .cloned()
            .unwrap_or_default();

        // Header: question header + "N/total" when multiple.
        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_medium()
                    .child(question.header.clone()),
            )
            .when(total > 1, |this| {
                this.child(div().text_size(px(11.)).text_color(muted).child(crate::tr!(
                    "userinput.question_count",
                    index = index + 1,
                    total = total
                )))
            });

        // Option rows.
        let mut options_content = v_flex().w_full().gap_1();
        for (opt_index, option) in question.options.iter().enumerate() {
            let is_selected = selected.iter().any(|l| l == &option.label);
            let label = option.label.clone();
            let question_for_click = question.clone();
            let questions_for_click = questions.clone();
            let request_for_click = request_id.clone();
            let mark: AnyElement = if multi {
                if is_selected {
                    Icon::new(IconName::CircleCheck)
                        .xsmall()
                        .text_color(primary)
                        .into_any_element()
                } else {
                    div()
                        .size(px(14.))
                        .rounded(px(4.))
                        .border_1()
                        .border_color(muted)
                        .into_any_element()
                }
            } else if is_selected {
                Icon::new(IconName::Check)
                    .xsmall()
                    .text_color(primary)
                    .into_any_element()
            } else {
                div().size(px(14.)).into_any_element()
            };
            options_content = options_content.child(
                h_flex()
                    .id(("ui-opt", opt_index))
                    .flex_none()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .items_start()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(is_selected, |s| s.bg(cx.theme().muted))
                    .hover(|s| s.bg(cx.theme().muted))
                    .child(div().flex_none().pt(px(2.)).child(mark))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .text_size(px(13.))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(muted)
                                            .child(format!("{}", opt_index + 1)),
                                    )
                                    .child(div().font_medium().child(option.label.clone())),
                            )
                            .when(!option.description.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(muted)
                                        .child(option.description.clone()),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_toggle_option(&question_for_click, label.clone(), cx);
                        // Single-select answers flow onward by themselves: next
                        // unanswered question, or submission when none remain.
                        if !multi {
                            this.ui_advance_or_submit(
                                &questions_for_click,
                                request_for_click.clone(),
                                window,
                                cx,
                            );
                        }
                    })),
            );
        }
        let options = div()
            .id("user-input-options-scroll")
            .w_full()
            .max_h(px(240.))
            .overflow_y_scroll()
            .child(options_content);

        let custom_input = self.user_input_custom.clone();
        let custom_has_text = !custom_input.read(cx).value().trim().is_empty();
        let custom_answer = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().popover)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.user_input_custom).appearance(false)),
            )
            .child(
                crate::material::accessible_clickable(
                    div(),
                    "ui-custom-answer-submit",
                    Role::Button,
                    crate::tr!("userinput.submit_custom"),
                    cx,
                )
                .size(px(28.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(if custom_has_text {
                    cx.theme().primary
                } else {
                    cx.theme().muted
                })
                .cursor_pointer()
                .when(custom_has_text, |this| this.hover(|this| this.opacity(0.9)))
                .child(
                    Icon::new(IconName::ArrowUp)
                        .xsmall()
                        .text_color(if custom_has_text {
                            cx.theme().primary_foreground
                        } else {
                            cx.theme().muted_foreground
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.submit_custom_user_input(&custom_input, window, cx);
                })),
            );

        // Actions row: navigation only. Answers submit themselves — a
        // single-select click that completes the set submits it, so the only
        // finishing affordance left is Done on a multi-select question (clicks
        // there cannot signal "I'm finished").
        let is_last = index + 1 >= total;
        let all_answered = user_input_all_answered(&questions, &self.ui_selections);
        let questions_submit = questions.clone();
        let request_submit = request_id.clone();
        let mut actions = h_flex().w_full().gap_2().items_center();
        if index > 0 {
            let questions_previous = questions.clone();
            actions = actions.child(
                Button::new("ui-prev")
                    .ghost()
                    .small()
                    .label(crate::tr!("userinput.previous"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_go(-1, &questions_previous, window, cx)
                    })),
            );
        }
        actions = actions.child(div().flex_1());
        if !is_last {
            let questions_next = questions.clone();
            actions = actions.child(
                Button::new("ui-next")
                    .outline()
                    .small()
                    .label(crate::tr!("userinput.next_question"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_go(1, &questions_next, window, cx)
                    })),
            );
        }
        if multi && all_answered {
            actions = actions.child(
                Button::new("ui-done")
                    .primary()
                    .small()
                    .label(crate::tr!("userinput.done"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_submit(&questions_submit, request_submit.clone(), window, cx);
                    })),
            );
        }

        // Number keys 1-9 select the matching option.
        let question_keys = question.clone();
        let questions_keys = questions.clone();
        let request_keys = request_id.clone();

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_sm()
            .on_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                    if let Ok(n) = ev.keystroke.key.parse::<usize>()
                        && n >= 1
                        && n <= question_keys.options.len()
                    {
                        let label = question_keys.options[n - 1].label.clone();
                        this.ui_toggle_option(&question_keys, label, cx);
                        if !question_keys.multi_select {
                            this.ui_advance_or_submit(
                                &questions_keys,
                                request_keys.clone(),
                                window,
                                cx,
                            );
                        }
                    }
                }),
            )
            .child(header)
            .child(div().text_size(px(15.)).child(question.question.clone()))
            .child(options)
            .child(custom_answer)
            .when(multi, |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(crate::tr!("userinput.multi_hint")),
                )
            })
            .child(actions)
            .into_any_element()
    }

    /// Toggle an option label for a question: single-select replaces, multi
    /// toggles membership.
    fn ui_toggle_option(
        &mut self,
        question: &UserInputQuestion,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let entry = self.ui_selections.entry(question.id.clone()).or_default();
        if question.multi_select {
            if let Some(pos) = entry.iter().position(|l| l == &label) {
                entry.remove(pos);
            } else {
                entry.push(label);
            }
        } else {
            *entry = vec![label];
        }
        cx.notify();
    }

    fn submit_custom_user_input(
        &mut self,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((request_id, questions)) = self.pending_user_input(cx) else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let index = self
            .ui_question_index
            .min(questions.len().saturating_sub(1));
        if let Some(question) = questions.get(index) {
            let entry = self.ui_selections.entry(question.id.clone()).or_default();
            if question.multi_select {
                entry.push(text);
            } else {
                *entry = vec![text];
            }
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_advance_or_submit(&questions, request_id, window, cx);
        cx.notify();
    }

    fn ui_go(
        &mut self,
        delta: i32,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.ui_question_index as i32 + delta;
        if next >= 0 {
            self.ui_question_index = next as usize;
            self.seed_user_input_prefill(questions, window, cx);
            cx.notify();
        }
    }

    fn seed_user_input_prefill(
        &mut self,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = questions
            .get(self.ui_question_index)
            .and_then(|question| question.prefill.as_deref())
            .unwrap_or_default();
        self.user_input_custom
            .update(cx, |state, cx| state.set_value(prefill, window, cx));
    }

    /// After answering: jump to the next unanswered question, or — when the
    /// answer completed the whole set — submit without any button press
    /// (S1 §7). The ~200ms pause lets the selection mark register visually
    /// before the panel moves on.
    fn ui_advance_or_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let questions = questions.to_vec();
        let at = self.ui_question_index;
        cx.spawn_in(window, async move |this, cx| {
            smol::Timer::after(std::time::Duration::from_millis(200)).await;
            let _ = this.update_in(cx, |this, window, cx| {
                // A newer request or manual navigation invalidates this hop.
                if this.ui_question_index != at
                    || this.ui_request_id.as_deref() != Some(&request_id)
                {
                    return;
                }
                if user_input_all_answered(&questions, &this.ui_selections) {
                    this.ui_submit(&questions, request_id, window, cx);
                } else if let Some(next) =
                    next_unanswered_question(&questions, &this.ui_selections, at)
                {
                    this.ui_question_index = next;
                    this.seed_user_input_prefill(&questions, window, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn ui_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let custom = self.user_input_custom.read(cx).value().trim().to_string();
        let custom = if custom.is_empty() {
            None
        } else {
            Some(custom.as_str())
        };
        let answers = assemble_user_input_answers(
            questions,
            &self.ui_selections,
            self.ui_question_index,
            custom,
        );
        self.user_input_custom
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_selections.clear();
        self.ui_question_index = 0;
        self.workspace_store.update(cx, |store, _cx| {
            store.dispatch(Command::RespondUserInput {
                request_id,
                answers,
            })
        });
        cx.notify();
    }

    // -- below-card + approval ---------------------------------------------

    fn render_checkout_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let checkout = self.workspace_store.read(cx).composer_checkout_state()?;
        let branch = checkout.branch;
        let branches = checkout.branches;
        let turn_running = checkout.turn_running;
        let is_draft = checkout.is_draft;
        let worktree_base = checkout.worktree_base;
        let worktree = checkout.worktree;
        let muted = cx.theme().muted_foreground;
        // In worktree draft mode the right-hand picker chooses the *base* branch
        // (its current value is the chosen base, defaulting to the live branch).
        let picker_current = worktree_base.clone().unwrap_or_else(|| branch.clone());
        let worktree_mode = worktree_base.is_some();

        // The branch chip: a popover listing local branches. While a turn runs
        // the selector is disabled (it just shows a "wait" tooltip).
        let right: AnyElement = if turn_running {
            Button::new("branch-picker")
                .ghost()
                .compact()
                .tooltip(crate::tr!("composer.wait_turn"))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                        .child(picker_current.clone()),
                )
                .into_any_element()
        } else {
            let store_open = self.workspace_store.clone();
            let store_content = self.workspace_store.clone();
            let current = picker_current.clone();
            let trigger = Button::new("branch-picker").ghost().compact().child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                    .child(picker_current.clone())
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );
            crate::material::overlay_popover("branch-popover")
                .anchor(Anchor::BottomRight)
                .trigger(trigger)
                .on_open_change(move |open, _window, cx| {
                    // Load branches lazily each time the popover opens.
                    if *open {
                        store_open.update(cx, |store, _cx| store.dispatch(Command::LoadBranches));
                    }
                })
                .content(move |_state, _window, cx| {
                    let branches = branches.clone();
                    let current = current.clone();
                    let popover = cx.entity();
                    let muted = cx.theme().muted_foreground;
                    let mut col = v_flex().w_full().p_1().gap_0p5();
                    if worktree_mode {
                        col = col.child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1()
                                .text_size(px(11.))
                                .font_medium()
                                .text_color(muted)
                                .child(crate::tr!("composer.worktree_base")),
                        );
                    }
                    if branches.is_empty() {
                        col = col.child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1p5()
                                .text_size(px(13.))
                                .text_color(muted)
                                .child(crate::tr!("composer.loading")),
                        );
                    } else {
                        for (index, name) in branches.iter().enumerate() {
                            let is_current = *name == current;
                            let branch_name = name.clone();
                            let store_pick = store_content.clone();
                            let pop = popover.clone();
                            col = col.child(
                                h_flex()
                                    .id(("branch-row", index))
                                    .flex_none()
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
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .child(name.clone()),
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
                                        store_pick.update(cx, |store, _cx| {
                                            let command = if worktree_mode {
                                                // Choose the worktree's base branch.
                                                Command::SetDraftWorkspace {
                                                    mode: WorkspaceMode::NewWorktree {
                                                        base: branch_name,
                                                    },
                                                }
                                            } else {
                                                Command::CheckoutBranch {
                                                    branch: branch_name,
                                                }
                                            };
                                            store.dispatch(command);
                                        });
                                        pop.update(cx, |st, cx| st.dismiss(window, cx));
                                    }),
                            );
                        }
                    }
                    div()
                        .id("branch-list")
                        .w(px(220.))
                        .max_h(px(280.))
                        .overflow_y_scroll()
                        .child(col)
                        .into_any_element()
                })
                .into_any_element()
        };

        // Left: the workspace-mode chip. A draft can pick "Local checkout" vs
        // "New worktree"; a started session shows its locked workspace.
        let left =
            self.render_workspace_chip(is_draft, worktree_mode, worktree.is_some(), &branch, cx);

        Some(
            h_flex()
                .w_full()
                .px_2()
                .pt_2()
                .items_center()
                .justify_between()
                .text_size(px(13.))
                .text_color(muted)
                .child(left)
                .child(right)
                .into_any_element(),
        )
    }

    /// The left-hand workspace chip: a draft can pick current checkout vs a new
    /// dedicated worktree; a started session shows its locked workspace.
    fn render_workspace_chip(
        &self,
        is_draft: bool,
        worktree_mode: bool,
        has_worktree: bool,
        base_default: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let label = if worktree_mode || has_worktree {
            crate::tr!("composer.new_worktree")
        } else {
            crate::tr!("composer.local_checkout")
        };

        // Started sessions show a static, locked workspace label.
        if !is_draft {
            return h_flex()
                .gap_1p5()
                .items_center()
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                .child(label)
                .into_any_element();
        }

        let store_content = self.workspace_store.clone();
        let base_default = base_default.to_string();
        let trigger = Button::new("workspace-picker").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );
        crate::material::overlay_popover("workspace-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let popover = cx.entity();
                let store_local = store_content.clone();
                let store_worktree = store_content.clone();
                let pop_local = popover.clone();
                let pop_worktree = popover.clone();
                let base = base_default.clone();
                let workspace_row = |label: gpui::SharedString,
                                     selected: bool,
                                     cx: &mut Context<PopoverState>|
                 -> gpui::Div {
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .hover(|s| s.bg(cx.theme().muted))
                        .child(div().flex_1().min_w_0().child(label))
                        .when(selected, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .xsmall()
                                    .text_color(cx.theme().primary),
                            )
                        })
                };
                v_flex()
                    .w(px(200.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("composer.workspace")),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.local_checkout").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-local")
                        .on_click(move |_, window, cx| {
                            store_local.update(cx, |store, _cx| {
                                store.dispatch(Command::SetDraftWorkspace {
                                    mode: WorkspaceMode::LocalCheckout,
                                });
                            });
                            pop_local.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.new_worktree").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-worktree")
                        .on_click(move |_, window, cx| {
                            let base = base.clone();
                            store_worktree.update(cx, |store, _cx| {
                                store.dispatch(Command::SetDraftWorkspace {
                                    mode: WorkspaceMode::NewWorktree { base },
                                });
                            });
                            pop_worktree.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_approval_panel(
        &self,
        request: &ApprovalRequest,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = match &request.kind {
            ApprovalKind::ExecCommand { .. } => crate::tr!("approval.command_requested"),
            ApprovalKind::FileRead { .. } => crate::tr!("approval.file_read_requested"),
            ApprovalKind::FileChange { .. } => crate::tr!("approval.file_requested"),
            ApprovalKind::ToolUse { .. } => crate::tr!("approval.tool_requested"),
        };
        let muted = cx.theme().muted_foreground;

        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, .. } => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(command.clone()),
                )
                .when_some(cwd.clone(), |this, cwd| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(crate::tr!("approval.in_directory", cwd = cwd)),
                    )
                })
                .into_any_element(),
            ApprovalKind::FileChange { changes, .. } => v_flex()
                .gap_0p5()
                .children(changes.iter().map(|change| {
                    div()
                        .text_size(px(13.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!(
                            "{} {}",
                            file_change_kind_label(change.kind),
                            change.path
                        ))
                }))
                .into_any_element(),
            ApprovalKind::FileRead { detail } => div()
                .text_size(px(13.))
                .font_family(cx.theme().mono_font_family.clone())
                .child(detail.clone())
                .into_any_element(),
            ApprovalKind::ToolUse { name, input, .. } => div()
                .text_size(px(13.))
                .font_family(cx.theme().mono_font_family.clone())
                .child(format!("{name} {input}"))
                .into_any_element(),
        };

        let expanded = self.approval_expanded;
        let approve_id = request.id.clone();
        let always_id = request.id.clone();
        let deny_id = request.id.clone();
        let cancel_id = request.id.clone();

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(crate::material::radius_card())
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
                            .child(crate::tr!("approval.pending")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_medium()
                            .child(summary),
                    )
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
                        .id("approval-detail-scroll")
                        .w_full()
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .p_2()
                        .rounded(px(8.))
                        .bg(cx.theme().muted)
                        .child(detail),
                )
            })
            .when(!request.options.is_empty(), |this| {
                // An ACP agent sends its own option list: render exactly those
                // buttons (the labels are the agent's), ordered rejections-first
                // like our fixed four, and answer with the chosen option id.
                let mut row = h_flex().w_full().gap_2().items_center();
                let mut options = request.options.clone();
                options.sort_by_key(|option| match option.kind {
                    ApprovalOptionKind::RejectAlways => 0,
                    ApprovalOptionKind::RejectOnce => 1,
                    ApprovalOptionKind::AllowAlways => 2,
                    ApprovalOptionKind::AllowOnce => 3,
                });
                let last = options.len().saturating_sub(1);
                for (index, option) in options.into_iter().enumerate() {
                    let request_id = request.id.clone();
                    let option_id = option.id.clone();
                    let rejects = matches!(
                        option.kind,
                        ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways
                    );
                    let button = Button::new(gpui::SharedString::from(format!(
                        "approval-option-{}",
                        option.id
                    )))
                    .small()
                    .label(option.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.respond(
                            request_id.clone(),
                            ApprovalDecision::Option(option_id.clone()),
                            cx,
                        );
                    }));
                    // The agent's preferred (last) option is the primary action.
                    let button = if index == last {
                        button.primary()
                    } else if rejects {
                        button.ghost().text_color(cx.theme().danger)
                    } else {
                        button.ghost()
                    };
                    if index == 1 {
                        row = row.child(div().flex_1());
                    }
                    row = row.child(button);
                }
                this.child(row)
            })
            .when(request.options.is_empty(), |this| {
                this.child(
                    // T3 order (S2 §4): Cancel turn, Decline, Always allow this
                    // session, Approve once.
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            Button::new("approval-cancel")
                                .ghost()
                                .small()
                                .label(crate::tr!("approval.cancel_turn"))
                                .text_color(cx.theme().muted_foreground)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(cancel_id.clone(), ApprovalDecision::Cancel, cx);
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("approval-deny")
                                .ghost()
                                .small()
                                .label(crate::tr!("approval.decline"))
                                .text_color(cx.theme().danger)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(deny_id.clone(), ApprovalDecision::Deny, cx);
                                })),
                        )
                        .child(
                            Button::new("approval-always")
                                .ghost()
                                .small()
                                .label(crate::tr!("approval.always_allow_session"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(
                                        always_id.clone(),
                                        ApprovalDecision::ApproveForSession,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            Button::new("approval-approve")
                                .primary()
                                .small()
                                .label(crate::tr!("approval.approve_once"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.respond(approve_id.clone(), ApprovalDecision::Approve, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    fn respond(&mut self, request_id: String, decision: ApprovalDecision, cx: &mut Context<Self>) {
        self.approval_expanded = false;
        self.workspace_store.update(cx, |store, _cx| {
            store.dispatch(Command::RespondApproval {
                request_id,
                decision,
            })
        });
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_user_input_state(window, cx);
        self.sync_images_session(cx);
        self.sync_text_destination(window, cx);
        self.sync_native_rewind_prefill(window, cx);
        let (turn_running, approval, approval_count) =
            self.workspace_store.read(cx).composer_render_state();

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
            .composer_plan_ready_markdown()
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
                state.set_placeholder(desired_placeholder, window, cx)
            });
        }

        let user_input = self.pending_user_input(cx);

        // Dropping image files onto the card attaches them (T3 drag-drop).
        let composer = cx.entity();
        let terminal_contexts = self.workspace_store.read(cx).composer_terminal_contexts();
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
                        .label(format!("{} · {}  ×", context.terminal_label, range))
                        .tooltip(context.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.dispatch(Command::RemoveTerminalContext { context_id: id })
                            });
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
                        .label(format!("{} {}  ×", comment.file, range))
                        .tooltip(comment.text)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.dispatch(Command::RemoveReviewComment { index })
                            });
                        }))
                }),
        );

        // The hero input surface (spec §5): 16px composer corners; resting =
        // hairline `input.border` + `shadow_md`. Focus only deepens the border
        // to a neutral tone — the redesign's blue ring + glow read as garish
        // against the frosted canvas and were reverted.
        let composer_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let card = v_flex()
            .w_full()
            .rounded(crate::material::radius_composer())
            .border_1()
            .border_color(if composer_focused {
                cx.theme().foreground.opacity(0.35)
            } else {
                cx.theme().input
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
            .when(has_terminal_contexts, |this| {
                this.child(div().px_3().pt_2().child(context_chips))
            })
            .when(has_review_comments, |this| {
                this.child(div().px_3().pt_2().child(review_chips))
            })
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .child(Input::new(&self.input).appearance(false)),
            )
            .children(self.render_image_strip(cx))
            // While a turn is running the two send gestures diverge, so say so.
            .when(turn_running, |this| {
                this.child(
                    div()
                        .px_4()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(crate::tr!(
                            "composer.queue_hint",
                            shortcut = format_secondary_shortcut("enter")
                        )),
                )
            })
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
                    this.workspace_store.update(cx, |store, _cx| {
                        store.dispatch(Command::ToggleInteractionMode)
                    });
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

// ---------------------------------------------------------------------------
// Popover panes (free functions: they run in a `PopoverState` context)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_model_pane(
    rows: &[ModelRow],
    selected: &Option<String>,
    rail: PickerRail,
    // (id, name) of every installed+enabled ACP agent — one rail entry each.
    acp_agents: &[(String, String)],
    pending_restart: bool,
    loading: bool,
    store_entity: &Entity<WorkspaceStore>,
    model_search: &Entity<InputState>,
    composer: &Entity<Composer>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;

    // Left rail: favorites star + one glyph per profile. The `label` names the
    // entry on hover so two profiles of the same kind (official vs third-party)
    // are told apart even though they share a glyph.
    let rail_icon = |id: gpui::SharedString,
                     label: gpui::SharedString,
                     icon: Icon,
                     active: bool,
                     target: PickerRail,
                     cx: &mut Context<PopoverState>|
     -> AnyElement {
        let composer = composer.clone();
        crate::material::accessible_clickable(div(), id, Role::Tab, label.clone(), cx)
            .aria_selected(active)
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .when(active, |s| s.bg(cx.theme().muted))
            .hover(|s| s.bg(cx.theme().muted))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(label.clone()).build(window, cx)
            })
            .child(
                icon.small()
                    .text_color(if active { cx.theme().foreground } else { muted }),
            )
            .on_click(move |_, _, cx| {
                let target = target.clone();
                composer.update(cx, |c, cx| {
                    c.picker_rail = Some(target);
                    cx.notify();
                });
            })
            .into_any_element()
    };

    let mut rail_col = v_flex().w_full().py_2().px_1p5().gap_1().child(rail_icon(
        "rail-fav".into(),
        crate::tr!("composer.favorites").into_owned().into(),
        Icon::new(IconName::Star),
        rail == PickerRail::Favorites,
        PickerRail::Favorites,
        cx,
    ));
    // One entry per *enabled* native profile: every built-in plus any
    // user-created profiles whose switch is on. Each is its own rail.
    let profile_ids: Vec<String> = {
        let store = store_entity.read(cx);
        let profiles = store.enabled_profiles();
        PICKER_PROVIDER_KINDS
            .into_iter()
            .flat_map(|kind| {
                profiles
                    .iter()
                    .filter(move |profile| profile.kind == kind)
                    .map(|profile| profile.id.clone())
            })
            .collect()
    };
    for id in profile_ids {
        let glyph = tinted_profile_glyph(&id, store_entity.read(cx));
        let label = store_entity.read(cx).provider_profile_display_name(&id);
        rail_col = rail_col.child(rail_icon(
            gpui::SharedString::from(format!("rail-profile-{id}")),
            label.into(),
            glyph,
            rail == PickerRail::Profile(id.clone()),
            PickerRail::Profile(id.clone()),
            cx,
        ));
    }
    // …then one entry per installed ACP agent (Settings → Providers → ACP Agents).
    for (id, name) in acp_agents {
        rail_col = rail_col.child(rail_icon(
            gpui::SharedString::from(format!("rail-acp-{id}")),
            gpui::SharedString::from(name.clone()),
            Icon::empty().path("icons/box.svg"),
            rail == PickerRail::Acp(id.clone()),
            PickerRail::Acp(id.clone()),
            cx,
        ));
    }
    let rail = div()
        .id("model-provider-rail")
        .role(Role::TabList)
        .aria_label(crate::tr!("composer.model_sources"))
        .flex_none()
        .w(px(44.))
        .h_full()
        .border_r_1()
        .border_color(cx.theme().border)
        .overflow_y_scroll()
        .child(rail_col);

    // Main pane: search + rows.
    let mut list = v_flex().w_full().min_h_0().gap_0p5().px_1().py_1();
    for (index, row) in rows.iter().enumerate() {
        list = list.child(render_model_row(
            row,
            index,
            selected,
            store_entity,
            popover,
            cx,
        ));
    }
    if rows.is_empty() {
        list = list.child(
            div()
                .flex_none()
                .px_3()
                .py_4()
                .text_size(px(13.))
                .text_color(muted)
                .child(if loading {
                    crate::tr!("composer.loading_models")
                } else {
                    crate::tr!("composer.no_models")
                }),
        );
    }

    let mut pane = v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .child(
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(Input::new(model_search).appearance(false)),
        )
        .child(
            div()
                .id("model-picker-list")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.model_results"))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(list),
        );
    if pending_restart {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }

    // Secondary+1-9 selects the corresponding row while the popover is open.
    let key_rows: Vec<ModelRow> = rows.iter().take(9).cloned().collect();
    let store_key = store_entity.clone();
    let popover_key = popover.clone();

    h_flex()
        .w(px(360.))
        .h(px(360.))
        .items_stretch()
        .rounded(px(12.))
        .overflow_hidden()
        .on_key_down(move |ev, window, cx| {
            if !ev.keystroke.modifiers.secondary() {
                return;
            }
            if let Ok(n) = ev.keystroke.key.parse::<usize>()
                && n >= 1
                && n <= key_rows.len()
            {
                let row = key_rows[n - 1].clone();
                store_key.update(cx, |store, _cx| {
                    let command = if row.acp {
                        Command::SetActiveAcpAgent { id: row.id }
                    } else {
                        Command::SetActiveModel {
                            provider: row.provider,
                            model: Some(row.id),
                            profile_id: row.profile_id,
                        }
                    };
                    store.dispatch(command);
                });
                popover_key.update(cx, |st, cx| st.dismiss(window, cx));
            }
        })
        .child(rail)
        .child(pane)
        .into_any_element()
}

fn render_model_row(
    row: &ModelRow,
    index: usize,
    selected: &Option<String>,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let is_current = selected.as_deref() == Some(row.id.as_str());
    let is_acp = row.acp;
    let is_fav = !is_acp && store_entity.read(cx).composer_is_favorite_model(&row.id);
    let name = row.name.clone();
    let id = row.id.clone();
    let provider = row.provider;
    let profile_id = row.profile_id.clone();
    let fav_id = row.id.clone();

    let store_select = store_entity.clone();
    let popover_select = popover.clone();
    let store_fav = store_entity.clone();
    let popover_fav = popover.clone();

    let accessible_label = crate::tr!("composer.model_option", model = name.clone()).into_owned();
    h_flex()
        .id(("model-row", index))
        .role(Role::ListBoxOption)
        .aria_label(accessible_label)
        .aria_selected(is_current)
        .when(is_current, |row| row.aria_active_descendant())
        .flex_none()
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(cx.theme().muted))
        .on_click(move |_, window, cx| {
            let id = id.clone();
            let profile_id = profile_id.clone();
            store_select.update(cx, |store, _cx| {
                let command = if is_acp {
                    Command::SetActiveAcpAgent { id }
                } else {
                    Command::SetActiveModel {
                        provider,
                        model: Some(id),
                        profile_id,
                    }
                };
                store.dispatch(command);
            });
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
                        .child(tinted_provider_glyph(row.provider, store_entity.read(cx)).xsmall())
                        .child(provider_label(row.provider)),
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
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(format_secondary_shortcut(&(index + 1).to_string())),
            )
        })
        .child(
            crate::material::accessible_clickable(
                div(),
                ("model-fav", index),
                Role::Button,
                if is_fav {
                    crate::tr!("composer.remove_favorite")
                } else {
                    crate::tr!("composer.add_favorite")
                },
                cx,
            )
            .flex_none()
            .p(px(2.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(
                Icon::new(if is_fav {
                    IconName::StarFill
                } else {
                    IconName::Star
                })
                .xsmall()
                .text_color(if is_fav {
                    rgb(CLAUDE_BRAND_COLOR).into()
                } else {
                    muted
                }),
            )
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let fav_id = fav_id.clone();
                store_fav.update(cx, |store, _cx| {
                    store.dispatch(Command::ToggleFavoriteModel { model: fav_id })
                });
                // Refresh the open popover so the star + ordering update.
                popover_fav.update(cx, |_, cx| cx.notify());
            }),
        )
        .into_any_element()
}

/// The approval-mode popover: three rows (icon + bold name + muted
/// description), a ✓ on the current mode, and an optional restart note when the
/// live provider (Codex) will restart to apply the change on the next turn.
fn render_permission_pane(
    current: ApprovalMode,
    pending_restart: bool,
    native_approval_modes_enabled: bool,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;

    let mut list = v_flex()
        .id("permission-menu")
        .role(Role::Menu)
        .aria_label(crate::tr!("approval.choose_mode"))
        .w_full()
        .p_1()
        .gap_0p5();
    for (index, (mode, label, description, icon_path)) in APPROVAL_MODES.iter().enumerate() {
        let mode = *mode;
        let is_current = mode == current;
        let is_disabled = !native_approval_modes_enabled
            && matches!(
                mode,
                ApprovalMode::Supervised | ApprovalMode::AutoAcceptEdits
            );
        let store = store_entity.clone();
        let popover = popover.clone();
        let disabled_hint = crate::tr!("approval.pi_native_approvals_required");
        let accessible_label = crate::tr!(
            "approval.mode_option",
            label = crate::tr!(*label),
            description = if is_disabled {
                format!("{} {}", crate::tr!(*description), disabled_hint)
            } else {
                crate::tr!(*description).into_owned()
            }
        )
        .into_owned();
        list = list.child(
            h_flex()
                .id(("permission-row", index))
                .role(Role::MenuItem)
                .aria_label(accessible_label)
                .aria_selected(is_current)
                .when(is_current, |row| row.aria_active_descendant())
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .items_start()
                .rounded(px(6.))
                .when(is_disabled, |row| row.opacity(0.55))
                .when(!is_disabled, |row| {
                    row.cursor_pointer()
                        .hover(|s| s.bg(cx.theme().muted))
                        .on_click(move |_, window, cx| {
                            store.update(cx, |store, _cx| {
                                store.dispatch(Command::SetActiveApprovalMode { mode })
                            });
                            popover.update(cx, |st, cx| st.dismiss(window, cx));
                        })
                })
                .child(
                    Icon::empty()
                        .path(*icon_path)
                        .small()
                        .text_color(if is_current { primary } else { muted }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_size(px(13.))
                                .child(div().font_medium().child(crate::tr!(*label)))
                                .when(is_current, |this| {
                                    this.child(
                                        Icon::new(IconName::Check).xsmall().text_color(primary),
                                    )
                                }),
                        )
                        .child(
                            v_flex()
                                .gap_0p5()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(crate::tr!(*description))
                                .when(is_disabled, |text| text.child(disabled_hint)),
                        ),
                ),
        );
    }

    let mut pane = v_flex().w(px(280.)).child(list);
    if pending_restart {
        pane = pane.child(
            div()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }
    pane.into_any_element()
}

/// The traits popover: one section per option descriptor (S1 §3). Select
/// descriptors list their options (✓ + " (default)"); booleans list On/Off. The
/// reasoning section locks while the prompt text contains "ultrathink".
#[allow(clippy::too_many_arguments)]
fn render_traits_pane(
    spec: &ModelSpec,
    selections: &[agent::OptionSelection],
    ultrathink_armed: bool,
    locked: bool,
    pending_restart: bool,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;
    let default_suffix = crate::tr!("composer.option_default").into_owned();

    let section_header = |label: &str, cx: &mut Context<PopoverState>| -> AnyElement {
        div()
            .flex_none()
            .px_2()
            .pt_2()
            .pb_1()
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.to_string())
            .into_any_element()
    };

    let mut pane = v_flex().w_full().p_1().gap_0p5();

    for descriptor in &spec.options {
        match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
            } => {
                let is_reasoning = id == "reasoningEffort";
                pane = pane.child(section_header(label, cx));
                if is_reasoning && locked {
                    pane = pane.child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_1p5()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(crate::tr!("composer.ultrathink_locked")),
                    );
                    continue;
                }
                let resolved = resolved_select_value(id, options, default_value, selections);
                for (index, opt) in options.iter().enumerate() {
                    let is_default = default_value.as_deref() == Some(opt.value.as_str());
                    let is_ultra = is_reasoning && opt.value == "ultrathink";
                    let is_selected = if is_reasoning && ultrathink_armed {
                        is_ultra
                    } else if is_ultra {
                        false
                    } else {
                        resolved.as_deref() == Some(opt.value.as_str())
                    };
                    let mut text = opt.label.clone();
                    if is_default {
                        text.push_str(&default_suffix);
                    }
                    let store = store_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    let opt_value = opt.value.clone();
                    pane = pane.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("trait-opt-{id}-{index}")))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(div().flex_1().min_w_0().child(text))
                            .when(is_selected, |this| {
                                this.child(Icon::new(IconName::Check).xsmall().text_color(primary))
                            })
                            .on_click(move |_, window, cx| {
                                let opt_id = opt_id.clone();
                                let opt_value = opt_value.clone();
                                store.update(cx, |store, _cx| {
                                    let command = if is_ultra {
                                        Command::SelectUltrathink
                                    } else {
                                        Command::SetActiveOption {
                                            id: opt_id,
                                            value: Some(serde_json::Value::String(opt_value)),
                                        }
                                    };
                                    store.dispatch(command);
                                });
                                pop.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    );
                }
            }
            OptionDescriptor::Boolean {
                id,
                label,
                default_value,
            } => {
                pane = pane.child(section_header(label, cx));
                let on = option_selection_bool(selections, id).unwrap_or(*default_value);
                for (index, (value, text)) in [
                    (true, crate::tr!("composer.on").into_owned()),
                    (false, crate::tr!("composer.off").into_owned()),
                ]
                .into_iter()
                .enumerate()
                {
                    let is_selected = on == value;
                    let store = store_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    pane = pane.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("trait-opt-{id}-{index}")))
                            .flex_none()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_size(px(13.))
                            .hover(|s| s.bg(cx.theme().muted))
                            .child(div().flex_1().min_w_0().child(text))
                            .when(is_selected, |this| {
                                this.child(Icon::new(IconName::Check).xsmall().text_color(primary))
                            })
                            .on_click(move |_, window, cx| {
                                let opt_id = opt_id.clone();
                                store.update(cx, |store, _cx| {
                                    store.dispatch(Command::SetActiveOption {
                                        id: opt_id,
                                        value: Some(serde_json::Value::Bool(value)),
                                    });
                                });
                                pop.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    );
                }
            }
        }
    }

    if pending_restart {
        pane = pane.child(
            div()
                .flex_none()
                .px_2()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.restart_note")),
        );
    }
    div()
        .id("traits-options-scroll")
        .w(px(280.))
        .max_h(px(360.))
        .overflow_y_scroll()
        .child(pane)
        .into_any_element()
}

/// The "⋯" overflow popover: the context chip's usage summary plus the
/// permission / mode chips, shown when the control row collapses at narrow
/// widths.
fn render_overflow_pane(
    usage: Option<TokenUsage>,
    mode: ApprovalMode,
    interaction: InteractionMode,
    store_entity: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
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

    let (mode_label, mode_icon) = approval_mode_meta(mode);
    let (interaction_icon, interaction_label) = match interaction {
        InteractionMode::Build => ("icons/box.svg", crate::tr!("composer.build")),
        InteractionMode::Plan => ("icons/ruler.svg", crate::tr!("composer.plan")),
    };
    let next_interaction = match interaction {
        InteractionMode::Build => InteractionMode::Plan,
        InteractionMode::Plan => InteractionMode::Build,
    };
    let interaction_store = store_entity.clone();
    let interaction_popover = popover.clone();
    v_flex()
        .w(px(220.))
        .p_1()
        .gap_0p5()
        .child(item(Icon::new(IconName::Info), context_label(usage)))
        // The permission row stays display-only: its full-width counterpart is
        // an explicit picker, and cycling here would let two stray clicks
        // escalate a Supervised session all the way to Full access.
        .child(item(Icon::empty().path(mode_icon), mode_label))
        .child(
            h_flex()
                .id("overflow-interaction")
                .w_full()
                .px_2()
                .py_1p5()
                .gap_1p5()
                .items_center()
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(muted)
                .hover(|style| style.bg(cx.theme().muted))
                .child(
                    Icon::empty()
                        .path(interaction_icon)
                        .small()
                        .text_color(muted),
                )
                .child(interaction_label)
                .on_click(move |_, window, cx| {
                    interaction_store.update(cx, |store, _cx| {
                        store.dispatch(Command::SetInteractionMode {
                            mode: next_interaction,
                        })
                    });
                    interaction_popover.update(cx, |state, cx| state.dismiss(window, cx));
                }),
        )
        .into_any_element()
}

/// The circular context-window meter's popover (T3's `ContextWindowMeter`
/// hover card): title, percentage · used/max, a progress bar, and the
/// "<Provider> automatically compacts its context when needed." line.
fn render_context_meter_pane(
    usage: Option<TokenUsage>,
    provider: Option<ProviderKind>,
    pct: Option<f32>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let overloaded = pct.map(context_meter::is_overloaded).unwrap_or(false);
    let bar_color: Hsla = if overloaded {
        rgb(METER_RED).into()
    } else {
        rgb(METER_BLUE).into()
    };
    let mut pane = v_flex().w(px(256.)).p_3().gap_2();

    // Header: "Context Window" + "N% · used/max" (or just used tokens).
    let used = usage.as_ref().and_then(context_meter::used_tokens);
    let max = usage.and_then(|u| u.context_window);
    let pct_label = context_meter::format_percentage(pct);
    let stat: AnyElement = match (max, pct_label.clone()) {
        (Some(max), Some(pct_label)) => h_flex()
            .gap_1()
            .text_size(px(11.))
            .text_color(muted)
            .child(pct_label)
            .child("·")
            .child(format!(
                "{}/{}",
                context_meter::format_tokens(used),
                context_meter::format_tokens(Some(max))
            ))
            .into_any_element(),
        _ => div()
            .text_size(px(11.))
            .text_color(muted)
            .child(context_meter::format_tokens(used))
            .into_any_element(),
    };
    pane = pane.child(
        h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(crate::tr!("composer.context_window_title")),
            )
            .child(stat),
    );

    // Progress bar (only when the window size is known).
    if max.is_some() {
        let fraction = pct.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0;
        pane = pane.child(
            div()
                .w_full()
                .h(px(6.))
                .rounded_full()
                .bg(cx.theme().muted)
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(bar_color)
                        .w(gpui::relative(fraction)),
                ),
        );
    }

    // "Total processed" — the session-cumulative token count, when the provider
    // reports it (a native running total or adapter-side accumulation).
    if let Some(total) = usage.and_then(|u| u.total_processed_tokens) {
        pane = pane.child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .gap_3()
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!("composer.total_processed"))
                .child(context_meter::format_tokens(Some(total))),
        );
    }

    // "<Provider> automatically compacts its context when needed."
    if let Some(provider) = provider {
        pane = pane.child(
            div()
                .pt_1()
                .text_size(px(11.))
                .text_color(muted)
                .child(crate::tr!(
                    "composer.compacts_automatically",
                    provider = provider_label(provider)
                )),
        );
    }

    pane.into_any_element()
}

/// Draw the small progress ring: a muted full-circle track plus a `pct`-swept
/// arc (starting at 12 o'clock), sampled as a stroked polyline.
fn ring_canvas(pct: f32, fg: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            let center = bounds.center();
            let radius = px(6.5);
            let width = px(2.5);
            if let Some(path) = stroked_arc(center, radius, 0.0, 360.0, width) {
                window.paint_path(path, track);
            }
            let pct = pct.clamp(0.0, 100.0);
            if pct > 0.0 {
                let end = -90.0 + pct / 100.0 * 360.0;
                if let Some(path) = stroked_arc(center, radius, -90.0, end, width) {
                    window.paint_path(path, fg);
                }
            }
        },
    )
    .size(px(16.))
}

/// Build a stroked arc path from `start_deg` to `end_deg` (degrees, clockwise
/// from 3 o'clock) as a sampled polyline of the given stroke width.
fn stroked_arc(
    center: gpui::Point<Pixels>,
    radius: Pixels,
    start_deg: f32,
    end_deg: f32,
    width: Pixels,
) -> Option<gpui::Path<Pixels>> {
    let mut builder = PathBuilder::stroke(width);
    let steps = 48;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let angle = (start_deg + (end_deg - start_deg) * t).to_radians();
        let p = point(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        );
        if i == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    builder.build().ok()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Traits (option descriptors)
// ---------------------------------------------------------------------------

fn option_selection_str<'a>(selections: &'a [agent::OptionSelection], id: &str) -> Option<&'a str> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_str())
}

fn option_selection_bool(selections: &[agent::OptionSelection], id: &str) -> Option<bool> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_bool())
}

/// The resolved value of a select descriptor: an accepted persisted selection,
/// else the descriptor default.
fn resolved_select_value(
    id: &str,
    options: &[agent::SelectOption],
    default_value: &Option<String>,
    selections: &[agent::OptionSelection],
) -> Option<String> {
    option_selection_str(selections, id)
        .filter(|v| options.iter().any(|o| &o.value == v))
        .map(str::to_string)
        .or_else(|| default_value.clone())
}

/// The traits chip label: every resolved descriptor label joined with " · "
/// (e.g. "High · 200k", "High · 200k · Fast", "Thinking Off"). `None` when the
/// model has no descriptors (S1 §3).
fn traits_chip_label(
    spec: &ModelSpec,
    selections: &[agent::OptionSelection],
    ultrathink_armed: bool,
) -> Option<String> {
    if spec.options.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for descriptor in &spec.options {
        match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
            } => {
                // An armed Ultrathink shows in the reasoning segment (it is not
                // persisted, so it does not resolve as an ordinary selection).
                if id == "reasoningEffort"
                    && ultrathink_armed
                    && let Some(o) = options.iter().find(|o| o.value == "ultrathink")
                {
                    parts.push(o.label.clone());
                    continue;
                }
                let part = resolved_select_value(id, options, default_value, selections)
                    .and_then(|value| options.iter().find(|o| o.value == value))
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| label.clone());
                parts.push(part);
            }
            OptionDescriptor::Boolean {
                id,
                label,
                default_value,
            } => {
                let on = option_selection_bool(selections, id).unwrap_or(*default_value);
                if id == "fastMode" {
                    parts.push(
                        crate::tr!(if on {
                            "composer.trait_fast"
                        } else {
                            "composer.trait_normal"
                        })
                        .into_owned(),
                    );
                } else {
                    let state = crate::tr!(if on { "composer.on" } else { "composer.off" });
                    parts.push(format!("{label} {state}"));
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Build the answers map for a user-input request: keyed by question id, with a
/// string (single-select / free-text) or string-array (multi-select) value. A
/// non-empty custom answer overrides the current question's selections (S1 §7).
/// A question counts as answered once it carries at least one selection (or a
/// recorded custom-text answer, which is stored the same way).
fn user_input_answered(
    question: &UserInputQuestion,
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    selections
        .get(&question.id)
        .is_some_and(|selected| !selected.is_empty())
}

fn user_input_all_answered(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    questions
        .iter()
        .all(|question| user_input_answered(question, selections))
}

/// The next unanswered question after `from`, wrapping around to earlier ones
/// (and back to `from` itself) so a skipped question is always revisited.
fn next_unanswered_question(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
    from: usize,
) -> Option<usize> {
    let total = questions.len();
    if total == 0 {
        return None;
    }
    (1..=total)
        .map(|step| (from + step) % total)
        .find(|&i| !user_input_answered(&questions[i], selections))
}

fn assemble_user_input_answers(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
    current_index: usize,
    custom_current: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (i, question) in questions.iter().enumerate() {
        if i == current_index
            && let Some(text) = custom_current.map(str::trim).filter(|t| !t.is_empty())
        {
            map.insert(
                question.id.clone(),
                serde_json::Value::String(text.to_string()),
            );
            continue;
        }
        let selected = selections.get(&question.id).cloned().unwrap_or_default();
        let value = if question.multi_select {
            serde_json::Value::Array(
                selected
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            )
        } else {
            serde_json::Value::String(selected.into_iter().next().unwrap_or_default())
        };
        map.insert(question.id.clone(), value);
    }
    map
}

fn current_model_name(catalog: &[ModelSpec], model: Option<&str>) -> String {
    match model {
        Some(id) => catalog
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| id.to_string()),
        None => crate::tr!("composer.default_model").into_owned(),
    }
}

/// The picker button's label: the resolved row's name (so a custom slug shows
/// its own name), else the catalog's, else the raw id.
fn current_model_name_resolved(
    resolved: &[crate::provider_models::ResolvedModel],
    catalog: &[ModelSpec],
    model: Option<&str>,
) -> String {
    let Some(id) = model else {
        return crate::tr!("composer.default_model").into_owned();
    };
    resolved
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| current_model_name(catalog, Some(id)))
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
                    format!(
                        "{} / {}",
                        context_meter::format_tokens(Some(used)),
                        context_meter::format_tokens(Some(window))
                    )
                }
                (Some(used), None) => context_meter::format_tokens(Some(used)),
                (None, Some(window)) => context_meter::format_tokens(Some(window)),
                (None, None) => crate::tr!("composer.context").into_owned(),
            }
        }
        None => crate::tr!("composer.context").into_owned(),
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
    use chrono::Timelike as _;

    #[test]
    fn bmp_and_tiff_transcode_to_decodable_png() {
        let source = image::DynamicImage::new_rgba8(2, 2);
        for format in [image::ImageFormat::Bmp, image::ImageFormat::Tiff] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            source.write_to(&mut encoded, format).unwrap();

            let png = transcode_image_to_png(&encoded.into_inner()).unwrap();
            assert_eq!(image::guess_format(&png).unwrap(), image::ImageFormat::Png);
            let decoded = image::load_from_memory(&png).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }

    fn thread(id: &str) -> ComposerDestination {
        ComposerDestination::Thread(id.to_string())
    }

    fn project_draft(id: &str) -> ComposerDestination {
        ComposerDestination::ProjectDraft(id.to_string())
    }

    #[test]
    fn composer_destination_uses_thread_id_or_stable_project_draft_key() {
        assert_eq!(
            composer_destination(false, "thread-a", Some("project-a")),
            Some(thread("thread-a"))
        );
        assert_eq!(
            composer_destination(true, "transient-draft-uuid-1", Some("project-a")),
            Some(project_draft("project-a"))
        );
        assert_eq!(
            composer_destination(true, "transient-draft-uuid-2", Some("project-a")),
            Some(project_draft("project-a"))
        );
    }

    #[test]
    fn composer_text_cache_restores_a_after_switching_a_to_b_to_a() {
        let mut cache = ComposerTextCache::default();

        assert_eq!(cache.switch_to(Some(thread("a")), ""), Some(String::new()));
        assert_eq!(
            cache.switch_to(Some(thread("b")), "text for a"),
            Some(String::new())
        );
        assert_eq!(
            cache.switch_to(Some(thread("a")), "text for b"),
            Some("text for a".to_string())
        );
        assert_eq!(cache.drafts.get(&thread("b")).unwrap(), "text for b");
    }

    #[test]
    fn composer_text_cache_isolates_two_project_new_thread_pages() {
        let mut cache = ComposerTextCache::default();

        assert_eq!(
            cache.switch_to(Some(project_draft("project-a")), ""),
            Some(String::new())
        );
        assert_eq!(
            cache.switch_to(Some(project_draft("project-b")), "draft for project a"),
            Some(String::new())
        );
        assert_eq!(
            cache.switch_to(Some(project_draft("project-a")), "draft for project b"),
            Some("draft for project a".to_string())
        );
        assert_eq!(
            cache.switch_to(Some(project_draft("project-b")), "draft for project a"),
            Some("draft for project b".to_string())
        );
    }

    #[test]
    fn composer_text_cache_first_visit_is_empty() {
        let mut cache = ComposerTextCache::default();

        assert_eq!(
            cache.switch_to(Some(thread("never-visited")), ""),
            Some(String::new())
        );
        assert_eq!(
            cache.switch_to(Some(thread("never-visited")), "typed"),
            None
        );
    }

    #[test]
    fn composer_text_cache_clears_only_the_submitted_destination() {
        let mut cache = ComposerTextCache::default();

        cache.switch_to(Some(thread("a")), "");
        cache.switch_to(Some(thread("b")), "text for a");
        cache.switch_to(Some(thread("a")), "text for b");
        cache.clear_current();

        assert!(!cache.drafts.contains_key(&thread("a")));
        assert_eq!(cache.drafts.get(&thread("b")).unwrap(), "text for b");
        assert_eq!(
            cache.switch_to(Some(thread("b")), ""),
            Some("text for b".to_string())
        );
        assert_eq!(
            cache.switch_to(Some(thread("a")), "text for b"),
            Some(String::new())
        );
    }

    #[test]
    fn orchestrate_prefix_requires_a_complete_command_token() {
        assert_eq!(strip_orchestrate_prefix("/orchestrate"), Some(""));
        assert_eq!(
            strip_orchestrate_prefix("/orchestrate   split the work"),
            Some("split the work")
        );
        assert_eq!(strip_orchestrate_prefix("/orchestrated"), None);
        assert_eq!(strip_orchestrate_prefix("hello /orchestrate"), None);
    }

    fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn parsed_later(text: &str, now: DateTime<Local>) -> (DateTime<Local>, String) {
        let (timestamp, message) = parse_later(text, now).unwrap().unwrap();
        (
            Local.timestamp_opt(timestamp as i64, 0).single().unwrap(),
            message,
        )
    }

    #[test]
    fn later_parses_wall_clock_as_the_next_strict_local_occurrence() {
        let now = local_time(2026, 8, 8, 12, 0);
        let (later_today, message) = parsed_later("/later 23:59 continue here", now);
        assert_eq!(later_today.date_naive(), now.date_naive());
        assert_eq!((later_today.hour(), later_today.minute()), (23, 59));
        assert_eq!(message, "continue here");

        let (tomorrow, message) = parsed_later("/later 5:10 first line\nsecond line", now);
        assert_eq!(tomorrow.date_naive(), now.date_naive().succ_opt().unwrap());
        assert_eq!((tomorrow.hour(), tomorrow.minute()), (5, 10));
        assert_eq!(message, "first line\nsecond line");
    }

    #[test]
    fn later_parses_all_relative_duration_units() {
        let now = local_time(2026, 8, 8, 12, 0);
        for (command, seconds, message) in [
            ("/later 5min multi word", 300, "multi word"),
            ("/later 30s soon", 30, "soon"),
            ("/later 2h much later", 7_200, "much later"),
        ] {
            let (fire_at, parsed_message) = parsed_later(command, now);
            assert_eq!((fire_at - now).num_seconds(), seconds);
            assert_eq!(parsed_message, message);
        }
    }

    #[test]
    fn later_reports_usage_errors_and_rejects_lookalikes() {
        let now = local_time(2026, 8, 8, 12, 0);
        for command in [
            "/later",
            "/later 5min",
            "/later 25:00 x",
            "/later 5:99 x",
            "/later abc x",
        ] {
            assert!(
                matches!(parse_later(command, now), Some(Err(_))),
                "{command}"
            );
        }
        assert_eq!(parse_later("/laters 5min x", now), None);
    }

    #[test]
    fn countdown_format_switches_at_one_hour() {
        assert_eq!(format_countdown(0), "0:00");
        assert_eq!(format_countdown(5), "0:05");
        assert_eq!(format_countdown(65), "1:05");
        assert_eq!(format_countdown(3_599), "59:59");
        assert_eq!(format_countdown(3_600), "1:00:00");
        assert_eq!(format_countdown(7_325), "2:02:05");
    }

    #[test]
    fn provider_menu_filter_is_fuzzy_kind_aware_and_uncapped() {
        let mut commands: Vec<ProviderCommand> = (0..60)
            .map(|index| ProviderCommand {
                name: format!("command-{index:02}"),
                description: Some(format!("Command {index}")),
                kind: ProviderCommandKind::Command,
            })
            .collect();
        commands.push(ProviderCommand {
            name: "deep-review".into(),
            description: None,
            kind: ProviderCommandKind::Skill,
        });

        let all = filter_provider_commands(&commands, ProviderCommandKind::Command, "");
        assert_eq!(all.len(), 60);
        assert_eq!(all[0].name, "command-00");
        assert_eq!(all[59].name, "command-59");

        // Fuzzy subsequence rather than prefix-only matching.
        let fuzzy = filter_provider_commands(&commands, ProviderCommandKind::Skill, "dprv");
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(fuzzy[0].name, "deep-review");
        assert!(
            filter_provider_commands(&commands, ProviderCommandKind::Command, "dprv").is_empty()
        );
    }

    #[test]
    fn context_label_variants() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
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
    fn approval_mode_meta_matches_ui_copy() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        assert_eq!(
            approval_mode_meta(ApprovalMode::Supervised),
            ("Supervised".to_string(), "icons/lock.svg")
        );
        assert_eq!(
            approval_mode_meta(ApprovalMode::AutoAcceptEdits),
            ("Auto-accept edits".to_string(), "icons/pencil.svg")
        );
        assert_eq!(
            approval_mode_meta(ApprovalMode::FullAccess),
            ("Full access".to_string(), "icons/unlock.svg")
        );
    }

    #[test]
    fn current_model_name_maps_catalog() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let catalog = vec![agent::ModelSpec {
            id: "claude-fable-5".into(),
            display_name: "Claude Fable 5".into(),
            is_default: false,
            options: Vec::new(),
        }];
        assert_eq!(current_model_name(&catalog, None), "Default");
        assert_eq!(
            current_model_name(&catalog, Some("claude-fable-5")),
            "Claude Fable 5"
        );
        // Unknown id falls back to the raw id.
        assert_eq!(current_model_name(&catalog, Some("gpt-9")), "gpt-9");
    }

    #[test]
    fn traits_chip_joins_descriptor_labels() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let spec = agent::ModelSpec {
            id: "claude-fable-5".into(),
            display_name: "Claude Fable 5".into(),
            is_default: false,
            options: vec![
                agent::OptionDescriptor::Select {
                    id: "reasoningEffort".into(),
                    label: "Reasoning".into(),
                    options: vec![
                        agent::SelectOption {
                            value: "high".into(),
                            label: "High".into(),
                            description: None,
                        },
                        agent::SelectOption {
                            value: "max".into(),
                            label: "Max".into(),
                            description: None,
                        },
                    ],
                    default_value: Some("high".into()),
                },
                agent::OptionDescriptor::Select {
                    id: "contextWindow".into(),
                    label: "Context Window".into(),
                    options: vec![
                        agent::SelectOption {
                            value: "200k".into(),
                            label: "200k".into(),
                            description: None,
                        },
                        agent::SelectOption {
                            value: "1m".into(),
                            label: "1M".into(),
                            description: None,
                        },
                    ],
                    default_value: Some("200k".into()),
                },
            ],
        };
        // Defaults resolve to "High · 200k".
        assert_eq!(
            traits_chip_label(&spec, &[], false),
            Some("High · 200k".into())
        );
        // A selection overrides the default.
        let sel = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::Value::String("1m".into()),
        }];
        assert_eq!(
            traits_chip_label(&spec, &sel, false),
            Some("High · 1M".into())
        );

        // Fast Mode boolean → Fast/Normal; a plain boolean → "<Label> On/Off".
        let fast = agent::ModelSpec {
            id: "m".into(),
            display_name: "m".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Boolean {
                id: "fastMode".into(),
                label: "Fast Mode".into(),
                default_value: false,
            }],
        };
        assert_eq!(traits_chip_label(&fast, &[], false), Some("Normal".into()));
        let thinking = agent::ModelSpec {
            id: "h".into(),
            display_name: "h".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Boolean {
                id: "thinking".into(),
                label: "Thinking".into(),
                default_value: false,
            }],
        };
        assert_eq!(
            traits_chip_label(&thinking, &[], false),
            Some("Thinking Off".into())
        );
        let unresolved = agent::ModelSpec {
            id: "u".into(),
            display_name: "u".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Thinking".into(),
                options: vec![agent::SelectOption {
                    value: "high".into(),
                    label: "High".into(),
                    description: None,
                }],
                default_value: None,
            }],
        };
        assert_eq!(
            traits_chip_label(&unresolved, &[], false),
            Some("Thinking".into())
        );
        // A model with no descriptors has no chip.
        let bare = agent::ModelSpec {
            id: "b".into(),
            display_name: "b".into(),
            is_default: false,
            options: Vec::new(),
        };
        assert_eq!(traits_chip_label(&bare, &[], false), None);
    }

    #[test]
    fn user_input_answer_flow_advances_to_unanswered_and_detects_completion() {
        let question = |id: &str| UserInputQuestion {
            id: id.into(),
            header: id.into(),
            question: id.into(),
            options: vec![agent::UserInputOption {
                label: "A".into(),
                description: String::new(),
            }],
            multi_select: false,
            prefill: None,
        };
        let questions = vec![question("q1"), question("q2"), question("q3")];
        let mut selections = std::collections::HashMap::new();

        // Nothing answered: from q1, the next stop is q2.
        assert!(!user_input_all_answered(&questions, &selections));
        assert_eq!(
            next_unanswered_question(&questions, &selections, 0),
            Some(1)
        );

        // q1 and q3 answered: from q1 the hop skips answered q3 and lands on q2;
        // an empty selection does not count as an answer.
        selections.insert("q1".into(), vec!["A".into()]);
        selections.insert("q2".into(), Vec::new());
        selections.insert("q3".into(), vec!["A".into()]);
        assert!(!user_input_all_answered(&questions, &selections));
        assert_eq!(
            next_unanswered_question(&questions, &selections, 2),
            Some(1)
        );

        // The wrap search revisits a skipped earlier question from the end.
        assert_eq!(
            next_unanswered_question(&questions, &selections, 0),
            Some(1)
        );

        // Everything answered → completion, and no hop target remains.
        selections.insert("q2".into(), vec!["custom text".into()]);
        assert!(user_input_all_answered(&questions, &selections));
        assert_eq!(next_unanswered_question(&questions, &selections, 0), None);

        assert_eq!(next_unanswered_question(&[], &selections, 0), None);
    }

    #[test]
    fn user_input_answers_assemble_with_multi_and_custom_override() {
        let questions = vec![
            UserInputQuestion {
                id: "q1".into(),
                header: "H1".into(),
                question: "Pick one".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "A".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "B".into(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
                prefill: None,
            },
            UserInputQuestion {
                id: "q2".into(),
                header: "H2".into(),
                question: "Pick many".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "X".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "Y".into(),
                        description: String::new(),
                    },
                ],
                multi_select: true,
                prefill: None,
            },
        ];
        let mut selections = std::collections::HashMap::new();
        selections.insert("q1".to_string(), vec!["A".to_string()]);
        selections.insert("q2".to_string(), vec!["X".to_string(), "Y".to_string()]);

        // No custom override: single-select → string, multi-select → array.
        let answers = assemble_user_input_answers(&questions, &selections, 0, None);
        assert_eq!(answers["q1"], serde_json::json!("A"));
        assert_eq!(answers["q2"], serde_json::json!(["X", "Y"]));

        // A custom answer overrides the current question's selection only.
        let answers = assemble_user_input_answers(&questions, &selections, 0, Some("  freehand  "));
        assert_eq!(answers["q1"], serde_json::json!("freehand"));
        assert_eq!(answers["q2"], serde_json::json!(["X", "Y"]));

        // A blank/whitespace custom answer does not override.
        let answers = assemble_user_input_answers(&questions, &selections, 0, Some("   "));
        assert_eq!(answers["q1"], serde_json::json!("A"));

        // An unanswered single-select yields an empty string.
        let answers =
            assemble_user_input_answers(&questions, &std::collections::HashMap::new(), 0, None);
        assert_eq!(answers["q1"], serde_json::json!(""));
        assert_eq!(answers["q2"], serde_json::json!([]));
    }
}
