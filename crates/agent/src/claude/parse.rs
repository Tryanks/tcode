use super::*;

/// Approval categories; tools without a dedicated category use `ToolUse`.
pub(super) enum ClaudeRequestType {
    FileRead,
    ExecCommand,
    FileChange,
    ToolUse,
}

/// The tools that spawn a subagent: `Agent` and its former name `Task`.
/// Coordination tools such as `ListAgents` and `SendMessage` do not.
pub(crate) fn is_agent_tool(normalized: &str) -> bool {
    matches!(normalized, "agent" | "task")
}

/// The in-progress Subagent snapshot for an Agent tool call. The call only
/// carries a model alias when the caller picked one; the resolved model and
/// effort arrive with the child's first assistant message.
pub(crate) fn spawned_subagent(input: &Value) -> ItemContent {
    ItemContent::Subagent {
        agent_type: input
            .get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("subagent")
            .to_owned(),
        description: subagent_description(input),
        status: ItemStatus::InProgress,
        summary: None,
        model: input
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
        effort: None,
    }
}

pub(super) fn subagent_description(input: &Value) -> String {
    input
        .get("description")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            input
                .get("prompt")
                .and_then(Value::as_str)
                .map(|prompt| prompt.chars().take(200).collect())
                .unwrap_or_default()
        })
}

/// The tool Claude Code calls to present its plan and leave plan mode.
pub(super) const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// The plan `ExitPlanMode` presents, which Claude Code fills in from the plan
/// file it wrote.
pub(super) fn plan_markdown(input: &Value) -> String {
    input
        .get("plan")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The Claude Code tools (2.1.322) whose approval has a category of its own;
/// every other tool, an MCP tool included, asks as itself.
pub(super) fn classify_claude_tool(name: &str) -> ClaudeRequestType {
    match name {
        "Read" | "Glob" | "Grep" | "WebSearch" => ClaudeRequestType::FileRead,
        "Bash" | "PowerShell" => ClaudeRequestType::ExecCommand,
        "Write" | "Edit" | "NotebookEdit" => ClaudeRequestType::FileChange,
        _ => ClaudeRequestType::ToolUse,
    }
}

/// Summarize a command, subagent brief, or tool input for its approval prompt.
pub(super) fn approval_detail(tool_name: &str, input: &Value) -> String {
    if let Some(cmd) = input.get("command").and_then(Value::as_str) {
        let clipped: String = cmd.trim().chars().take(400).collect();
        return format!("{tool_name}: {clipped}");
    }
    if is_agent_tool(&tool_name.to_lowercase()) {
        let body = subagent_description(input);
        return match input
            .get("subagent_type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(subtype) => format!("{subtype}: {body}"),
            None => body,
        };
    }
    let json = serde_json::to_string(input).unwrap_or_default();
    if json.chars().count() <= 400 {
        format!("{tool_name}: {json}")
    } else {
        let clipped: String = json.chars().take(397).collect();
        format!("{tool_name}: {clipped}...")
    }
}

/// Parse `AskUserQuestion` tool input into canonical [`UserInputQuestion`]s
/// with the complete question text as `id` (falling back to `q-<index>`);
/// options and empty labels are preserved (the Claude side does not filter).
pub(super) fn parse_ask_user_questions(input: &Value) -> Vec<UserInputQuestion> {
    let questions = match input.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .enumerate()
        .map(|(index, q)| {
            let question_text = q.get("question").and_then(Value::as_str);
            let id = match question_text {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => format!("q-{index}"),
            };
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Question {}", index + 1));
            let question = question_text.unwrap_or("").to_owned();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .map(|opt| UserInputOption {
                            label: opt
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            description: opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multi_select = q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            UserInputQuestion {
                id,
                header,
                question,
                options,
                multi_select,
                optional: false,
                prefill: None,
            }
        })
        .collect()
}

/// Questions the user has open, and what their answer has to carry back.
pub(super) enum PendingUserInput {
    /// `AskUserQuestion`: its `questions` input, echoed with the answers.
    AskUserQuestion { questions: Value },
    /// A form-mode MCP elicitation: its fields, in the order asked.
    Elicitation { fields: Vec<ElicitationField> },
}

/// One field of an MCP elicitation form: its property name and how the answer
/// to its question becomes the property's value.
pub(super) struct ElicitationField {
    key: String,
    kind: ElicitationKind,
}

pub(super) enum ElicitationKind {
    Text,
    Number,
    Boolean,
    /// Each option's label and the value it stands for.
    Choice(Vec<(String, String)>),
    Choices(Vec<(String, String)>),
}

/// The questions asking a form-mode `elicitation` control request's fields
/// (Claude Code 2.1.323 control schema; the field shapes are MCP 2025-11-25's
/// `PrimitiveSchemaDefinition`), each naming the server and its message.
/// `None` when the request is not a form or a field is of a shape a question
/// cannot ask.
pub(super) fn elicitation_questions(
    request: &Value,
) -> Option<(Vec<UserInputQuestion>, Vec<ElicitationField>)> {
    if !matches!(
        request.get("mode").and_then(Value::as_str),
        None | Some("form")
    ) {
        return None;
    }
    let properties = request
        .pointer("/requested_schema/properties")
        .and_then(Value::as_object)
        .filter(|properties| !properties.is_empty())?;
    // `property_order` is the order the server listed them in, which the
    // parsed object no longer has.
    let keys: Vec<&str> = match request.get("property_order").and_then(Value::as_array) {
        Some(order) => order.iter().filter_map(Value::as_str).collect(),
        None => properties.keys().map(String::as_str).collect(),
    };
    // A field the schema's `required` does not list may be left out; with no
    // `required`, none is (JSON Schema).
    let required: Vec<&str> = request
        .pointer("/requested_schema/required")
        .and_then(Value::as_array)
        .map(|required| required.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let asked_by = format!(
        "{}: {}",
        text(request, "mcp_server_name").unwrap_or_default(),
        text(request, "message").unwrap_or_default()
    );
    let titled = |choices: &Value| -> Option<Vec<(String, String)>> {
        choices
            .as_array()?
            .iter()
            .map(|choice| Some((text(choice, "title")?, text(choice, "const")?)))
            .collect()
    };
    let named = |values: &Value, names: &Value| -> Option<Vec<(String, String)>> {
        values
            .as_array()?
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let value = value.as_str()?;
                let name = names.get(index).and_then(Value::as_str).unwrap_or(value);
                Some((name.to_owned(), value.to_owned()))
            })
            .collect()
    };
    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for key in keys {
        let schema = properties.get(key)?;
        let mut prefill = None;
        let kind = match schema.get("type").and_then(Value::as_str)? {
            "string" if schema.get("enum").is_some() => {
                ElicitationKind::Choice(named(&schema["enum"], &schema["enumNames"])?)
            }
            "string" if schema.get("oneOf").is_some() => {
                ElicitationKind::Choice(titled(&schema["oneOf"])?)
            }
            "string" => {
                prefill = text(schema, "default");
                ElicitationKind::Text
            }
            "number" | "integer" => {
                prefill = schema
                    .get("default")
                    .and_then(Value::as_number)
                    .map(ToString::to_string);
                ElicitationKind::Number
            }
            "boolean" => {
                prefill = schema
                    .get("default")
                    .and_then(Value::as_bool)
                    .map(|default| default.to_string());
                ElicitationKind::Boolean
            }
            "array" => {
                let items = schema.get("items")?;
                ElicitationKind::Choices(match items.get("anyOf") {
                    Some(choices) => titled(choices)?,
                    None => named(items.get("enum")?, &Value::Null)?,
                })
            }
            _ => return None,
        };
        // A choice's default is offered as its option's label, which the
        // answer maps back to the value.
        if let ElicitationKind::Choice(choices) = &kind
            && let Some(default) = text(schema, "default")
        {
            prefill = choices
                .iter()
                .find(|(_, value)| *value == default)
                .map(|(label, _)| label.clone());
        }
        let labels: Vec<&str> = match &kind {
            ElicitationKind::Text | ElicitationKind::Number => Vec::new(),
            ElicitationKind::Boolean => vec!["true", "false"],
            ElicitationKind::Choice(choices) | ElicitationKind::Choices(choices) => {
                choices.iter().map(|(label, _)| label.as_str()).collect()
            }
        };
        questions.push(UserInputQuestion {
            id: key.to_owned(),
            header: text(schema, "title").unwrap_or_else(|| key.to_owned()),
            question: match text(schema, "description") {
                Some(description) => format!("{asked_by}\n\n{description}"),
                None => asked_by.clone(),
            },
            options: labels
                .into_iter()
                .map(|label| UserInputOption {
                    label: label.to_owned(),
                    description: String::new(),
                })
                .collect(),
            multi_select: matches!(kind, ElicitationKind::Choices(_)),
            prefill,
            optional: !required.contains(&key),
        });
        fields.push(ElicitationField {
            key: key.to_owned(),
            kind,
        });
    }
    Some((questions, fields))
}

/// The `content` of an accepted elicitation: each answered field's value in
/// the type its schema asks for. A field left empty is left out, and an answer
/// that is none of a field's options or not of its type goes as the user gave
/// it, for the server to judge.
pub(super) fn elicitation_content(
    fields: &[ElicitationField],
    answers: &serde_json::Map<String, Value>,
) -> serde_json::Map<String, Value> {
    let value_of = |choices: &[(String, String)], answer: &str| {
        let value = choices.iter().find(|(label, _)| label == answer);
        json!(value.map_or(answer, |(_, value)| value))
    };
    fields
        .iter()
        .filter_map(|field| {
            let answer: Vec<&str> = match answers.get(&field.key)? {
                Value::String(text) => vec![text.as_str()],
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let first = *answer.first().filter(|text| !text.is_empty())?;
            let value = match &field.kind {
                ElicitationKind::Text => json!(first),
                ElicitationKind::Number => serde_json::from_str::<serde_json::Number>(first.trim())
                    .map_or_else(|_| json!(first), Value::Number),
                ElicitationKind::Boolean => match first {
                    "true" => json!(true),
                    "false" => json!(false),
                    other => json!(other),
                },
                ElicitationKind::Choice(choices) => value_of(choices, first),
                ElicitationKind::Choices(choices) => answer
                    .iter()
                    .map(|answer| value_of(choices, answer))
                    .collect(),
            };
            Some((field.key.clone(), value))
        })
        .collect()
}

/// Derive canonical [`FileChange`]s from a file-editing tool's input.
pub(super) fn file_changes(name: &str, input: &Value) -> Vec<FileChange> {
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match name {
        "Write" => {
            let content = input.get("content").and_then(Value::as_str).unwrap_or("");
            vec![FileChange {
                path,
                kind: FileChangeKind::Create,
                diff: (!content.is_empty()).then(|| {
                    content
                        .lines()
                        .map(|l| format!("+{l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            }]
        }
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut diff = String::new();
            for l in old.lines() {
                diff.push('-');
                diff.push_str(l);
                diff.push('\n');
            }
            for l in new.lines() {
                diff.push('+');
                diff.push_str(l);
                diff.push('\n');
            }
            vec![FileChange {
                path,
                kind: FileChangeKind::Modify,
                diff: (!diff.is_empty()).then(|| diff.trim_end().to_string()),
            }]
        }
        _ => vec![FileChange {
            path,
            kind: FileChangeKind::Modify,
            diff: None,
        }],
    }
}

/// The file change that follows a command whose Bash result carries
/// `bashEditDiff`: the files Claude Code saw change while it ran. Those it
/// gives a diff for come first (`files`), then the rest of `changedFiles`,
/// whose operation it does not say.
pub fn command_changes(command_id: &str, tool_use_result: Option<&Value>) -> Option<ThreadItem> {
    let edit_diff = tool_use_result?.get("bashEditDiff")?;
    let list = |key: &str| edit_diff.get(key).and_then(Value::as_array);
    let flag = |file: &Value, key: &str| file.get(key).and_then(Value::as_bool) == Some(true);
    let mut changes: Vec<FileChange> = list("files")
        .into_iter()
        .flatten()
        .filter_map(|file| {
            Some(FileChange {
                path: file.get("filePath")?.as_str()?.to_owned(),
                kind: if flag(file, "created") {
                    FileChangeKind::Create
                } else if flag(file, "deleted") {
                    FileChangeKind::Delete
                } else {
                    FileChangeKind::Modify
                },
                diff: file.get("hunks").and_then(render_structured_patch),
            })
        })
        .collect();
    for path in list("changedFiles")
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !changes.iter().any(|change| change.path == path) {
            changes.push(FileChange {
                path: path.to_owned(),
                kind: FileChangeKind::Changed,
                diff: None,
            });
        }
    }
    (!changes.is_empty()).then(|| ThreadItem {
        id: format!("{command_id}:changes"),
        parent_item_id: None,
        content: ItemContent::FileChange {
            changes,
            status: ItemStatus::Completed,
            output: None,
            command: Some(command_id.to_owned()),
        },
    })
}

/// Render Claude's structured edit hunks as a unified diff body.
pub(super) fn render_structured_patch(value: &Value) -> Option<String> {
    let hunks = value.as_array()?;
    let mut rendered = Vec::new();
    for hunk in hunks {
        let old_start = hunk.get("oldStart").and_then(Value::as_u64)?;
        let old_lines = hunk.get("oldLines").and_then(Value::as_u64)?;
        let new_start = hunk.get("newStart").and_then(Value::as_u64)?;
        let new_lines = hunk.get("newLines").and_then(Value::as_u64)?;
        rendered.push(format!(
            "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@"
        ));
        rendered.extend(
            hunk.get("lines")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

pub(super) fn result_status(msg: &Value) -> TurnStatus {
    if msg.get("subtype").and_then(Value::as_str) == Some("success")
        && msg.get("is_error").and_then(Value::as_bool) != Some(true)
    {
        TurnStatus::Completed
    } else {
        TurnStatus::Failed
    }
}

pub(super) fn map_usage(usage: &Value, model_usage: Option<&Value>) -> TokenUsage {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let input = get("input_tokens");
    let cache_read = get("cache_read_input_tokens");
    let cache_creation = get("cache_creation_input_tokens");
    let output = get("output_tokens");

    let used = [input, cache_read, cache_creation, output]
        .into_iter()
        .flatten()
        .sum::<u64>();
    let used_tokens = (used > 0).then_some(used);

    let context_window = model_usage.and_then(Value::as_object).and_then(|m| {
        m.values()
            .filter_map(|v| v.get("contextWindow").and_then(Value::as_u64))
            .max()
    });

    TokenUsage {
        context_window,
        input_tokens: input,
        cached_input_tokens: cache_read,
        output_tokens: output,
        used_tokens,
        // Session-cumulative total is stamped by the caller (`on_result`); the
        // streaming/partial usage path leaves it unset.
        ..TokenUsage::default()
    }
}

/// Parse Claude system-init `slash_commands` (→ [`ProviderCommandKind::Command`])
/// and `skills` (→ [`ProviderCommandKind::Skill`]) into [`ProviderCommand`]s.
/// Both are arrays of bare name strings; the CLI supplies no descriptions. A
/// skill is listed under both, which is how the runtime knows a `$name`
/// mention of it is sent as the `/name` command the CLI expands.
pub(super) fn parse_provider_commands(init: &Value) -> Vec<ProviderCommand> {
    let mut out = Vec::new();
    let mut push = |field: &str, kind: ProviderCommandKind| {
        if let Some(names) = init.get(field).and_then(Value::as_array) {
            for name in names.iter().filter_map(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(ProviderCommand {
                        name: name.to_owned(),
                        description: None,
                        kind,
                    });
                }
            }
        }
    };
    push("slash_commands", ProviderCommandKind::Command);
    push("skills", ProviderCommandKind::Skill);
    out
}

/// The plan a call of `name` sets, when it is a `TodoWrite`: a session
/// shows it as the plan, not as a call. The importer reads a transcript's
/// calls through this.
pub fn todo_plan(name: &str, input: &Value) -> Option<Vec<PlanStep>> {
    is_todo_tool(name)
        .then(|| extract_plan_steps_from_todo(input))
        .flatten()
}

pub(super) fn is_todo_tool(name: &str) -> bool {
    name.to_lowercase().contains("todowrite")
}

/// Map `TodoWrite` input `{ todos: [{ content, status, activeForm? }] }` to
/// plan steps (content → step, fallback `"Task"`; completed/in_progress →
/// Completed/InProgress, else Pending). `activeForm` is ignored.
pub(super) fn extract_plan_steps_from_todo(input: &Value) -> Option<Vec<PlanStep>> {
    let todos = input.get("todos").and_then(Value::as_array)?;
    let steps = todos
        .iter()
        .filter(|todo| todo.is_object())
        .map(|todo| {
            let step = todo
                .get("content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Task")
                .to_owned();
            let status = match todo.get("status").and_then(Value::as_str) {
                Some("completed") => PlanStepStatus::Completed,
                Some("in_progress") => PlanStepStatus::InProgress,
                _ => PlanStepStatus::Pending,
            };
            PlanStep { step, status }
        })
        .collect();
    Some(steps)
}

pub(super) fn has_boolean_option(spec: &ModelSpec, id: &str) -> bool {
    spec.options
        .iter()
        .any(|o| matches!(o, OptionDescriptor::Boolean { id: oid, .. } if oid == id))
}

/// Resolve the effort selection against the model's `reasoningEffort`
/// descriptor: an accepted listed value wins, else the descriptor default.
/// Returns `None` when the model has no reasoning selector (e.g. Haiku).
pub(super) fn resolve_claude_effort(spec: Option<&ModelSpec>, raw: Option<&str>) -> Option<String> {
    let spec = spec?;
    let (options, default_value) = spec.options.iter().find_map(|o| match o {
        OptionDescriptor::Select {
            id,
            options,
            default_value,
            ..
        } if id == crate::EFFORT_OPTION_ID => Some((options, default_value)),
        _ => None,
    })?;
    if let Some(raw) = raw
        && options.iter().any(|o| o.value == raw)
    {
        return Some(raw.to_owned());
    }
    default_value.clone()
}
