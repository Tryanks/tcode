use super::*;

pub(super) fn map_item(item: &Value) -> Option<ThreadItem> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let provider_kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // The app synthesizes a canonical `UserMessage` event at send time (needed
    // for universal replay across providers). Codex echoes the same user input
    // back as a `userMessage` item; emitting it too would render a duplicate
    // user bubble, so swallow the provider echo here.
    if provider_kind == "userMessage" {
        log::debug!("suppressing provider-echoed userMessage item {id}");
        return None;
    }
    let content = match provider_kind {
        // `request_user_input_async` output: the text only lists the questions
        // the input panel shows, so the transcript records the tool call.
        "agentMessage" if has_async_questions(item) => ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: ASYNC_QUESTION_TOOL.into(),
            input: json!({ "questions": item.get("questions").cloned().unwrap_or_default() }),
            output: None,
            status: ItemStatus::Completed,
            family: question_family(ASYNC_QUESTION_TOOL, item, None).map(Box::new),
        },
        "plan" | "proposed_plan" => ItemContent::Plan {
            markdown: string_field(item, "text"),
        },
        "agentMessage" => ItemContent::AssistantMessage {
            text: string_field(item, "text"),
            summarized: false,
        },
        "reasoning" => {
            let mut parts = strings(item.get("summary"));
            parts.extend(strings(item.get("content")));
            ItemContent::Reasoning {
                text: parts.join("\n"),
            }
        }
        "commandExecution" => ItemContent::CommandExecution {
            command: shell_script(&string_field(item, "command")),
            input: Vec::new(),
            output: string_field(item, "aggregatedOutput"),
            exit_code: item
                .get("exitCode")
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "fileChange" => ItemContent::FileChange {
            changes: item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.iter().filter_map(map_file_change).collect())
                .unwrap_or_default(),
            status: map_status(item.get("status").and_then(Value::as_str)),
            // `FileChangeThreadItem` (v2, 0.167.1) says nothing of the patch's
            // result beyond its status.
            output: None,
            command: None,
        },
        "mcpToolCall" | "dynamicToolCall" => {
            let result = item
                .get(if provider_kind == "mcpToolCall" {
                    "result"
                } else {
                    "contentItems"
                })
                .filter(|value| !value.is_null());
            let ToolOutput { text, images } =
                result.map(ToolOutput::from_content).unwrap_or_default();
            ItemContent::ToolCall {
                image_reads: images,
                name: if provider_kind == "mcpToolCall" {
                    format!(
                        "{}/{}",
                        string_field(item, "server"),
                        string_field(item, "tool")
                    )
                } else {
                    string_field(item, "tool")
                },
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
                output: item
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(result.map(|_| text)),
                status: map_status(item.get("status").and_then(Value::as_str)),
                family: None,
            }
        }
        "collabAgentToolCall" => ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: string_field(item, "tool"),
            input: collab_tool_input(item),
            output: item
                .get("agentsStates")
                .and_then(Value::as_object)
                .filter(|states| !states.is_empty())
                .map(|_| item["agentsStates"].to_string()),
            status: map_status(item.get("status").and_then(Value::as_str)),
            family: None,
        },
        "imageView" => ItemContent::ImageRead {
            path: string_field(item, "path"),
            image: None,
        },
        // `WebSearchThreadItem` (v2, 0.167.1) has no status: it runs from
        // item/started to item/completed, as ChatGPT.app's web-search row
        // streams until its item completes. Its `results` are opaque JSON.
        "webSearch" => {
            let query = string_field(item, "query");
            let mut input = serde_json::Map::new();
            input.insert("query".into(), query.clone().into());
            if let Some(action) = item.get("action").filter(|action| !action.is_null()) {
                input.insert("action".into(), action.clone());
            }
            ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: provider_kind.into(),
                input: Value::Object(input),
                output: item
                    .get("results")
                    .filter(|results| {
                        results
                            .as_array()
                            .is_some_and(|results| !results.is_empty())
                    })
                    .map(Value::to_string),
                status: ItemStatus::InProgress,
                family: Some(Box::new(ToolFamily::WebSearch { query, links: None })),
            }
        }
        // The display item of the `clock.sleep` call, which a rollout records
        // as that call with its arguments.
        "sleep" => ItemContent::ToolCall {
            image_reads: Vec::new(),
            name: "sleep".into(),
            input: json!({ "duration_ms": item.get("durationMs").cloned().unwrap_or(Value::Null) }),
            output: None,
            status: map_status(item.get("status").and_then(Value::as_str)),
            family: item
                .get("durationMs")
                .and_then(Value::as_u64)
                .map(|duration_ms| Box::new(ToolFamily::Wait { duration_ms })),
        },
        "imageGeneration" => image_generation(item),
        _ => ItemContent::Other {
            provider_kind: provider_kind.into(),
            summary: serde_json::to_string(item).unwrap_or_else(|_| provider_kind.into()),
        },
    };
    Some(ThreadItem {
        id,
        parent_item_id: None,
        content,
    })
}

/// [`map_item`] for an `item/*` notification. A sleep carries no status of
/// its own, so it has finished when it completes.
pub(super) fn lifecycle_item(method: &str, value: &Value) -> Option<ThreadItem> {
    let mut item = map_item(value)?;
    if method == "item/completed"
        && value.get("status").is_none()
        && let ItemContent::ToolCall { status, .. } = &mut item.content
    {
        *status = ItemStatus::Completed;
    }
    Some(item)
}

/// An `imageGeneration` item as the call of the hosted `image_generation`
/// tool: the prompt the picture was generated from, the local images it was
/// given, and the picture, kept where Codex saved it. A rollout records the
/// same call with the same fields (`image_generation_call`), and the
/// importer reads it through this.
pub fn image_generation(item: &Value) -> ItemContent {
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let picture = text("result")
        .and_then(|data| crate::tool_output::image(Some("image/png"), data))
        .map(|picture| Attachment {
            source_path: text("savedPath").map(str::to_owned),
            ..picture
        });
    let mut input = serde_json::Map::new();
    if let Some(prompt) = text("revisedPrompt") {
        input.insert("revised_prompt".into(), prompt.into());
    }
    let inputs: Vec<Value> = item
        .get("inputImages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|image| image.get("path").cloned())
        .collect();
    if !inputs.is_empty() {
        input.insert("input_images".into(), Value::Array(inputs));
    }
    let failure = item.get("failure").filter(|failure| !failure.is_null());
    let status = match map_status(item.get("status").and_then(Value::as_str)) {
        _ if failure.is_some() => ItemStatus::Failed,
        // A rollout keeps `generating` on a generation whose picture arrived.
        ItemStatus::InProgress if picture.is_some() => ItemStatus::Completed,
        status => status,
    };
    ItemContent::ToolCall {
        image_reads: picture.into_iter().collect(),
        name: "image_generation".into(),
        input: Value::Object(input),
        output: failure.map(Value::to_string),
        status,
        family: Some(Box::new(ToolFamily::ImageGeneration {
            prompt: text("revisedPrompt").map(str::to_owned),
        })),
    }
}

/// The questions a call of Codex's question tool `name` asked, from its
/// input, and the answers each got, from its output. `request_user_input`
/// asks `{id, question, options: [{label, description}]}` and is answered
/// `{"answers": {<id>: {"answers": [...]}}}`
/// (`request_user_input_spec.rs`, `protocol/src/request_user_input.rs` at
/// `rust-v0.167.0`); `request_user_input_async` asks `{title, options?}`
/// with option labels alone (`AsyncUserInputQuestion`,
/// `protocol/src/items.rs`) and is answered in a later message, not here.
/// The importer reads a rollout's calls through this.
pub fn question_family(name: &str, input: &Value, output: Option<&str>) -> Option<ToolFamily> {
    let asked = input.get("questions")?.as_array()?;
    let answered: Option<Value> = output.and_then(|output| serde_json::from_str(output).ok());
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let questions = match name {
        "request_user_input" => asked
            .iter()
            .filter_map(|question| {
                let answers = question
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| answered.as_ref()?.get("answers")?.get(id)?.get("answers"))
                    .and_then(Value::as_array);
                Some(AskedQuestion {
                    question: text(question, "question")?,
                    options: question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| {
                            Some(UserInputOption {
                                label: text(option, "label")?,
                                description: text(option, "description").unwrap_or_default(),
                            })
                        })
                        .collect(),
                    answers: answers
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                })
            })
            .collect(),
        ASYNC_QUESTION_TOOL => asked
            .iter()
            .filter_map(|question| {
                Some(AskedQuestion {
                    question: text(question, "title")?,
                    options: question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|label| UserInputOption {
                            label: label.to_owned(),
                            description: String::new(),
                        })
                        .collect(),
                    answers: Vec::new(),
                })
            })
            .collect(),
        _ => return None,
    };
    Some(ToolFamily::Question { questions })
}

/// The lines `stdin` finishes, written after what `pending` holds of an
/// unfinished one, which it then holds of the next, as ChatGPT.app lists
/// what was typed into a command (`applyTerminalInteraction`, 26.1103): a
/// return or newline ends a line, blank lines are dropped, ^C drops the
/// line so far, and a backspace or delete takes back its last character.
pub fn typed_lines(pending: &mut String, stdin: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for character in stdin.chars() {
        match character {
            '\r' | '\n' => {
                let line = pending.trim();
                if !line.is_empty() {
                    lines.push(line.to_owned());
                }
                pending.clear();
            }
            '\u{3}' => pending.clear(),
            '\u{8}' | '\u{7f}' => {
                pending.pop();
            }
            other => pending.push(other),
        }
    }
    lines
}

/// A command as a rollout records Codex's own `CommandExecution` turn item
/// (`CommandExecutionItem`, `protocol/src/items.rs` at `rust-v0.167.0`: the
/// argv as `command`, `aggregated_output`, `exit_code`, a snake_case
/// `status`), shown as the live `commandExecution` item of the same command
/// is. The importer reads it through this.
pub fn rollout_command(item: &Value) -> ItemContent {
    let argv: Vec<&str> = item
        .get("command")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    ItemContent::CommandExecution {
        command: shell_script(&shell_words::join(argv)),
        input: Vec::new(),
        output: string_field(item, "aggregated_output"),
        exit_code: item
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
        status: map_status(item.get("status").and_then(Value::as_str)),
    }
}

/// The files a rollout records a patch changing: the `changes` of Codex's
/// `FileChange` turn item or of its `patch_apply_end` event, each path's
/// `FileChange` (`protocol/src/protocol.rs` at `rust-v0.167.0`: `add` and
/// `delete` with the file's `content`, `update` with a `unified_diff` and a
/// `move_path`). They are converted as the app-server converts them for the
/// live `fileChange` item (`convert_patch_changes`,
/// `app-server-protocol/src/protocol/item_builders.rs`), and then read as
/// that item's changes are.
pub fn rollout_file_changes(changes: &Value) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = changes
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(path, change)| {
            let text = |key: &str| change.get(key).and_then(Value::as_str).unwrap_or_default();
            let move_path = change.get("move_path").and_then(Value::as_str);
            let diff = match (change.get("type").and_then(Value::as_str), move_path) {
                (Some("update"), Some(to)) => format!("{}\n\nMoved to: {to}", text("unified_diff")),
                (Some("update"), None) => text("unified_diff").to_owned(),
                _ => text("content").to_owned(),
            };
            map_file_change(&json!({
                "path": path,
                "kind": {"type": change.get("type"), "move_path": move_path},
                "diff": diff,
            }))
        })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// What a rollout records apply_patch printing for a patch, its own
/// `FileChange` item's or a legacy `patch_apply_end`'s `stdout` and then
/// `stderr` (`protocol/src/protocol.rs` at `rust-v0.167.0`).
pub fn rollout_patch_output(record: &Value) -> Option<String> {
    let printed: Vec<&str> = ["stdout", "stderr"]
        .into_iter()
        .filter_map(|stream| record.get(stream).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect();
    (!printed.is_empty()).then(|| printed.join("\n"))
}

/// A rollout's snake_case status of a turn item, as its live item's.
pub fn rollout_status(status: Option<&str>) -> ItemStatus {
    map_status(status)
}

pub(super) fn collab_tool_input(item: &Value) -> Value {
    let mut input = serde_json::Map::new();
    for key in [
        "senderThreadId",
        "receiverThreadIds",
        "prompt",
        "model",
        "reasoningEffort",
    ] {
        if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
            input.insert(key.to_owned(), value.clone());
        }
    }
    let summary = item
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            item.get("receiverThreadIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|ids| !ids.is_empty())
        });
    if let Some(summary) = summary {
        input.insert("summary".into(), Value::String(summary));
    }
    Value::Object(input)
}

pub(super) fn find_subagent_activity(value: &Value) -> Option<&Value> {
    value
        .get("item")
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("subAgentActivity"))
}

pub(super) fn item_status(content: &ItemContent) -> ItemStatus {
    match content {
        ItemContent::CommandExecution { status, .. }
        | ItemContent::FileChange { status, .. }
        | ItemContent::ToolCall { status, .. }
        | ItemContent::Subagent { status, .. } => *status,
        _ => ItemStatus::Completed,
    }
}

pub(super) fn item_summary(content: &ItemContent) -> Option<String> {
    match content {
        ItemContent::ToolCall { output, .. } => output.clone(),
        _ => None,
    }
}

/// Codex runs every command through the user's login shell and reports the
/// whole argv, shell-quoted: `/bin/zsh -lc 'sed -n 1,2p hosts'`. Its own TUI
/// shows the script alone, so that is what Tcode surfaces; anything else is
/// left as reported.
pub(super) fn shell_script(command: &str) -> String {
    let Ok(argv) = shell_words::split(command) else {
        return command.to_owned();
    };
    match argv.as_slice() {
        [shell, flag, script]
            if flag == "-lc"
                && matches!(
                    std::path::Path::new(shell)
                        .file_name()
                        .and_then(|name| name.to_str()),
                    Some("bash" | "zsh" | "sh")
                ) =>
        {
            script.clone()
        }
        _ => command.to_owned(),
    }
}

pub(super) fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn notification_turn_id(params: &Value, active_turn: &Option<String>) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| active_turn.clone())
}

/// A `ThreadGoal`; `null` (no goal) maps to `None`.
pub(super) fn map_goal(goal: &Value) -> Option<Goal> {
    let status = match goal.get("status")?.as_str()? {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "blocked" => GoalStatus::Blocked,
        "usageLimited" => GoalStatus::UsageLimited,
        "budgetLimited" => GoalStatus::BudgetLimited,
        "complete" => GoalStatus::Complete,
        _ => return None,
    };
    let count = |field: &str| goal.get(field).and_then(Value::as_u64);
    Some(Goal {
        objective: goal.get("objective")?.as_str()?.to_owned(),
        status,
        tokens_used: count("tokensUsed"),
        token_budget: count("tokenBudget"),
        time_used_secs: count("timeUsedSeconds"),
        checks: None,
        last_check: None,
    })
}

pub(super) fn goal_status_wire(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usageLimited",
        GoalStatus::BudgetLimited => "budgetLimited",
        GoalStatus::Complete => "complete",
    }
}

/// One `turn/plan/updated` step (`v2/TurnPlanStep.ts`).
/// The plan an `update_plan` call sets, as a rollout records its
/// arguments: `{explanation?, plan: [{step, status}]}` with a snake_case
/// status (`UpdatePlanArgs`, `protocol/src/plan_tool.rs` at
/// `rust-v0.167.0`). Live, Codex reports the same plan as
/// `turn/plan/updated`, never as a call; the importer reads it through this.
pub fn rollout_plan(arguments: &Value) -> Option<(Option<String>, Vec<PlanStep>)> {
    let steps = arguments
        .get("plan")?
        .as_array()?
        .iter()
        .filter_map(|step| {
            let status = match step.get("status")?.as_str()? {
                "pending" => PlanStepStatus::Pending,
                "in_progress" => PlanStepStatus::InProgress,
                "completed" => PlanStepStatus::Completed,
                _ => return None,
            };
            Some(PlanStep {
                step: step.get("step")?.as_str()?.to_owned(),
                status,
            })
        })
        .collect();
    let explanation = arguments
        .get("explanation")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((explanation, steps))
}

pub(super) fn map_plan_step(step: &Value) -> Option<PlanStep> {
    let status = match step.get("status")?.as_str()? {
        "pending" => PlanStepStatus::Pending,
        "inProgress" => PlanStepStatus::InProgress,
        "completed" => PlanStepStatus::Completed,
        _ => return None,
    };
    Some(PlanStep {
        step: step.get("step")?.as_str()?.to_owned(),
        status,
    })
}

pub(super) fn map_status(status: Option<&str>) -> ItemStatus {
    match status {
        Some("completed") => ItemStatus::Completed,
        Some("failed") => ItemStatus::Failed,
        Some("interrupted") => ItemStatus::Interrupted,
        Some("declined") => ItemStatus::Declined,
        _ => ItemStatus::InProgress,
    }
}

pub(super) fn map_file_change(change: &Value) -> Option<FileChange> {
    let kind_value = change.get("kind").unwrap_or(&Value::Null);
    let kind_type = kind_value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("update");
    let kind = match kind_type {
        "add" => FileChangeKind::Create,
        "delete" => FileChangeKind::Delete,
        "update" if kind_value.get("move_path").is_some_and(|v| !v.is_null()) => {
            FileChangeKind::Rename
        }
        _ => FileChangeKind::Modify,
    };
    let mut change = FileChange {
        path: change.get("path").and_then(Value::as_str)?.to_owned(),
        kind,
        diff: change
            .get("diff")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    normalize_file_change_diff(&mut change);
    Some(change)
}

pub(super) fn normalize_file_change_diff(change: &mut FileChange) {
    let sign = match change.kind {
        FileChangeKind::Create => '+',
        FileChangeKind::Delete => '-',
        _ => return,
    };
    let Some(content) = change.diff.as_ref().filter(|content| !content.is_empty()) else {
        return;
    };
    // Codex's add/delete `diff` is the complete file, even when that file itself looks like a patch.
    let lines = content.split_inclusive('\n').count();
    let mut patch = if sign == '+' {
        format!("@@ -0,0 +1,{lines} @@\n")
    } else {
        format!("@@ -1,{lines} +0,0 @@\n")
    };
    for line in content.split_inclusive('\n') {
        patch.push(sign);
        patch.push_str(line);
    }
    if !content.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file\n");
    }
    change.diff = Some(patch);
}

/// A `ConfigWarningNotification` or `DeprecationNoticeNotification`
/// (app-server protocol v2, codex-cli 0.169.0): its summary, then its details
/// where it has them. A configuration Codex could not read names the file and
/// place in `details` already; `path` and `range` say them again as data.
pub(super) fn notice_text(summary: &str, params: &Value) -> String {
    match params.get("details").and_then(Value::as_str) {
        Some(details) => format!("{summary}\n{details}"),
        None => summary.to_owned(),
    }
}

pub(super) fn map_usage(value: &Value) -> Option<TokenUsage> {
    let last = value.get("last")?;
    // The session-cumulative running total lives in a sibling `total` object.
    let total_processed_tokens = value.pointer("/total/totalTokens").and_then(Value::as_u64);
    Some(TokenUsage {
        freshness: crate::ContextFreshness::Current,
        context_window: value.get("modelContextWindow").and_then(Value::as_u64),
        total_processed_tokens,
        input_tokens: last.get("inputTokens").and_then(Value::as_u64),
        cached_input_tokens: last.get("cachedInputTokens").and_then(Value::as_u64),
        output_tokens: last.get("outputTokens").and_then(Value::as_u64),
        used_tokens: last.get("totalTokens").and_then(Value::as_u64),
        ..TokenUsage::default()
    })
}

/// `ModelReroutedNotification` (app-server protocol v2, codex-cli 0.167.1).
pub(super) fn model_reroute(params: &Value) -> Option<AgentEvent> {
    let to = params.get("toModel").and_then(Value::as_str)?.to_owned();
    let field = |name| params.get(name).and_then(Value::as_str).map(str::to_owned);
    Some(AgentEvent::ServedModel {
        model: to,
        from: field("fromModel"),
        reason: field("reason"),
    })
}
