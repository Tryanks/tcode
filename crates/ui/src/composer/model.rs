use agent::{
    FileChangeKind, ModelSpec, OptionDescriptor, ProviderCommand, ProviderCommandKind, TokenUsage,
    UserInputQuestion,
};
use chrono::{DateTime, Local, LocalResult, NaiveTime, TimeZone as _};
use tcode_core::ui::ConversationDestination;

use crate::context_meter;
use crate::palette::fuzzy_score;

pub(super) enum SlashIntent {
    Model,
    Compact,
    Goal(GoalIntent),
}

/// What a `/goal` the composer runs itself asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalIntent {
    Set(String),
    Clear,
    Pause,
    Resume,
    /// Bare `/goal` or `/goal edit`: put the current objective up for editing.
    Edit,
}

/// The harness's goal control when `/goal` is Tcode's to run. A harness whose
/// goal change is a turn of its own takes it only between turns; otherwise the
/// text reaches the provider as typed.
pub(super) fn goal_offered(state: &crate::store::ComposerState) -> Option<agent::GoalControl> {
    let control = state.goal_control?;
    (!state.conversation_read_only && (control.live || !state.turn_running && !state.stopping))
        .then_some(control)
}

/// `pause` and `resume` are subcommands only where the goal can be paused;
/// elsewhere they are an objective like any other, as the harness reads them.
pub(super) fn parse_goal(text: &str, control: agent::GoalControl) -> Option<GoalIntent> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(match rest.trim() {
        "" | "edit" => GoalIntent::Edit,
        "clear" => GoalIntent::Clear,
        "pause" if control.pause => GoalIntent::Pause,
        "resume" if control.pause => GoalIntent::Resume,
        objective => GoalIntent::Set(objective.to_owned()),
    })
}

/// Whether `/compact` is Tcode's to run: the provider compacts on request and
/// no turn is running. Otherwise the text reaches the provider as typed.
pub(super) fn compact_offered(state: &crate::store::ComposerState) -> bool {
    !state.conversation_read_only
        && !state.turn_running
        && !state.stopping
        && state.manual_compaction
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// User-facing validation failures for a syntactically recognized `/later`.
pub(super) enum LaterError {
    MissingTime,
    MissingMessage,
    InvalidTime,
    /// The next time the local clock would read this hour and minute falls in
    /// the hour a daylight-saving change skips.
    SkippedTime {
        hour: u32,
        minute: u32,
    },
}

/// Parse a scheduled-message command into the seconds from `now` until it is
/// due and the message. A non-command returns `None`, keeping provider
/// commands and lookalikes on the ordinary submit path; recognized but
/// malformed commands return an error.
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

    let delay = if let Some((hour, minute)) = parse_wall_clock(time_spec) {
        next_wall_clock(now, hour, minute).map(|at| at - now)
    } else {
        parse_relative(time_spec)
            .filter(|duration| now.checked_add_signed(*duration).is_some())
            .ok_or(LaterError::InvalidTime)
    };
    Some(delay.and_then(|delay| {
        u64::try_from(delay.num_seconds())
            .map(|secs| (secs, message.to_string()))
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

/// The next moment after `now` the local clock reads `hour:minute`: today's
/// if it is still ahead, else tomorrow's, and the first of the two a clock set
/// back reads. One the clocks skip is not moved to another time.
fn next_wall_clock(
    now: DateTime<Local>,
    hour: u32,
    minute: u32,
) -> Result<DateTime<Local>, LaterError> {
    let time = NaiveTime::from_hms_opt(hour, minute, 0).ok_or(LaterError::InvalidTime)?;
    let today = now.date_naive();
    for date in [Some(today), today.succ_opt()].into_iter().flatten() {
        let wall_clock = date.and_time(time);
        match Local.from_local_datetime(&wall_clock) {
            LocalResult::Single(at) if at > now => return Ok(at),
            // chrono 0.4.45's system time zone lists the smaller offset first,
            // the later instant, though `Ambiguous` documents (earliest, latest).
            LocalResult::Ambiguous(first, second) => {
                if let Some(at) = [first, second].into_iter().filter(|at| *at > now).min() {
                    return Ok(at);
                }
            }
            LocalResult::None if wall_clock > now.naive_local() => {
                return Err(LaterError::SkippedTime { hour, minute });
            }
            _ => {}
        }
    }
    Err(LaterError::InvalidTime)
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
        .and_then(chrono::Duration::try_seconds)
}

pub(super) fn slash_command(
    text: &str,
    compact_offered: bool,
    goal_offered: Option<agent::GoalControl>,
) -> Option<SlashIntent> {
    match text.trim() {
        "/model" => Some(SlashIntent::Model),
        "/compact" if compact_offered => Some(SlashIntent::Compact),
        text => goal_offered
            .and_then(|control| parse_goal(text, control))
            .map(SlashIntent::Goal),
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

#[derive(Clone, Copy, PartialEq)]
/// Which glyph a trigger-menu row shows.
pub(super) enum MenuIcon {
    File,
    Folder,
    Command,
    /// Skill rows (`$`), populated from the provider's skills feed.
    Skill,
    Thread,
}

#[derive(Clone, PartialEq)]
/// What accepting a trigger-menu row does.
pub(super) enum MenuAccept {
    /// Insert the serialized `[basename](path)` mention for this relative path.
    InsertPath(String),
    /// Reference this thread from the active one: a chip, not text.
    ReferenceThread(String),
    /// Insert `$<name> ` for this provider skill.
    InsertSkill(String),
    /// Insert `/<name> ` for this provider slash command.
    InsertCommand(String),
    /// Insert the scheduled-message command prefix.
    InsertLater,
    /// Insert the goal command prefix.
    InsertGoal,
    /// Strip the `/model` command and open the model picker.
    OpenModelPicker,
}

#[derive(Clone, PartialEq)]
/// One selectable row in a trigger (`@`/`/`/`$`) menu.
pub(super) struct MenuRow {
    /// Bold primary text (basename / command label).
    pub(super) primary: String,
    /// Muted secondary text (parent path / description).
    pub(super) secondary: String,
    pub(super) icon: MenuIcon,
    pub(super) accept: MenuAccept,
    /// Group heading emitted before this row when its group changes.
    /// `None` leaves the file-mention menu ungrouped.
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

/// The options as every control of the composer shows them. Each runs at the
/// value the host gives, which while a harness runs is the one it reported:
/// a menu checks that. The thread's choice where it is not that value is
/// pending: a chip, and a control that holds one position, show it with the
/// mark. An option the harness does not report runs at what was sent, and says
/// so; until one it reports has, nothing is known of it.
#[derive(Clone, Default)]
pub(crate) struct OptionStates {
    running: Vec<agent::OptionSelection>,
    requested: Vec<agent::OptionSelection>,
    unconfirmed: Vec<String>,
    harness: &'static str,
}

impl OptionStates {
    pub(crate) fn of(state: &crate::store::ComposerState) -> Self {
        Self {
            running: state.active_option_selections.clone(),
            requested: state.requested_option_selections.clone(),
            unconfirmed: state.unconfirmed_options.clone(),
            harness: state
                .provider
                .map_or("", |provider| provider.display_name()),
        }
    }

    fn find<'a>(
        selections: &'a [agent::OptionSelection],
        id: &str,
    ) -> Option<&'a serde_json::Value> {
        selections
            .iter()
            .find(|selection| selection.id == id)
            .map(|selection| &selection.value)
    }

    /// The value `id` runs at, `None` while nothing is known of it.
    pub(crate) fn running(&self, id: &str) -> Option<&serde_json::Value> {
        Self::find(&self.running, id)
    }

    /// The thread's choice for `id` that it does not run yet.
    pub(crate) fn pending(&self, id: &str) -> Option<&serde_json::Value> {
        Self::find(&self.requested, id).filter(|value| Some(*value) != self.running(id))
    }

    /// `id`'s running value is only what Tcode sent: the harness reports none.
    pub(crate) fn unconfirmed(&self, id: &str) -> bool {
        self.unconfirmed.iter().any(|option| option == id)
    }

    /// What a chip or a one-position control shows of `id`.
    pub(crate) fn shown(&self, id: &str) -> Option<&serde_json::Value> {
        self.pending(id).or_else(|| self.running(id))
    }

    pub(crate) fn shown_str(&self, id: &str) -> Option<&str> {
        self.shown(id).and_then(serde_json::Value::as_str)
    }

    /// Each of `descriptors` at what [`Self::shown`] gives it.
    pub(crate) fn shown_selections(
        &self,
        descriptors: &[OptionDescriptor],
    ) -> Vec<agent::OptionSelection> {
        descriptors
            .iter()
            .filter_map(|descriptor| {
                let (OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. }) =
                    descriptor;
                Some(agent::OptionSelection {
                    id: id.clone(),
                    value: self.shown(id)?.clone(),
                })
            })
            .collect()
    }

    /// Whether any of `descriptors` has a choice pending.
    pub(crate) fn any_pending(&self, descriptors: &[OptionDescriptor]) -> bool {
        descriptors.iter().any(|descriptor| {
            let (OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. }) =
                descriptor;
            self.pending(id).is_some()
        })
    }

    /// The note a menu section carries for `id`, when the harness does not
    /// report it: the value checked is what was sent, or nothing was.
    pub(crate) fn unconfirmed_note(&self, id: &str) -> Option<std::borrow::Cow<'static, str>> {
        self.unconfirmed(id).then(|| match self.running(id) {
            Some(_) => crate::tr!("composer.option_sent", harness = self.harness),
            None => crate::tr!("composer.option_unknown", harness = self.harness),
        })
    }
}

/// How a select value is named: its option's label, or the value itself when
/// no option carries it.
pub(super) fn select_value_label(options: &[agent::SelectOption], value: &str) -> String {
    options
        .iter()
        .find(|option| option.value == value)
        .map_or_else(|| value.to_owned(), |option| option.label.clone())
}

/// The traits chip label: every descriptor's [`OptionStates::shown`] value
/// joined with " · " (e.g. "High", "High · Fast", "Thinking Off"), its own
/// label where nothing is known of it. `None` when the model has no
/// descriptors.
pub(super) fn traits_chip_label(spec: &ModelSpec, states: &OptionStates) -> Option<String> {
    if spec.options.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for descriptor in &spec.options {
        match descriptor {
            OptionDescriptor::Select {
                id, label, options, ..
            } => {
                let part = states
                    .shown_str(id)
                    .map(|value| select_value_label(options, value))
                    .unwrap_or_else(|| label.clone());
                parts.push(part);
            }
            OptionDescriptor::Boolean {
                id, label, role, ..
            } => {
                let Some(on) = states.shown(id).and_then(serde_json::Value::as_bool) else {
                    parts.push(label.clone());
                    continue;
                };
                if *role == agent::OptionRole::Speed {
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

/// Build the answers map for a user-input request: keyed by question id, with a
/// string (single-select / free-text) or string-array (multi-select) value.
pub(super) fn assemble_user_input_answers(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for question in questions {
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
    resolved: &[tcode_core::provider_models::ResolvedModel],
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
            let used = u.used_tokens;
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

pub(super) fn file_change_kind_label(kind: FileChangeKind) -> std::borrow::Cow<'static, str> {
    match kind {
        FileChangeKind::Create => crate::tr!("diff.created"),
        FileChangeKind::Modify => crate::tr!("diff.modified"),
        FileChangeKind::Delete => crate::tr!("diff.deleted"),
        FileChangeKind::Rename => crate::tr!("diff.renamed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike as _;

    fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn parsed_later(text: &str, now: DateTime<Local>) -> (DateTime<Local>, String) {
        let (delay, message) = parse_later(text, now).unwrap().unwrap();
        (now + chrono::Duration::seconds(delay as i64), message)
    }

    #[test]
    fn later_parses_schedules_and_rejects_invalid_or_overflowing_input() {
        let now = local_time(2026, 8, 8, 12, 0);
        let (later_today, message) = parsed_later("/later 23:59 continue here", now);
        assert_eq!(later_today.date_naive(), now.date_naive());
        assert_eq!((later_today.hour(), later_today.minute()), (23, 59));
        assert_eq!(message, "continue here");

        let (tomorrow, message) = parsed_later("/later 5:10 first line\nsecond line", now);
        assert_eq!(tomorrow.date_naive(), now.date_naive().succ_opt().unwrap());
        assert_eq!((tomorrow.hour(), tomorrow.minute()), (5, 10));
        assert_eq!(message, "first line\nsecond line");
        let (same_time_tomorrow, _) = parsed_later("/later 12:00 next day", now);
        assert_eq!(
            same_time_tomorrow.date_naive(),
            now.date_naive().succ_opt().unwrap()
        );

        for (command, seconds, message) in [
            ("/later 5min multi word", 300, "multi word"),
            ("/later 30s soon", 30, "soon"),
            ("/later 2h much later", 7_200, "much later"),
        ] {
            let (fire_at, parsed_message) = parsed_later(command, now);
            assert_eq!((fire_at - now).num_seconds(), seconds);
            assert_eq!(parsed_message, message);
        }
        for (command, error) in [
            ("/later", LaterError::MissingTime),
            ("/later 5min", LaterError::MissingMessage),
            ("/later 25:00 x", LaterError::InvalidTime),
            ("/later 5:99 x", LaterError::InvalidTime),
            ("/later abc x", LaterError::InvalidTime),
            ("/later 0s x", LaterError::InvalidTime),
            ("/later -1h x", LaterError::InvalidTime),
            ("/later 9223372036854775s x", LaterError::InvalidTime),
            ("/later 9223372036854775807s x", LaterError::InvalidTime),
            ("/later 9223372036854775807h x", LaterError::InvalidTime),
            ("/later 999999999999999999999min x", LaterError::InvalidTime),
        ] {
            assert_eq!(parse_later(command, now), Some(Err(error)), "{command}");
        }
        assert_eq!(parse_later("/laters 5min x", now), None);
    }

    /// Codex's `/goal` takes `pause` and `resume`; Claude Code's reads any
    /// word as the condition, so there they are sent on as one.
    #[test]
    fn goal_subcommands_follow_what_the_harness_can_do() {
        let [claude, codex] = [agent::ProviderKind::ClaudeCode, agent::ProviderKind::Codex]
            .map(|provider| provider.caps().goals.unwrap());
        let goal = |text, control| parse_goal(text, control);
        assert_eq!(
            goal("/goal  ship the strip ", codex),
            Some(GoalIntent::Set("ship the strip".into()))
        );
        assert_eq!(goal("/goal clear", claude), Some(GoalIntent::Clear));
        assert_eq!(goal("/goal", claude), Some(GoalIntent::Edit));
        assert_eq!(goal("/goal pause", codex), Some(GoalIntent::Pause));
        assert_eq!(goal("/goal resume", codex), Some(GoalIntent::Resume));
        assert_eq!(
            goal("/goal pause", claude),
            Some(GoalIntent::Set("pause".into()))
        );
        assert_eq!(goal("/goals", codex), None);
        assert_eq!(goal("ship /goal", codex), None);
    }

    #[test]
    fn user_input_answers_assemble_one_string_or_a_list_per_question() {
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

        let answers = assemble_user_input_answers(&questions, &selections);
        assert_eq!(answers["q1"], serde_json::json!("A"));
        assert_eq!(answers["q2"], serde_json::json!(["X", "Y"]));

        let answers = assemble_user_input_answers(&questions, &std::collections::HashMap::new());
        assert_eq!(answers["q1"], serde_json::json!(""));
        assert_eq!(answers["q2"], serde_json::json!([]));
    }
}
