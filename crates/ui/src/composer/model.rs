use std::collections::HashMap;

use agent::{
    ApprovalMode, FileChangeKind, InteractionMode, ModelSpec, OptionDescriptor, ProviderCommand,
    ProviderCommandKind, TokenUsage, UserInputQuestion,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone as _};
use tcode_core::ui::ConversationDestination;

use crate::context_meter;
use crate::palette::fuzzy_score;

/// The three approval modes in display order, each with its label, one-line
/// description (exact UI copy), and chip icon (lock → pencil → unlock).
pub(super) const APPROVAL_MODES: [(ApprovalMode, &str, &str, &str); 3] = [
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

pub(super) fn approval_mode_meta(mode: ApprovalMode) -> (String, &'static str) {
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

/// The minimal `/`-command set this slice handles (S1 §7).
pub(super) enum SlashIntent {
    Plan,
    Default,
    Model,
}

/// Strip a leading `/orchestrate` command token while rejecting lookalikes
/// such as `/orchestrated`.
pub(super) fn strip_orchestrate_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/orchestrate")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// User-facing validation failures for a syntactically recognized `/later`.
pub(super) enum LaterError {
    MissingTime,
    MissingMessage,
    InvalidTime,
}

/// Parse a scheduled-message command against an injectable local clock. A
/// non-command returns `None`, keeping provider commands and lookalikes on the
/// ordinary submit path; recognized but malformed commands return an error.
pub(super) fn parse_later(
    text: &str,
    now: DateTime<Local>,
) -> Option<Result<(u64, String), LaterError>> {
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

pub(super) fn parse_wall_clock(spec: &str) -> Option<(u32, u32)> {
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

pub(super) fn local_occurrence(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .earliest()
}

pub(super) fn parse_relative(spec: &str) -> Option<chrono::Duration> {
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
pub(crate) fn format_countdown(secs: u64) -> String {
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
pub(super) fn slash_command(text: &str) -> Option<SlashIntent> {
    match text.trim() {
        "/plan" => Some(SlashIntent::Plan),
        "/default" => Some(SlashIntent::Default),
        "/model" => Some(SlashIntent::Model),
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

#[derive(Default)]
/// In-memory text drafts plus the destination currently represented by the
/// shared [`crate::widgets::input::TextareaState`]. Keeping switching in this
/// small state machine makes it explicit that the outgoing value is saved before
/// the incoming one is restored.
pub(super) struct ComposerTextCache {
    current: Option<ComposerDestination>,
    drafts: HashMap<ComposerDestination, String>,
}

impl ComposerTextCache {
    /// Change destinations, returning the text that should replace the shared
    /// input. `None` means the input already represents this destination.
    pub(super) fn switch_to(
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

    pub(super) fn clear_current(&mut self) {
        if let Some(current) = self.current.as_ref() {
            self.drafts.remove(current);
        }
    }
}

#[derive(Clone, Copy)]
/// Which glyph a trigger-menu row shows.
pub(super) enum MenuIcon {
    File,
    Folder,
    Command,
    /// Skill rows (`$`), populated from the provider's skills feed.
    Skill,
}

#[derive(Clone)]
/// What accepting a trigger-menu row does.
pub(super) enum MenuAccept {
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

#[derive(Clone)]
/// One selectable row in a trigger (`@`/`/`/`$`) menu.
pub(super) struct MenuRow {
    /// Bold primary text (basename / command label).
    pub(super) primary: String,
    /// Muted secondary text (parent path / description).
    pub(super) secondary: String,
    pub(super) icon: MenuIcon,
    pub(super) accept: MenuAccept,
    /// The group this row belongs to (T3 §5: the `/` menu is grouped
    /// `Built-in` / `Provider`; the `$` menu is a single `Skills` group). A
    /// header is rendered above the first row of each group. `None` = ungrouped
    /// (the `@` file menu).
    pub(super) group: Option<&'static str>,
}

/// Select and rank one provider-command kind with the same fuzzy subsequence
/// matcher as the command palette. Empty queries preserve provider order;
/// non-empty queries put the strongest matches first and never truncate.
pub(super) fn filter_provider_commands<'a>(
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

pub(super) fn option_selection_str<'a>(
    selections: &'a [agent::OptionSelection],
    id: &str,
) -> Option<&'a str> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_str())
}

pub(super) fn option_selection_bool(
    selections: &[agent::OptionSelection],
    id: &str,
) -> Option<bool> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_bool())
}

/// The resolved value of a select descriptor: an accepted persisted selection,
/// else the descriptor default.
pub(super) fn resolved_select_value(
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
pub(super) fn traits_chip_label(
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
                if id == "contextWindow" {
                    parts.push(agent::claude::format_context_window(
                        agent::claude::resolved_context_window(&spec.id, selections),
                    ));
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
pub(super) fn user_input_answered(
    question: &UserInputQuestion,
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    selections
        .get(&question.id)
        .is_some_and(|selected| !selected.is_empty())
}

pub(super) fn user_input_all_answered(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    questions
        .iter()
        .all(|question| user_input_answered(question, selections))
}

/// The next unanswered question after `from`, wrapping around to earlier ones
/// (and back to `from` itself) so a skipped question is always revisited.
pub(super) fn next_unanswered_question(
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

pub(super) fn assemble_user_input_answers(
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

pub(super) fn current_model_name(catalog: &[ModelSpec], model: Option<&str>) -> String {
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
pub(super) fn current_model_name_resolved(
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
pub(super) fn context_label(usage: Option<TokenUsage>) -> String {
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

pub(super) fn file_change_kind_label(kind: FileChangeKind) -> &'static str {
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
    use crate::composer::components::images::transcode_image_to_png;
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
        // Context windows resolve from the model's native default, not the
        // descriptor's stale fallback.
        assert_eq!(
            traits_chip_label(&spec, &[], false),
            Some("High · 1M".into())
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
        let custom = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::json!(500_000),
        }];
        assert_eq!(
            traits_chip_label(&spec, &custom, false),
            Some("High · 500k".into())
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
