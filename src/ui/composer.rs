//! The floating composer card: input, control row (model picker + context +
//! permission/mode chips + send/stop), the below-card checkout/branch row, and
//! the pending-approval panel (see docs/DESIGN.md "Composer").

use std::cell::Cell;
use std::rc::Rc;

use std::path::PathBuf;

use agent::{
    ApprovalDecision, ApprovalKind, ApprovalMode, ApprovalRequest, FileChangeKind, InteractionMode,
    ModelSpec, OptionDescriptor, ProviderKind, TokenUsage, UserInputQuestion,
};
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, ClipboardEntry, Context, Entity, EventEmitter,
    ExternalPaths, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, PathBuilder,
    Pixels, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div,
    img, point, prelude::FluentBuilder as _, px, rgb,
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

use crate::app::{AppState, TerminalContext, WorkspaceMode};
use crate::ui::attachments::{self, validate_attachment};
use crate::ui::composer_trigger::{
    ComposerTrigger, TriggerKind, detect_composer_trigger, serialize_composer_file_link,
};
use crate::ui::context_meter;
use crate::ui::workspace_walk::{PathEntry, filter_entries, list_workspace};

/// Blue-500 (normal meter) and red-500 (>90% overloaded), matching T3.
const METER_BLUE: u32 = 0x3B82F6;
const METER_RED: u32 = 0xEF4444;
/// Maximum rows shown in a trigger (`@`/`/`/`$`) menu.
const MENU_ROW_CAP: usize = 50;

fn normalize_terminal_context_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .trim_matches('\n')
        .to_string()
}

pub(crate) fn append_terminal_contexts_to_prompt(
    prompt: &str,
    contexts: &[TerminalContext],
) -> String {
    let prompt = prompt.trim();
    let mut lines = Vec::new();
    for context in contexts {
        let text = normalize_terminal_context_text(&context.text);
        if text.is_empty() || context.terminal_label.trim().is_empty() {
            continue;
        }
        let range = if context.line_start == context.line_end {
            format!("line {}", context.line_start)
        } else {
            format!("lines {}-{}", context.line_start, context.line_end)
        };
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("- {} {}:", context.terminal_label.trim(), range));
        lines.extend(
            text.lines()
                .enumerate()
                .map(|(index, line)| format!("  {} | {}", context.line_start + index, line)),
        );
    }
    if lines.is_empty() {
        return prompt.to_string();
    }
    let block = format!("<terminal_context>\n{}\n</terminal_context>", lines.join("\n"));
    if prompt.is_empty() {
        block
    } else {
        format!("{prompt}\n\n{block}")
    }
}

/// Claude's warm brand tint for the starburst glyph.
const CLAUDE_TINT: u32 = 0xD97757;
/// T3's circular stop button red-orange.
const STOP_TINT: u32 = 0xF4562E;
/// Below this measured control-row width the row collapses its context /
/// permission / mode chips into a "⋯" overflow popover so nothing spills past
/// the card edge (diff panel open, or a small window).
const CONTROL_ROW_COMPACT_BELOW: f32 = 520.;

/// One selectable model in the picker (a catalog [`ModelSpec`] row).
#[derive(Clone)]
struct ModelRow {
    /// Provider-native model id (the favorites key + selection value).
    id: String,
    /// Display name.
    name: String,
    provider: ProviderKind,
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
        ProviderKind::ClaudeCode => Icon::empty()
            .path("icons/claude.svg")
            .text_color(rgb(CLAUDE_TINT)),
        ProviderKind::Codex => Icon::empty().path("icons/openai.svg"),
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

fn approval_mode_meta(mode: ApprovalMode) -> (String, String, &'static str) {
    let (_, label_key, description_key, icon) = APPROVAL_MODES
        .iter()
        .find(|(m, ..)| *m == mode)
        .expect("every ApprovalMode is present in APPROVAL_MODES");
    (
        rust_i18n::t!(*label_key).into_owned(),
        rust_i18n::t!(*description_key).into_owned(),
        icon,
    )
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

/// The minimal `/`-command set this slice handles (S1 §7).
enum SlashCommand {
    Plan,
    Default,
    Model,
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

/// A pending image attachment: validated, persisted to the session attachments
/// dir, and shown in the composer thumbnail strip. Kept per active session.
#[derive(Clone)]
struct PendingImage {
    /// On-disk path of the persisted copy (also the thumbnail image source).
    path: PathBuf,
    /// Display name.
    name: String,
}

/// Which glyph a trigger-menu row shows.
#[derive(Clone, Copy)]
enum MenuIcon {
    File,
    Folder,
    Command,
    /// Skill rows — wired for when provider skills become reachable (the agent
    /// crate is frozen; no skills are listed today, see the reported gap).
    #[allow(dead_code)]
    Skill,
}

/// What accepting a trigger-menu row does.
#[derive(Clone)]
enum MenuAccept {
    /// Insert the serialized `[basename](path)` mention for this relative path.
    InsertPath(String),
    /// Insert `$<name> ` for this skill. Wired for when provider skills become
    /// reachable (agent crate frozen — see reported contract gap).
    #[allow(dead_code)]
    InsertSkill(String),
    /// Insert `/<name> ` for this provider command. Wired for when provider
    /// slash commands become reachable (agent crate frozen — see reported gap).
    #[allow(dead_code)]
    InsertCommand(String),
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
}

/// T3's provider display name (used by the context meter's compaction line).
fn provider_display_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "Claude",
        ProviderKind::Codex => "Codex",
    }
}

/// Guess a file extension for a persisted attachment from its MIME type,
/// falling back to the source name's extension, then `png`.
fn image_extension(mime: &str, name: &str) -> String {
    let from_mime = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" | "image/tif" => "tiff",
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

/// Best-effort MIME type from a file extension (for drag/drop of image files).
fn mime_from_path(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub struct Composer {
    app_state: Entity<AppState>,
    input: Entity<InputState>,
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
    /// Index of the image shown in the expanded-preview overlay, if any.
    image_preview: Option<usize>,
    /// Whether the one-shot screenshot debug seed has been applied.
    debug_applied: bool,
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
                .placeholder(rust_i18n::t!("composer.placeholder"))
        });
        let model_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("composer.search_models"))
        });

        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, input, event, window, cx| {
                match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        // Enter accepts the highlighted trigger-menu row when the
                        // menu is open, otherwise submits the turn.
                        if this.menu_visible(cx) {
                            this.accept_menu(this.menu_highlight, window, cx);
                        } else {
                            let input = input.clone();
                            this.submit(&input, window, cx);
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
            ui_request_id: None,
            ui_question_index: 0,
            ui_selections: std::collections::HashMap::new(),
            applied_placeholder: rust_i18n::t!("composer.placeholder").into_owned(),
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
            image_preview: None,
            debug_applied: false,
            _subscriptions: subscriptions,
        }
    }

    /// Apply the one-shot screenshot debug seed (`--debug-compose` / `--debug-image`).
    fn apply_debug_seed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug_applied {
            return;
        }
        let (compose, image) = {
            let state = self.app_state.read(cx);
            (state.debug_compose.clone(), state.debug_image.clone())
        };
        if compose.is_none() && image.is_none() {
            // Nothing to seed, but only latch once a session exists so the seed
            // survives the initial empty frame.
            if self.app_state.read(cx).active.is_some() {
                self.debug_applied = true;
            }
            return;
        }
        if self.app_state.read(cx).active.is_none() {
            return;
        }
        self.debug_applied = true;
        if let Some(text) = compose {
            let len = text.len();
            self.input.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_selected_range(len..len, cx);
            });
            self.recompute_trigger(cx);
        }
        if let Some(path) = image {
            self.add_image_path(path, window, cx);
        }
        cx.notify();
    }

    fn submit(&mut self, input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        // While a user-input question is pending, Enter is captured by the
        // question panel; normal send is suppressed (S1 §7).
        if self.pending_user_input(cx).is_some() {
            return;
        }
        let text = input.read(cx).value().trim().to_string();
        let has_images = !self.pending_images.is_empty();
        let terminal_contexts = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|active| active.terminal_workspace.contexts.clone())
            .unwrap_or_default();
        if text.is_empty() && !has_images && terminal_contexts.is_empty() {
            return;
        }
        if self.app_state.read(cx).active.is_none() {
            window.push_notification(Notification::info(rust_i18n::t!("composer.no_session")), cx);
            return;
        }
        // Intercept the minimal `/`-command set (S1 §4/§7): `/plan` and
        // `/default` switch mode and are stripped; `/model` opens the picker.
        if terminal_contexts.is_empty() && let Some(command) = slash_command(&text) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            match command {
                SlashCommand::Plan => self
                    .app_state
                    .update(cx, |state, cx| state.set_interaction_mode(InteractionMode::Plan, cx)),
                SlashCommand::Default => self.app_state.update(cx, |state, cx| {
                    state.set_interaction_mode(InteractionMode::Build, cx)
                }),
                SlashCommand::Model => {
                    self.model_picker_token = self.model_picker_token.wrapping_add(1);
                }
            }
            cx.notify();
            return;
        }
        let text = append_terminal_contexts_to_prompt(&text, &terminal_contexts);
        input.update(cx, |state, cx| state.set_value("", window, cx));
        // Image-only messages get T3's exact synthetic text. Attachments are
        // persisted on disk (see `add_image_*`); the send wire currently carries
        // only text, so the images themselves are not transmitted (contract gap:
        // `SessionCommand::SendTurn` needs an `attachments` field — see report).
        let sent_text = if text.is_empty() && has_images {
            attachments::image_only_message().to_string()
        } else {
            text
        };
        self.pending_images.clear();
        self.image_preview = None;
        self.app_state.update(cx, |state, cx| {
            if let Some(active) = state.active.as_mut() {
                active.terminal_workspace.contexts.clear();
            }
            state.send_turn(sent_text, cx)
        });
        cx.emit(ComposerEvent::Submitted);
        cx.notify();
    }

    // -- inline triggers (@ mentions / $ skills / commands) ----------------

    /// Whether a trigger menu should currently be shown.
    fn menu_visible(&self, _cx: &App) -> bool {
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
        let Some(cwd) = self.app_state.read(cx).active_cwd() else {
            return;
        };
        if self.workspace_loading || self.workspace.as_ref().is_some_and(|(c, _)| *c == cwd) {
            return;
        }
        self.workspace_loading = true;
        cx.spawn(async move |this, cx| {
            let walked = {
                let cwd = cwd.clone();
                smol::unblock(move || list_workspace(&cwd)).await
            };
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
    fn menu_rows(&self, _cx: &App) -> (Vec<MenuRow>, String, bool) {
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
                let rows = filter_entries(entries, &trigger.query, MENU_ROW_CAP)
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
                    })
                    .collect();
                let loading = self.workspace_loading && self.workspace.is_none();
                (rows, rust_i18n::t!("composer.no_files").into_owned(), loading)
            }
            TriggerKind::Skill => {
                // Provider skills are not reachable without an agent-crate change
                // (frozen). List nothing; show T3's exact empty copy.
                (
                    Vec::new(),
                    rust_i18n::t!("composer.no_skills").into_owned(),
                    false,
                )
            }
            TriggerKind::SlashCommand | TriggerKind::SlashModel => {
                let query = trigger.query.to_lowercase();
                let builtins: [(&str, &str, MenuAccept); 3] = [
                    (
                        "model",
                        "composer.cmd_model_desc",
                        MenuAccept::OpenModelPicker,
                    ),
                    (
                        "plan",
                        "composer.cmd_plan_desc",
                        MenuAccept::SetMode(InteractionMode::Plan),
                    ),
                    (
                        "default",
                        "composer.cmd_default_desc",
                        MenuAccept::SetMode(InteractionMode::Build),
                    ),
                ];
                let rows = builtins
                    .into_iter()
                    .filter(|(name, _, _)| query.is_empty() || name.starts_with(&query))
                    .map(|(name, desc, accept)| MenuRow {
                        primary: format!("/{name}"),
                        secondary: rust_i18n::t!(desc).into_owned(),
                        icon: MenuIcon::Command,
                        accept,
                    })
                    .collect();
                // Provider-native commands (Claude slash commands / Codex skills)
                // require an agent-crate change (frozen); none are listed here.
                (rows, rust_i18n::t!("composer.no_command").into_owned(), false)
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
            MenuAccept::OpenModelPicker => {
                self.replace_trigger("", window, cx);
                self.model_picker_token = self.model_picker_token.wrapping_add(1);
            }
            MenuAccept::SetMode(mode) => {
                let mode = *mode;
                self.replace_trigger("", window, cx);
                self.app_state
                    .update(cx, |state, cx| state.set_interaction_mode(mode, cx));
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
        let id = self
            .app_state
            .read(cx)
            .active_session_id()
            .map(str::to_string);
        if id != self.images_session {
            self.images_session = id;
            self.pending_images.clear();
            self.image_preview = None;
        }
    }

    /// Validate `bytes` against the type/size/count limits and, if ok, persist a
    /// copy to the session attachments dir and add it to the pending strip.
    fn add_image_bytes(
        &mut self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) =
            validate_attachment(&name, &mime, bytes.len() as u64, self.pending_images.len())
        {
            window.push_notification(Notification::error(err.message()), cx);
            return;
        }
        let ext = image_extension(&mime, &name);
        match self.app_state.read(cx).save_attachment(&bytes, &ext) {
            Ok(path) => {
                self.pending_images.push(PendingImage { path, name });
                cx.notify();
            }
            Err(err) => window.push_notification(
                Notification::error(
                    rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                ),
                cx,
            ),
        }
    }

    /// Add an image from a dropped file path (reads + re-validates it).
    fn add_image_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let mime = mime_from_path(&path);
        match std::fs::read(&path) {
            Ok(bytes) => self.add_image_bytes(name, mime, bytes, window, cx),
            Err(err) => window.push_notification(
                Notification::error(
                    rust_i18n::t!("errors.persist_event", error = err).into_owned(),
                ),
                cx,
            ),
        }
    }

    /// Pull an image off the clipboard (⌘V with image content), if present.
    fn paste_clipboard_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => {
                    let mime = image.format().mime_type().to_string();
                    let bytes = image.bytes().to_vec();
                    self.add_image_bytes("pasted-image".to_string(), mime, bytes, window, cx);
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    for path in paths.paths() {
                        if mime_from_path(path).starts_with("image/") {
                            self.add_image_path(path.clone(), window, cx);
                        }
                    }
                }
                ClipboardEntry::String(_) => {}
            }
        }
    }

    fn remove_image(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.pending_images.len() {
            let removed = self.pending_images.remove(index);
            let _ = std::fs::remove_file(&removed.path);
            if self.image_preview == Some(index) {
                self.image_preview = None;
            }
            cx.notify();
        }
    }

    /// The active session's pending user-input request, if any.
    fn pending_user_input(&self, cx: &App) -> Option<(String, Vec<UserInputQuestion>)> {
        self.app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.timeline.pending_user_input.clone())
    }

    /// The rail the picker shows: an explicit user choice, else Favorites when
    /// any favorites exist (S1 §2), else the active provider.
    fn rail_for(&self, provider: ProviderKind, has_favorites: bool) -> PickerRail {
        self.picker_rail.unwrap_or({
            if has_favorites {
                PickerRail::Favorites
            } else {
                PickerRail::Provider(provider)
            }
        })
    }

    // -- control-row popovers ----------------------------------------------

    /// The model-picker button + popover (anchored above, ~360px).
    fn render_model_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let app_state = self.app_state.read(cx);
        let (provider, current_model) = match &app_state.active {
            Some(active) => (active.meta.provider, active.meta.model.clone()),
            None => return div().into_any_element(),
        };
        let catalog = app_state.models_for(provider);
        let display = current_model_name(catalog, current_model.as_deref());

        // Build the filtered + favorites-first row list for the current frame.
        // Favorites open first when any exist (S1 §2).
        let query = self.model_search.read(cx).value().to_lowercase();
        let has_favorites = app_state
            .model_catalogs
            .values()
            .flatten()
            .any(|m| app_state.is_favorite_model(&m.id));
        let rail = self.rail_for(provider, has_favorites);
        let all_rows: Vec<ModelRow> = match rail {
            PickerRail::Favorites => app_state
                .model_catalogs
                .iter()
                .flat_map(|(p, models)| {
                    models.iter().map(move |m| ModelRow {
                        id: m.id.clone(),
                        name: m.display_name.clone(),
                        provider: *p,
                    })
                })
                .filter(|r| app_state.is_favorite_model(&r.id))
                .collect(),
            PickerRail::Provider(p) => app_state
                .models_for(p)
                .iter()
                .map(|m| ModelRow {
                    id: m.id.clone(),
                    name: m.display_name.clone(),
                    provider: p,
                })
                .collect(),
        };
        let mut rows: Vec<ModelRow> = all_rows
            .into_iter()
            .filter(|r| query.is_empty() || r.name.to_lowercase().contains(&query))
            .collect();
        rows.sort_by_key(|r| !app_state.is_favorite_model(&r.id));
        let loading = app_state.models_loading(provider)
            && matches!(rail, PickerRail::Provider(_))
            && rows.is_empty()
            && query.is_empty();

        let composer = cx.entity();
        let app_entity = self.app_state.clone();
        let model_search = self.model_search.clone();
        let pending_restart = app_state.model_pending_restart();
        let selected = current_model.clone();

        let trigger = Button::new("model-picker").ghost().compact().child(
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

        Popover::new(("model-picker-popover", self.model_picker_token))
            .anchor(Anchor::BottomLeft)
            .default_open(self.model_picker_token > 0)
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
                    loading,
                    &app_entity,
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
        let app_state = self.app_state.read(cx);
        let Some(spec) = app_state.active_model_spec() else {
            return div().into_any_element();
        };
        let selections = app_state.active_option_selections();
        let ultrathink_armed = app_state.ultrathink_armed();
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
        let pending_restart = app_state.options_pending_restart();

        let trigger = Button::new("traits-chip").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );

        let app_entity = self.app_state.clone();
        Popover::new("traits-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_traits_pane(
                    &spec,
                    &selections,
                    ultrathink_armed,
                    locked,
                    pending_restart,
                    &app_entity,
                    &cx.entity(),
                    cx,
                )
            })
            .into_any_element()
    }

    /// The Build/Plan interaction-mode chip (S1 §4).
    fn render_mode_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.app_state.read(cx).active_interaction_mode();
        let muted = cx.theme().muted_foreground;
        let (icon, label, tooltip) = match mode {
            InteractionMode::Build => (
                "icons/box.svg",
                rust_i18n::t!("composer.build"),
                rust_i18n::t!("composer.build_tooltip"),
            ),
            InteractionMode::Plan => (
                "icons/ruler.svg",
                rust_i18n::t!("composer.plan"),
                rust_i18n::t!("composer.plan_tooltip"),
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
                this.app_state
                    .update(cx, |state, cx| state.toggle_interaction_mode(cx));
            }))
            .into_any_element()
    }

    /// The circular context-window meter (ring showing used%, red > 90%) + a
    /// hover/click popover (T3's `ContextWindowMeter`).
    fn render_context_meter(&self, cx: &mut Context<Self>) -> AnyElement {
        let (usage, provider) = {
            let state = self.app_state.read(cx);
            (
                state.active.as_ref().and_then(|a| a.timeline.usage),
                state.active.as_ref().map(|a| a.meta.provider),
            )
        };
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

        Popover::new("context-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| render_context_meter_pane(usage, provider, pct, cx))
            .into_any_element()
    }

    /// The approval-mode selector: a chip showing the current mode (icon +
    /// label) opening a popover of the three modes (icon + bold name + muted
    /// description, ✓ on the current one).
    fn render_permission_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.app_state.read(cx).active_approval_mode();
        let (label, _, icon_path) = approval_mode_meta(current);
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

        let app_entity = self.app_state.clone();
        let pending_restart = self.app_state.read(cx).approval_pending_restart();
        Popover::new("permission-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| {
                render_permission_pane(current, pending_restart, &app_entity, &cx.entity(), cx)
            })
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
        let mode = self.app_state.read(cx).active_approval_mode();
        let interaction = self.app_state.read(cx).active_interaction_mode();

        let trigger = Button::new("overflow-controls")
            .ghost()
            .compact()
            .tooltip(rust_i18n::t!("composer.more_controls"))
            .child(Icon::new(IconName::Ellipsis).small().text_color(muted));

        Popover::new("overflow-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_, _, cx| render_overflow_pane(usage, mode, interaction, cx))
            .into_any_element()
    }

    // -- trigger menu + image strip ----------------------------------------

    /// The floating `@`/`/`/`$` menu, rendered in-flow just above the composer
    /// card. `None` when no trigger is active (or it was dismissed).
    fn render_trigger_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.menu_visible(cx) {
            return None;
        }
        let (rows, empty_text, loading) = self.menu_rows(cx);
        let muted = cx.theme().muted_foreground;
        let highlight = self.menu_highlight.min(rows.len().saturating_sub(1));

        let mut list = v_flex().w_full().p_1().gap_0p5();
        if rows.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_2p5()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(if loading {
                        rust_i18n::t!("composer.searching").into_owned()
                    } else {
                        empty_text
                    }),
            );
        } else {
            for (index, row) in rows.iter().enumerate() {
                let is_active = index == highlight;
                let icon = match row.icon {
                    MenuIcon::File => Icon::empty().path("icons/file.svg"),
                    MenuIcon::Folder => Icon::empty().path("icons/folder-closed.svg"),
                    MenuIcon::Command => Icon::empty().path("icons/box.svg"),
                    MenuIcon::Skill => Icon::empty().path("icons/ruler.svg"),
                };
                list = list.child(
                    h_flex()
                        .id(("menu-row", index))
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
                                    .text_size(px(12.))
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
                .w_full()
                .max_h(px(288.))
                .overflow_hidden()
                .rounded(px(12.))
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .shadow_lg()
                .child(list)
                .into_any_element(),
        )
    }

    /// The 64px thumbnail strip above the control row (T3), when images are
    /// attached; each thumbnail opens an expanded preview and has a remove `x`.
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
                    .rounded(px(8.))
                    .overflow_hidden()
                    .border_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .child(img(path).size(px(64.)).rounded(px(8.)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.image_preview = Some(index);
                        cx.notify();
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
                            .child(Icon::new(IconName::Close).xsmall().text_color(gpui::white()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_image(index, cx);
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
    }

    /// The expanded image preview overlay (click backdrop / `x` to close).
    fn render_image_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let index = self.image_preview?;
        let image = self.pending_images.get(index)?;
        let path = image.path.clone();
        let name = image.name.clone();
        Some(
            div()
                .id("image-preview")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.7))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.image_preview = None;
                    cx.notify();
                }))
                .child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .max_w(px(420.))
                                .max_h(px(420.))
                                .rounded(px(12.))
                                .overflow_hidden()
                                .child(img(path).max_w(px(420.)).max_h(px(420.))),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(gpui::white())
                                .child(name),
                        ),
                )
                .into_any_element(),
        )
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

        // Group C: while the first send is creating a worktree, show a disabled
        // "Preparing worktree…" pill instead of the send button.
        if self.app_state.read(cx).preparing_worktree() {
            return h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small().color(cx.theme().primary))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(rust_i18n::t!("composer.preparing_worktree")),
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

    /// The composer's primary control: the stop button while a turn runs, the
    /// Refine / Implement (split) controls in the plan-ready state, else send.
    fn render_primary_action(&self, turn_running: bool, cx: &mut Context<Self>) -> AnyElement {
        if turn_running {
            return self.render_send_or_stop(true, cx);
        }
        if self.app_state.read(cx).plan_ready_markdown().is_some() {
            let has_text = !self.input.read(cx).value().trim().is_empty();
            if has_text {
                // Refine: send the feedback and stay in Plan mode (a normal send
                // while the session is in Plan mode continues planning).
                return Button::new("plan-refine")
                    .primary()
                    .label(rust_i18n::t!("plan.refine"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let input = this.input.clone();
                        this.submit(&input, window, cx);
                    }))
                    .into_any_element();
            }
            return self.render_implement_split(cx);
        }
        self.render_send_or_stop(turn_running, cx)
    }

    /// The Implement split-button: primary "Implement" + a chevron menu with
    /// "Implement in a new thread" (S1 §5).
    fn render_implement_split(&self, cx: &mut Context<Self>) -> AnyElement {
        let primary = cx.theme().primary;
        let fg = cx.theme().primary_foreground;
        let app_main = self.app_state.clone();

        let chevron = Popover::new("implement-menu")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("implement-menu-trigger")
                    .primary()
                    .compact()
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let app = cx.entity();
                let app_state = app_main.clone();
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
                            .child(rust_i18n::t!("plan.implement_new_thread"))
                            .on_click(move |_, window, cx| {
                                app_state.update(cx, |state, cx| {
                                    state.implement_plan_in_new_thread(cx)
                                });
                                let _ = &app;
                                popover.update(cx, |st, cx| st.dismiss(window, cx));
                            }),
                    )
                    .into_any_element()
            });

        let app_impl = self.app_state.clone();
        h_flex()
            .flex_none()
            .h(px(32.))
            .items_center()
            .rounded(px(8.))
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
                    .child(rust_i18n::t!("plan.implement"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        app_impl.update(cx, |state, cx| state.implement_plan(cx));
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(fg).opacity(0.3))
            .child(chevron)
            .into_any_element()
    }

    /// The "Plan Ready" header strip shown atop the composer while a proposed
    /// plan awaits a decision (S1 §5).
    fn render_plan_ready_header(&self, title: String, cx: &mut Context<Self>) -> AnyElement {
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
                    .child(rust_i18n::t!("plan.ready")),
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
            .into_any_element()
    }

    // -- user-input question panel (S1 §7) ---------------------------------

    /// Keep the per-request question state in sync: reset the index/selections
    /// when a new request arrives (or the pending one resolves).
    fn sync_user_input_state(&mut self, cx: &mut Context<Self>) {
        let current = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .and_then(|a| a.timeline.pending_user_input.as_ref().map(|(id, _)| id.clone()));
        if current != self.ui_request_id {
            self.ui_request_id = current;
            self.ui_question_index = 0;
            self.ui_selections.clear();
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
        let selected = self.ui_selections.get(&question.id).cloned().unwrap_or_default();

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
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(rust_i18n::t!(
                            "userinput.question_count",
                            index = index + 1,
                            total = total
                        )),
                )
            });

        // Option rows.
        let mut options = v_flex().w_full().gap_1();
        for (opt_index, option) in question.options.iter().enumerate() {
            let is_selected = selected.iter().any(|l| l == &option.label);
            let label = option.label.clone();
            let question_for_click = question.clone();
            let questions_for_click = questions.clone();
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
            options = options.child(
                h_flex()
                    .id(("ui-opt", opt_index))
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
                                    .child(div().flex_none().text_color(muted).child(format!(
                                        "{}",
                                        opt_index + 1
                                    )))
                                    .child(div().font_medium().child(option.label.clone())),
                            )
                            .when(!option.description.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(muted)
                                        .child(option.description.clone()),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.ui_toggle_option(&question_for_click, label.clone(), cx);
                        // Single-select auto-advances to the next question.
                        if !multi {
                            this.ui_auto_advance(&questions_for_click, String::new(), cx);
                        }
                    })),
            );
        }

        // Actions row.
        let is_last = index + 1 >= total;
        let submit_label = if !multi && total == 1 {
            rust_i18n::t!("userinput.submit_answer")
        } else {
            rust_i18n::t!("userinput.submit_answers")
        };
        let questions_submit = questions.clone();
        let request_submit = request_id.clone();
        let mut actions = h_flex().w_full().gap_2().items_center();
        if index > 0 {
            actions = actions.child(
                Button::new("ui-prev")
                    .ghost()
                    .small()
                    .label(rust_i18n::t!("userinput.previous"))
                    .on_click(cx.listener(|this, _, _, cx| this.ui_go(-1, cx))),
            );
        }
        actions = actions.child(div().flex_1());
        if !is_last {
            actions = actions.child(
                Button::new("ui-next")
                    .outline()
                    .small()
                    .label(rust_i18n::t!("userinput.next_question"))
                    .on_click(cx.listener(|this, _, _, cx| this.ui_go(1, cx))),
            );
        }
        actions = actions.child(
            Button::new("ui-submit")
                .primary()
                .small()
                .label(submit_label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.ui_submit(&questions_submit, request_submit.clone(), window, cx);
                })),
        );

        // Number keys 1-9 select the matching option.
        let question_keys = question.clone();
        let questions_keys = questions.clone();

        v_flex()
            .w_full()
            .gap_2()
            .p(px(14.))
            .rounded(px(12.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_sm()
            .on_key_down(cx.listener(move |this, ev: &gpui::KeyDownEvent, _, cx| {
                if let Ok(n) = ev.keystroke.key.parse::<usize>() {
                    if n >= 1 && n <= question_keys.options.len() {
                        let label = question_keys.options[n - 1].label.clone();
                        this.ui_toggle_option(&question_keys, label, cx);
                        if !question_keys.multi_select {
                            this.ui_auto_advance(&questions_keys, String::new(), cx);
                        }
                    }
                }
            }))
            .child(header)
            .child(
                div()
                    .text_size(px(14.))
                    .child(question.question.clone()),
            )
            .child(options)
            .when(multi, |this| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(rust_i18n::t!("userinput.multi_hint")),
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

    fn ui_go(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = self.ui_question_index as i32 + delta;
        if next >= 0 {
            self.ui_question_index = next as usize;
            cx.notify();
        }
    }

    /// Single-select auto-advance (~200ms) to the next question (S1 §7).
    fn ui_auto_advance(&mut self, questions: &[UserInputQuestion], _req: String, cx: &mut Context<Self>) {
        let total = questions.len();
        let at = self.ui_question_index;
        if at + 1 >= total {
            return;
        }
        cx.spawn(async move |this, cx| {
            smol::Timer::after(std::time::Duration::from_millis(200)).await;
            let _ = this.update(cx, |this, cx| {
                if this.ui_question_index == at && at + 1 < total {
                    this.ui_question_index = at + 1;
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
        let custom = self.input.read(cx).value().trim().to_string();
        let custom = if custom.is_empty() {
            None
        } else {
            Some(custom.as_str())
        };
        let answers =
            assemble_user_input_answers(questions, &self.ui_selections, self.ui_question_index, custom);
        self.input.update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_selections.clear();
        self.ui_question_index = 0;
        self.app_state
            .update(cx, |state, cx| state.respond_user_input(request_id, answers, cx));
        cx.notify();
    }

    // -- below-card + approval ---------------------------------------------

    fn render_checkout_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (branch, branches, turn_running, is_draft, worktree_base, worktree) = {
            let state = self.app_state.read(cx);
            let active = state.active.as_ref()?;
            // Worktrees have a `.git` file (not dir), so `read_git_branch` yields
            // None; fall back to the recorded worktree branch so the row shows.
            let branch = active
                .git_branch
                .clone()
                .or_else(|| active.meta.worktree.as_ref().map(|w| w.branch.clone()))?;
            // The base the worktree draft will branch from (Some in worktree mode).
            let worktree_base = match state.draft_workspace_mode() {
                Some(WorkspaceMode::NewWorktree { base }) => Some(base),
                _ => None,
            };
            (
                branch,
                active.branches.clone(),
                active.timeline.turn_running,
                active.draft,
                worktree_base,
                active.meta.worktree.clone(),
            )
        };
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
                .tooltip(rust_i18n::t!("composer.wait_turn"))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                        .child(picker_current.clone()),
                )
                .into_any_element()
        } else {
            let app_open = self.app_state.clone();
            let app_content = self.app_state.clone();
            let current = picker_current.clone();
            let trigger = Button::new("branch-picker").ghost().compact().child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                    .child(picker_current.clone())
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
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
                    if worktree_mode {
                        col = col.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_size(px(11.))
                                .font_medium()
                                .text_color(muted)
                                .child(rust_i18n::t!("composer.worktree_base")),
                        );
                    }
                    if branches.is_empty() {
                        col = col.child(
                            div()
                                .px_2()
                                .py_1p5()
                                .text_size(px(13.))
                                .text_color(muted)
                                .child(rust_i18n::t!("composer.loading")),
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
                                        app_pick.update(cx, |state, cx| {
                                            if worktree_mode {
                                                // Choose the worktree's base branch.
                                                state.set_draft_workspace(
                                                    WorkspaceMode::NewWorktree { base: branch_name },
                                                    cx,
                                                );
                                            } else {
                                                state.checkout_branch(branch_name, cx);
                                            }
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

        // Left: the workspace-mode chip. A draft can pick "Local checkout" vs
        // "New worktree"; a started session shows its locked workspace.
        let left = self.render_workspace_chip(is_draft, worktree_mode, worktree.is_some(), &branch, cx);

        Some(
            h_flex()
                .w_full()
                .px_2()
                .pt_2()
                .items_center()
                .justify_between()
                .text_size(px(12.))
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
            rust_i18n::t!("composer.new_worktree")
        } else {
            rust_i18n::t!("composer.local_checkout")
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

        let app_content = self.app_state.clone();
        let base_default = base_default.to_string();
        let trigger = Button::new("workspace-picker").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(12.))
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );
        Popover::new("workspace-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let popover = cx.entity();
                let app_local = app_content.clone();
                let app_worktree = app_content.clone();
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
                            .child(rust_i18n::t!("composer.workspace")),
                    )
                    .child(
                        workspace_row(
                            rust_i18n::t!("composer.local_checkout").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-local")
                        .on_click(move |_, window, cx| {
                            app_local.update(cx, |state, cx| {
                                state.set_draft_workspace(WorkspaceMode::LocalCheckout, cx);
                            });
                            pop_local.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        workspace_row(
                            rust_i18n::t!("composer.new_worktree").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-worktree")
                        .on_click(move |_, window, cx| {
                            let base = base.clone();
                            app_worktree.update(cx, |state, cx| {
                                state.set_draft_workspace(
                                    WorkspaceMode::NewWorktree { base },
                                    cx,
                                );
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
            ApprovalKind::ExecCommand { .. } => rust_i18n::t!("approval.command_requested"),
            ApprovalKind::FileRead { .. } => rust_i18n::t!("approval.file_read_requested"),
            ApprovalKind::FileChange { .. } => rust_i18n::t!("approval.file_requested"),
            ApprovalKind::ToolUse { .. } => rust_i18n::t!("approval.tool_requested"),
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
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(rust_i18n::t!("approval.in_directory", cwd = cwd)),
                    )
                })
                .into_any_element(),
            ApprovalKind::FileChange { changes, .. } => v_flex()
                .gap_0p5()
                .children(changes.iter().map(|change| {
                    div()
                        .text_size(px(12.5))
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!(
                            "{} {}",
                            file_change_kind_label(change.kind),
                            change.path
                        ))
                }))
                .into_any_element(),
            ApprovalKind::FileRead { detail } => div()
                .text_size(px(12.5))
                .font_family(cx.theme().mono_font_family.clone())
                .child(detail.clone())
                .into_any_element(),
            ApprovalKind::ToolUse { name, input, .. } => div()
                .text_size(px(12.5))
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
                            .child(rust_i18n::t!("approval.pending")),
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
                        .w_full()
                        .p_2()
                        .rounded(px(8.))
                        .bg(cx.theme().muted)
                        .child(detail),
                )
            })
            .child(
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
                            .label(rust_i18n::t!("approval.cancel_turn"))
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
                            .label(rust_i18n::t!("approval.decline"))
                            .text_color(cx.theme().danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(deny_id.clone(), ApprovalDecision::Deny, cx);
                            })),
                    )
                    .child(
                        Button::new("approval-always")
                            .ghost()
                            .small()
                            .label(rust_i18n::t!("approval.always_allow_session"))
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
                            .label(rust_i18n::t!("approval.approve_once"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(approve_id.clone(), ApprovalDecision::Approve, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn respond(&mut self, request_id: String, decision: ApprovalDecision, cx: &mut Context<Self>) {
        self.approval_expanded = false;
        self.app_state.update(cx, |state, cx| {
            state.respond_approval(request_id, decision, cx)
        });
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_user_input_state(cx);
        self.sync_images_session(cx);
        self.apply_debug_seed(window, cx);
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
        let plan_ready_title = {
            let state = self.app_state.read(cx);
            state.plan_ready_markdown().map(|md| {
                crate::session::plan_title(&md)
                    .unwrap_or_else(|| rust_i18n::t!("plan.proposed_plan").into_owned())
            })
        };
        let desired_placeholder = if plan_ready_title.is_some() {
            rust_i18n::t!("plan.refine_placeholder").into_owned()
        } else {
            rust_i18n::t!("composer.placeholder").into_owned()
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
        let terminal_contexts = self
            .app_state
            .read(cx)
            .active
            .as_ref()
            .map(|active| active.terminal_workspace.contexts.clone())
            .unwrap_or_default();
        let has_terminal_contexts = !terminal_contexts.is_empty();
        let context_chips = h_flex()
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
                        this.app_state
                            .update(cx, |state, cx| state.remove_terminal_context(id, cx));
                    }))
            }));

        let card = v_flex()
            .w_full()
            .rounded(px(16.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_sm()
            // ⌘V with image clipboard content, and arrow/Escape trigger-menu
            // navigation (fires after the input's own key actions).
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                if key == "v" && ev.keystroke.modifiers.platform {
                    this.paste_clipboard_image(window, cx);
                    return;
                }
                if !this.menu_visible(cx) {
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
            .on_drop(move |paths: &ExternalPaths, window: &mut Window, cx: &mut App| {
                let paths: Vec<PathBuf> = paths.paths().to_vec();
                composer.update(cx, |this, cx| {
                    for path in paths {
                        if mime_from_path(&path).starts_with("image/") {
                            this.add_image_path(path, window, cx);
                        }
                    }
                });
            })
            .when_some(plan_ready_title, |this, title| {
                this.child(self.render_plan_ready_header(title, cx))
            })
            .when(has_terminal_contexts, |this| {
                this.child(div().px_3().pt_2().child(context_chips))
            })
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .child(Input::new(&self.input).appearance(false)),
            )
            .children(self.render_image_strip(cx))
            .child(control_row);

        v_flex()
            .relative()
            .flex_shrink_0()
            .px_4()
            .pt_2()
            .pb_3()
            .gap_2()
            // Shift+Tab toggles Build ↔ Plan (S1 §4).
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "tab" && ev.keystroke.modifiers.shift {
                    this.app_state
                        .update(cx, |state, cx| state.toggle_interaction_mode(cx));
                    cx.notify();
                }
            }))
            .when_some(approval, |this, request| {
                this.child(self.render_approval_panel(&request, approval_count, cx))
            })
            .when_some(user_input, |this, (request_id, questions)| {
                this.child(self.render_user_input_panel(request_id, questions, cx))
            })
            .children(self.render_trigger_menu(cx))
            .child(card)
            .children(self.render_checkout_row(cx))
            .children(self.render_image_preview(cx))
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
    loading: bool,
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
            .child(
                icon.small()
                    .text_color(if active { cx.theme().foreground } else { muted }),
            )
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
        list = list.child(render_model_row(
            row, index, selected, app_entity, popover, cx,
        ));
    }
    if rows.is_empty() {
        list = list.child(
            div()
                .px_3()
                .py_4()
                .text_size(px(13.))
                .text_color(muted)
                .child(if loading {
                    rust_i18n::t!("composer.loading_models")
                } else {
                    rust_i18n::t!("composer.no_models")
                }),
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
                .child(rust_i18n::t!("composer.restart_note")),
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
                    let id = key_rows[n - 1].id.clone();
                    app_key.update(cx, |s, cx| s.set_active_model(Some(id), cx));
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
    let is_current = selected.as_deref() == Some(row.id.as_str());
    let is_fav = app_entity.read(cx).is_favorite_model(&row.id);
    let name = row.name.clone();
    let id = row.id.clone();
    let fav_id = row.id.clone();

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
            app_select.update(cx, |s, cx| s.set_active_model(Some(id.clone()), cx));
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
                    Icon::new(if is_fav {
                        IconName::StarFill
                    } else {
                        IconName::Star
                    })
                    .xsmall()
                    .text_color(if is_fav {
                        rgb(CLAUDE_TINT).into()
                    } else {
                        muted
                    }),
                )
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    app_fav.update(cx, |s, cx| s.toggle_favorite_model(&fav_id, cx));
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
    app_entity: &Entity<AppState>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;

    let mut list = v_flex().w_full().p_1().gap_0p5();
    for (index, (mode, label, description, icon_path)) in APPROVAL_MODES.iter().enumerate() {
        let mode = *mode;
        let is_current = mode == current;
        let app = app_entity.clone();
        let popover = popover.clone();
        list = list.child(
            h_flex()
                .id(("permission-row", index))
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .items_start()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().muted))
                .on_click(move |_, window, cx| {
                    app.update(cx, |s, cx| s.set_active_approval_mode(mode, cx));
                    popover.update(cx, |st, cx| st.dismiss(window, cx));
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
                                .child(div().font_medium().child(rust_i18n::t!(*label)))
                                .when(is_current, |this| {
                                    this.child(
                                        Icon::new(IconName::Check).xsmall().text_color(primary),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(rust_i18n::t!(*description)),
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
                .child(rust_i18n::t!("composer.restart_note")),
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
    app_entity: &Entity<AppState>,
    popover: &Entity<PopoverState>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let primary = cx.theme().primary;
    let default_suffix = rust_i18n::t!("composer.option_default").into_owned();

    let section_header = |label: &str, cx: &mut Context<PopoverState>| -> AnyElement {
        div()
            .px_2()
            .pt_2()
            .pb_1()
            .text_size(px(11.))
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(label.to_string())
            .into_any_element()
    };

    let mut pane = v_flex().w(px(280.)).p_1().gap_0p5();

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
                            .px_2()
                            .py_1p5()
                            .text_size(px(12.))
                            .text_color(muted)
                            .child(rust_i18n::t!("composer.ultrathink_locked")),
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
                    let app = app_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    let opt_value = opt.value.clone();
                    pane = pane.child(
                        h_flex()
                            .id(("trait-opt", index * 31 + descriptor_hash(id)))
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
                                app.update(cx, |s, cx| {
                                    if is_ultra {
                                        s.select_ultrathink(cx);
                                    } else {
                                        s.set_active_option(
                                            &opt_id,
                                            Some(serde_json::Value::String(opt_value.clone())),
                                            cx,
                                        );
                                    }
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
                    (true, rust_i18n::t!("composer.on").into_owned()),
                    (false, rust_i18n::t!("composer.off").into_owned()),
                ]
                .into_iter()
                .enumerate()
                {
                    let is_selected = on == value;
                    let app = app_entity.clone();
                    let pop = popover.clone();
                    let opt_id = id.clone();
                    pane = pane.child(
                        h_flex()
                            .id(("trait-bool", index * 61 + descriptor_hash(id)))
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
                                app.update(cx, |s, cx| {
                                    s.set_active_option(
                                        &opt_id,
                                        Some(serde_json::Value::Bool(value)),
                                        cx,
                                    );
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
                .px_2()
                .py_1p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_size(px(11.))
                .text_color(muted)
                .child(rust_i18n::t!("composer.restart_note")),
        );
    }
    pane.into_any_element()
}

/// A tiny stable hash of a descriptor id, to keep row element ids unique across
/// sections without colliding.
fn descriptor_hash(id: &str) -> usize {
    id.bytes().fold(0usize, |acc, b| acc.wrapping_add(b as usize))
}

/// The "⋯" overflow popover: the context chip's usage summary plus the
/// permission / mode chips, shown when the control row collapses at narrow
/// widths.
fn render_overflow_pane(
    usage: Option<TokenUsage>,
    mode: ApprovalMode,
    interaction: InteractionMode,
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

    let (mode_label, _, mode_icon) = approval_mode_meta(mode);
    let (interaction_icon, interaction_label) = match interaction {
        InteractionMode::Build => ("icons/box.svg", rust_i18n::t!("composer.build")),
        InteractionMode::Plan => ("icons/ruler.svg", rust_i18n::t!("composer.plan")),
    };
    v_flex()
        .w(px(220.))
        .p_1()
        .gap_0p5()
        .child(item(Icon::new(IconName::Info), context_label(usage)))
        .child(item(Icon::empty().path(mode_icon), mode_label.into()))
        .child(item(
            Icon::empty().path(interaction_icon),
            interaction_label.into_owned().into(),
        ))
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
                    .child(rust_i18n::t!("composer.context_window_title")),
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

    // "<Provider> automatically compacts its context when needed." Both Claude
    // and Codex compact automatically, so the line always shows for a known
    // provider (our TokenUsage carries no explicit `compactsAutomatically` flag
    // — see the reported contract gap; no `Total processed` field either).
    if let Some(provider) = provider {
        pane = pane.child(
            div()
                .pt_1()
                .text_size(px(11.))
                .text_color(muted)
                .child(rust_i18n::t!(
                    "composer.compacts_automatically",
                    provider = provider_display_name(provider)
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
                options,
                default_value,
                ..
            } => {
                // An armed Ultrathink shows in the reasoning segment (it is not
                // persisted, so it does not resolve as an ordinary selection).
                if id == "reasoningEffort" && ultrathink_armed {
                    if let Some(o) = options.iter().find(|o| o.value == "ultrathink") {
                        parts.push(o.label.clone());
                        continue;
                    }
                }
                if let Some(value) = resolved_select_value(id, options, default_value, selections) {
                    if let Some(o) = options.iter().find(|o| o.value == value) {
                        parts.push(o.label.clone());
                    }
                }
            }
            OptionDescriptor::Boolean {
                id,
                label,
                default_value,
            } => {
                let on = option_selection_bool(selections, id).unwrap_or(*default_value);
                if id == "fastMode" {
                    parts.push(
                        rust_i18n::t!(if on {
                            "composer.trait_fast"
                        } else {
                            "composer.trait_normal"
                        })
                        .into_owned(),
                    );
                } else {
                    let state = rust_i18n::t!(if on { "composer.on" } else { "composer.off" });
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
fn assemble_user_input_answers(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
    current_index: usize,
    custom_current: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (i, question) in questions.iter().enumerate() {
        if i == current_index {
            if let Some(text) = custom_current.map(str::trim).filter(|t| !t.is_empty()) {
                map.insert(
                    question.id.clone(),
                    serde_json::Value::String(text.to_string()),
                );
                continue;
            }
        }
        let selected = selections.get(&question.id).cloned().unwrap_or_default();
        let value = if question.multi_select {
            serde_json::Value::Array(
                selected.into_iter().map(serde_json::Value::String).collect(),
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
        None => rust_i18n::t!("composer.default_model").into_owned(),
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
                (None, None) => rust_i18n::t!("composer.context").into_owned(),
            }
        }
        None => rust_i18n::t!("composer.context").into_owned(),
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
    fn serializes_terminal_context_like_t3() {
        let contexts = vec![TerminalContext {
            id: 1,
            terminal_label: "zsh".into(),
            line_start: 12,
            line_end: 13,
            text: "cargo test\nok".into(),
        }];
        assert_eq!(
            append_terminal_contexts_to_prompt("Explain this", &contexts),
            "Explain this\n\n<terminal_context>\n- zsh lines 12-13:\n  12 | cargo test\n  13 | ok\n</terminal_context>"
        );
    }

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
    fn approval_mode_meta_matches_ui_copy() {
        assert_eq!(
            approval_mode_meta(ApprovalMode::Supervised),
            (
                "Supervised".to_string(),
                "Ask before commands and file changes.".to_string(),
                "icons/lock.svg"
            )
        );
        assert_eq!(
            approval_mode_meta(ApprovalMode::AutoAcceptEdits),
            (
                "Auto-accept edits".to_string(),
                "Auto-approve edits, ask before other actions.".to_string(),
                "icons/pencil.svg"
            )
        );
        assert_eq!(
            approval_mode_meta(ApprovalMode::FullAccess),
            (
                "Full access".to_string(),
                "Allow commands and edits without prompts.".to_string(),
                "icons/unlock.svg"
            )
        );
    }

    #[test]
    fn current_model_name_maps_catalog() {
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
        assert_eq!(traits_chip_label(&spec, &[], false), Some("High · 200k".into()));
        // A selection overrides the default.
        let sel = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::Value::String("1m".into()),
        }];
        assert_eq!(traits_chip_label(&spec, &sel, false), Some("High · 1M".into()));

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
        let answers = assemble_user_input_answers(&questions, &std::collections::HashMap::new(), 0, None);
        assert_eq!(answers["q1"], serde_json::json!(""));
        assert_eq!(answers["q2"], serde_json::json!([]));
    }
}
