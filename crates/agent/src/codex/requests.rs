use super::*;
use crate::AttachmentKind;

pub(super) fn approval_choices(
    method: &str,
    params: &Value,
) -> (Vec<ApprovalOption>, HashMap<String, Value>, Value) {
    if method == "item/permissions/requestApproval" {
        let mut permissions = params.get("permissions").cloned().unwrap_or(json!({}));
        if let Some(fields) = permissions.as_object_mut() {
            fields.retain(|_, value| !value.is_null());
        }
        let mut choices = Vec::new();
        let mut replies = HashMap::new();
        for (id, label, kind, permissions, scope) in [
            (
                "turn",
                "Grant for this turn",
                ApprovalOptionKind::AllowOnce,
                permissions.clone(),
                "turn",
            ),
            (
                "session",
                "Grant for this session",
                ApprovalOptionKind::AllowAlways,
                permissions,
                "session",
            ),
            (
                "decline",
                "Decline",
                ApprovalOptionKind::RejectOnce,
                json!({}),
                "turn",
            ),
        ] {
            choices.push(ApprovalOption {
                id: id.into(),
                label: label.into(),
                kind,
            });
            replies.insert(id.into(), json!({"permissions":permissions, "scope":scope}));
        }
        return (choices, replies, json!({"permissions":{}, "scope":"turn"}));
    }
    let defaults = json!(["accept", "acceptForSession", "decline", "cancel"]);
    let decisions = params
        .get("availableDecisions")
        .and_then(Value::as_array)
        .unwrap_or(defaults.as_array().expect("decision array"));
    let mut options = Vec::new();
    let mut replies = HashMap::new();
    for (index, decision) in decisions.iter().enumerate() {
        let (id, label, kind) = match decision.as_str() {
            Some("accept") => (
                "accept".to_owned(),
                "Allow once",
                ApprovalOptionKind::AllowOnce,
            ),
            Some("acceptForSession") => (
                "acceptForSession".to_owned(),
                "Allow for this session",
                ApprovalOptionKind::AllowAlways,
            ),
            Some("decline") => (
                "decline".to_owned(),
                "Decline",
                ApprovalOptionKind::RejectOnce,
            ),
            Some("cancel") => continue,
            _ if decision.get("acceptWithExecpolicyAmendment").is_some() => (
                format!("decision:{index}"),
                "Allow and remember this command",
                ApprovalOptionKind::AllowAlways,
            ),
            _ if decision.get("applyNetworkPolicyAmendment").is_some() => {
                let deny = decision
                    .pointer("/applyNetworkPolicyAmendment/network_policy_amendment/action")
                    .and_then(Value::as_str)
                    == Some("deny");
                (
                    format!("decision:{index}"),
                    if deny {
                        "Deny this host"
                    } else {
                        "Allow this host"
                    },
                    if deny {
                        ApprovalOptionKind::RejectAlways
                    } else {
                        ApprovalOptionKind::AllowAlways
                    },
                )
            }
            _ => continue,
        };
        options.push(ApprovalOption {
            id: id.clone(),
            label: label.into(),
            kind,
        });
        replies.insert(id, json!({"decision":decision}));
    }
    (options, replies, json!({"decision": "cancel"}))
}

pub(super) fn auto_review_action_summary(action: &Value) -> String {
    match action.get("type").and_then(Value::as_str) {
        Some("command") => format!(
            "Command: {}",
            shell_script(&string_field(action, "command"))
        ),
        Some("execve") => {
            let mut command = vec![string_field(action, "program")];
            command.extend(strings(action.get("argv")));
            format!("Command: {}", command.join(" "))
        }
        Some("writeStdin") => format!(
            "Input to process {}: {}",
            string_field(action, "processId"),
            string_field(action, "stdin")
        ),
        Some("applyPatch") => format!("Patch files: {}", strings(action.get("files")).join(", ")),
        Some("networkAccess") => format!(
            "Network target: {} ({}:{} via {})",
            string_field(action, "target"),
            string_field(action, "host"),
            action
                .get("port")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            string_field(action, "protocol")
        ),
        Some("mcpToolCall") => format!(
            "MCP tool: {}/{}",
            string_field(action, "server"),
            string_field(action, "toolName")
        ),
        Some("requestPermissions") => {
            let permissions = action.get("permissions").unwrap_or(&Value::Null);
            let mut requested = Vec::new();
            if let Some(enabled) = permissions
                .pointer("/network/enabled")
                .and_then(Value::as_bool)
            {
                requested.push(if enabled {
                    "network access".into()
                } else {
                    "no network access".into()
                });
            }
            if let Some(files) = permissions.get("fileSystem") {
                for (key, label) in [("read", "read"), ("write", "write")] {
                    let paths = strings(files.get(key));
                    if !paths.is_empty() {
                        requested.push(format!("{label}: {}", paths.join(", ")));
                    }
                }
                for entry in files
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let path = entry.get("path").unwrap_or(&Value::Null);
                    let path = path
                        .get("path")
                        .or_else(|| path.get("pattern"))
                        .or_else(|| path.pointer("/value/kind"))
                        .and_then(Value::as_str)
                        .unwrap_or("configured path");
                    requested.push(format!("{}: {path}", string_field(entry, "access")));
                }
            }
            let reason = action
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            format!(
                "Requested permissions: {}{reason}",
                if requested.is_empty() {
                    "none specified".into()
                } else {
                    requested.join("; ")
                }
            )
        }
        _ => "Action under review".into(),
    }
}

pub(super) fn guardian_event(params: &Value) -> Option<Value> {
    if let Some(event) = params.get("event").filter(|event| event.is_object()) {
        return Some(event.clone());
    }
    let review = params.get("review")?;
    let id = params.get("reviewId")?.as_str()?;
    let action = params.get("action")?;
    Some(json!({
        "id":id, "turn_id":params.get("turnId"), "target_item_id":params.get("targetItemId"),
        "started_at_ms":params.get("startedAtMs"), "completed_at_ms":params.get("completedAtMs"),
        "status":snake_case(review.get("status")?.as_str()?), "risk_level":review.get("riskLevel"),
        "user_authorization":review.get("userAuthorization"), "rationale":review.get("rationale"),
        "decision_source":params.get("decisionSource"), "action":native_guardian_value(action),
    }))
}

pub(super) fn native_guardian_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let value = if matches!(key.as_str(), "type" | "source")
                        && let Some(value) = value.as_str()
                    {
                        json!(snake_case(value))
                    } else {
                        native_guardian_value(value)
                    };
                    (snake_case(key), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(native_guardian_value).collect()),
        _ => value.clone(),
    }
}

pub(super) fn snake_case(value: &str) -> String {
    let mut result = String::new();
    for c in value.chars() {
        if c.is_ascii_uppercase() {
            result.push('_');
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// The text a message sends Codex: what was typed, with the path of each
/// attachment [`user_input`] does not carry appended.
pub(super) fn input_text(text: &str, attachments: &[Attachment]) -> String {
    crate::fold_attachments(text, attachments, takes).0
}

/// What the app-server takes as user input besides text (`UserInput` of
/// codex-cli 0.171.0): an image, and a sound by the path of its file
/// (`localAudio`). It has no document input.
fn takes(attachment: &Attachment) -> bool {
    match attachment.kind() {
        AttachmentKind::Image => true,
        AttachmentKind::Audio => attachment.source_path.is_some(),
        _ => false,
    }
}

/// The `input: UserInput[]` array shared by `turn/start` and `turn/steer`: text
/// first, then one `image` entry per image carrying a
/// `data:<mime>;base64,<data>` URL (the Codex app-server image-input shape)
/// and one `localAudio` entry per sound. Any other attachment is a path in
/// the text ([`input_text`]).
pub(super) fn user_input(text: &str, attachments: &[Attachment]) -> Value {
    let (text, taken) = crate::fold_attachments(text, attachments, takes);
    // A message of pictures alone carries no text item, as ChatGPT.app and
    // Codex's own TUI send one.
    let mut input = Vec::with_capacity(1 + taken.len());
    if !text.is_empty() || taken.is_empty() {
        input.push(json!({ "type": "text", "text": text, "text_elements": [] }));
    }
    for attachment in taken {
        input.push(match attachment.kind() {
            AttachmentKind::Audio => json!({
                "type": "localAudio",
                "path": attachment.source_path,
            }),
            _ => json!({
                "type": "image",
                "url": format!("data:{};base64,{}", attachment.media_type, attachment.data_base64),
            }),
        });
    }
    Value::Array(input)
}

/// The text parts of a `userMessage` item's `content: UserInput[]`, joined in
/// order; mirrors what [`user_input`] sent so a steer echo compares equal.
pub(super) fn user_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

pub(super) fn pending_name(request: Option<&PendingRequest>) -> &'static str {
    match request {
        Some(PendingRequest::TurnStart { .. }) => "turn/start",
        Some(PendingRequest::BackgroundTerminalList) => "thread/backgroundTerminals/list",
        Some(PendingRequest::BackgroundTerminalStop { method }) => method,
        Some(PendingRequest::Interrupt) => "turn/interrupt",
        Some(PendingRequest::Steer { .. }) => "turn/steer",
        Some(PendingRequest::SubagentMetadata { .. }) => "thread/read",
        Some(PendingRequest::GuardianApproval { .. }) => "thread/approveGuardianDeniedAction",
        Some(PendingRequest::InjectItems) => "thread/inject_items",
        Some(PendingRequest::Revert { .. }) => "thread/revert",
        Some(PendingRequest::Compact) => "thread/compact/start",
        Some(PendingRequest::GoalChange) => "thread/goal",
        Some(PendingRequest::GoalSnapshot) => "thread/goal/get",
        Some(PendingRequest::AccountUsage(_)) => "account/rateLimits/read",
        None => "unknown",
    }
}

pub(super) fn request_id_string(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

/// Normalize a canonical answer value into the wire array shape. A single
/// string becomes a 1-element array; an array of strings is kept; anything
/// else yields an empty array.
pub(super) fn strings(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn non_empty_string(value: &Value) -> Option<String> {
    strings(Some(value))
        .into_iter()
        .find(|answer| !answer.is_empty())
}

/// The reply to an elicitation, or the keys of the answers that do not fit
/// its schema. No answer at all declines.
pub(super) fn elicitation_result(
    fields: &[ElicitField],
    answers: &serde_json::Map<String, Value>,
) -> Result<Value, Vec<String>> {
    if fields.len() == 1 && matches!(fields[0].kind, ElicitFieldKind::UrlAck) {
        let action = answers
            .get(&fields[0].key)
            .and_then(non_empty_string)
            .filter(|answer| answer != ELICITATION_URL_CANCEL_LABEL)
            .map_or("cancel", |_| "accept");
        return Ok(json!({ "action": action }));
    }

    let mut content = serde_json::Map::new();
    let mut unusable = Vec::new();
    for field in fields {
        let answer = answers
            .get(&field.key)
            .filter(|answer| strings(Some(answer)).iter().any(|text| !text.is_empty()));
        let Some(answer) = answer else {
            if field.required {
                unusable.push(field.key.clone());
            }
            continue;
        };
        let value = match field.kind {
            ElicitFieldKind::Text => non_empty_string(answer).map(Value::String),
            ElicitFieldKind::Enum => {
                elicitation_enum_values(field, answer).and_then(|values| values.into_iter().next())
            }
            ElicitFieldKind::Number => non_empty_string(answer)
                .and_then(|answer| answer.parse::<f64>().ok())
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number),
            ElicitFieldKind::Integer => non_empty_string(answer)
                .and_then(|answer| answer.parse::<i64>().ok())
                .map(|number| json!(number)),
            ElicitFieldKind::Boolean => non_empty_string(answer).and_then(|answer| {
                match answer.to_ascii_lowercase().as_str() {
                    "yes" | "true" => Some(Value::Bool(true)),
                    "no" | "false" => Some(Value::Bool(false)),
                    _ => None,
                }
            }),
            ElicitFieldKind::MultiEnum => elicitation_enum_values(field, answer).map(Value::Array),
            ElicitFieldKind::UrlAck => None,
        };
        match value {
            Some(value) => {
                content.insert(field.key.clone(), value);
            }
            None => unusable.push(field.key.clone()),
        }
    }

    // No answers at all is the user dismissing the form; a form whose
    // optional fields were all left empty is still accepted.
    if answers.is_empty() {
        Ok(json!({ "action": "decline" }))
    } else if unusable.is_empty() {
        Ok(json!({ "action": "accept", "content": content }))
    } else {
        Err(unusable)
    }
}

/// The wire values of the chosen labels, or `None` when one is not offered.
pub(super) fn elicitation_enum_values(field: &ElicitField, answer: &Value) -> Option<Vec<Value>> {
    strings(Some(answer))
        .into_iter()
        .filter(|answer| !answer.is_empty())
        .map(|answer| {
            if field.values.is_empty() {
                Some(Value::String(answer))
            } else {
                field.values.get(&answer).cloned().map(Value::String)
            }
        })
        .collect()
}

pub(super) fn elicit_option(label: &str) -> UserInputOption {
    UserInputOption {
        label: label.to_owned(),
        description: String::new(),
    }
}

pub(super) fn schema_text<'a>(schema: &'a Value, key: &str) -> Option<&'a str> {
    schema
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub(super) fn titled_enum(
    entries: Option<&Vec<Value>>,
) -> (Vec<UserInputOption>, HashMap<String, String>) {
    enum_options(entries.into_iter().flatten().filter_map(|entry| {
        Some((
            schema_text(entry, "title")?.to_owned(),
            entry.get("const").and_then(Value::as_str)?.to_owned(),
        ))
    }))
}

pub(super) fn enum_options(
    entries: impl IntoIterator<Item = (String, String)>,
) -> (Vec<UserInputOption>, HashMap<String, String>) {
    let mut options = Vec::new();
    let mut values = HashMap::new();
    for (label, wire_value) in entries {
        if !label.is_empty() {
            options.push(elicit_option(&label));
            values.insert(label, wire_value);
        }
    }
    (options, values)
}

pub(super) fn parse_elicitation_form(
    params: &Value,
    server_name: &str,
) -> Option<(Vec<UserInputQuestion>, Vec<ElicitField>)> {
    let properties = params
        .pointer("/requestedSchema/properties")
        .and_then(Value::as_object)?;
    let message = schema_text(params, "message");
    let required = strings(params.pointer("/requestedSchema/required"));
    let mut questions = Vec::new();
    let mut fields = Vec::new();

    // `serde_json`'s `preserve_order` feature keeps questions in the schema's
    // declaration order, which is also their answering order.
    for (key, schema) in properties {
        let schema_type = schema.get("type").and_then(Value::as_str);
        let (kind, options, values) = match schema_type {
            Some("boolean") => (
                ElicitFieldKind::Boolean,
                vec![elicit_option("Yes"), elicit_option("No")],
                HashMap::new(),
            ),
            Some("string") if schema.get("oneOf").is_some() => {
                let (options, values) = titled_enum(schema.get("oneOf").and_then(Value::as_array));
                (ElicitFieldKind::Enum, options, values)
            }
            Some("string") if schema.get("enum").is_some() => {
                let wire_values = strings(schema.get("enum"));
                let names = strings(schema.get("enumNames"));
                if names.len() == wire_values.len() {
                    let (options, values) = enum_options(names.into_iter().zip(wire_values));
                    (ElicitFieldKind::Enum, options, values)
                } else {
                    let options = wire_values
                        .into_iter()
                        .filter(|label| !label.is_empty())
                        .map(|label| elicit_option(&label))
                        .collect();
                    (ElicitFieldKind::Enum, options, HashMap::new())
                }
            }
            Some("string") => (ElicitFieldKind::Text, Vec::new(), HashMap::new()),
            Some("number") => (ElicitFieldKind::Number, Vec::new(), HashMap::new()),
            Some("integer") => (ElicitFieldKind::Integer, Vec::new(), HashMap::new()),
            Some("array") if schema.pointer("/items/anyOf").is_some() => {
                let (options, values) =
                    titled_enum(schema.pointer("/items/anyOf").and_then(Value::as_array));
                (ElicitFieldKind::MultiEnum, options, values)
            }
            Some("array") if schema.pointer("/items/enum").is_some() => {
                let options = strings(schema.pointer("/items/enum"))
                    .into_iter()
                    .filter(|label| !label.is_empty())
                    .map(|label| elicit_option(&label))
                    .collect();
                (ElicitFieldKind::MultiEnum, options, HashMap::new())
            }
            _ => {
                log::debug!("codex: dropping unsupported elicitation field {key}");
                continue;
            }
        };
        let title = schema_text(schema, "title").unwrap_or(key);
        let question = schema_text(schema, "description")
            .or(message)
            .unwrap_or(key);
        questions.push(UserInputQuestion {
            id: key.clone(),
            header: format!("{server_name}: {title}"),
            question: question.to_owned(),
            options,
            multi_select: matches!(kind, ElicitFieldKind::MultiEnum),
            optional: !required.contains(key),
            prefill: None,
        });
        fields.push(ElicitField {
            key: key.clone(),
            kind,
            required: required.contains(key),
            values,
        });
    }
    Some((questions, fields))
}

pub(super) fn parse_elicitation_url(
    params: &Value,
    server_name: &str,
) -> (Vec<UserInputQuestion>, Vec<ElicitField>) {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The agent crate has no window handle, so keep the URL visible and
    // selectable in the question panel instead of attempting to open it.
    let question = UserInputQuestion {
        id: "url".into(),
        header: format!("{server_name}: open link"),
        question: format!("{message}\n\n{url}"),
        options: vec![
            elicit_option(ELICITATION_URL_ACK_LABEL),
            elicit_option(ELICITATION_URL_CANCEL_LABEL),
        ],
        multi_select: false,
        optional: false,
        prefill: None,
    };
    let field = ElicitField {
        key: "url".into(),
        kind: ElicitFieldKind::UrlAck,
        required: false,
        values: HashMap::new(),
    };
    (vec![question], vec![field])
}

/// Map an `item/tool/requestUserInput` params object into canonical
/// [`UserInputQuestion`]s. A question without a non-empty `id` cannot be
/// answered and is dropped. Questions with
/// zero options remain available for free-text answers. `multiSelect` is forced
/// false.
pub(super) fn parse_codex_user_input(params: &Value) -> Vec<UserInputQuestion> {
    let questions = match params.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .filter_map(|q| {
            let id = q
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let header = q.get("header").and_then(Value::as_str).unwrap_or_default();
            let question = q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|opt| {
                            let label = opt.get("label").and_then(Value::as_str)?;
                            let description = opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            Some(UserInputOption {
                                label: label.to_owned(),
                                description: description.to_owned(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UserInputQuestion {
                id: id.to_owned(),
                header: header.to_owned(),
                question: question.to_owned(),
                options,
                multi_select: false,
                optional: false,
                prefill: None,
            })
        })
        .collect()
}

/// Map a completed `agentMessage`'s `questions` (the `request_user_input_async`
/// tool's `{title, options?}` list) into canonical questions. The id is the
/// desktop app's `JSON.stringify([tool, message id, index])`, which is what
/// the reply envelope must echo back; the tool has no header. Questions with
/// an empty title and options with an empty label are dropped.
pub(super) const ASYNC_QUESTION_TOOL: &str = "request_user_input_async";

pub(super) fn has_async_questions(item: &Value) -> bool {
    item.get("questions")
        .and_then(Value::as_array)
        .is_some_and(|questions| !questions.is_empty())
}

pub(super) fn parse_codex_async_questions(
    message_id: &str,
    item: &Value,
) -> Vec<UserInputQuestion> {
    let questions = match item.get("questions").and_then(Value::as_array) {
        Some(questions) => questions,
        None => return Vec::new(),
    };
    questions
        .iter()
        .enumerate()
        .filter_map(|(index, q)| {
            let title = q
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())?;
            let options = strings(q.get("options"))
                .into_iter()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .map(|label| UserInputOption {
                    label,
                    description: String::new(),
                })
                .collect();
            Some(UserInputQuestion {
                id: json!([ASYNC_QUESTION_TOOL, message_id, index]).to_string(),
                header: String::new(),
                question: title.to_owned(),
                options,
                multi_select: false,
                optional: false,
                prefill: None,
            })
        })
        .collect()
}

/// Codex's `request_user_input_async` reply: the `<send_user_message_question_reply>`
/// envelope the desktop app and TUI send, listing each answered question with
/// its `questionItemId` so the model correlates the answer. Returns the wire
/// text and the plain `> question` / answer rendering the timeline shows, or
/// `None` when no question was answered. Question text is flattened to one
/// line and capped at 512 characters, as the TUI does.
pub(super) fn async_question_reply(
    questions: &[UserInputQuestion],
    answers: &serde_json::Map<String, Value>,
) -> Option<(String, String)> {
    let mut replies = Vec::new();
    let mut display = Vec::new();
    for question in questions {
        let answer = strings(answers.get(&question.id)).join(", ");
        let answer = answer.trim();
        if answer.is_empty() {
            continue;
        }
        let title = question
            .question
            .chars()
            .take(512)
            .collect::<String>()
            .replace(['\n', '\r'], " ");
        replies.push(json!({
            "answer": answer,
            "question": title,
            "questionItemId": question.id,
        }));
        display.push(format!("> {title}\n\n{answer}"));
    }
    if replies.is_empty() {
        return None;
    }
    let wire = format!(
        "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
        Value::Array(replies)
    );
    Some((wire, display.join("\n\n")))
}
