use super::*;

impl Actor {
    pub(super) async fn handle_server_request(&mut self, value: &Value) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let key = request_id_string(&id);
        let params = value.get("params").unwrap_or(&Value::Null);
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item_id = params
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // Structured user-input request: map to a canonical UserInputRequested
        // and remember the JSON-RPC id so RespondUserInput can reply.
        if method == "item/tool/requestUserInput" {
            let questions = parse_codex_user_input(params);
            self.user_inputs.insert(key.clone(), id);
            self.events
                .emit(AgentEvent::UserInputRequested {
                    request_id: key,
                    parent_item_id: None,
                    questions,
                    delivery: UserInputDelivery::Blocking,
                })
                .await;
            return;
        }

        if method == "mcpServer/elicitation/request" {
            self.handle_elicitation_request(key, id, params).await;
            return;
        }

        let kind = match method {
            "item/commandExecution/requestApproval" => ApprovalKind::ExecCommand {
                command: shell_script(&string_field(params, "command")),
                cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                description: None,
            },
            "item/fileChange/requestApproval" => ApprovalKind::FileChange {
                changes: self
                    .items
                    .get(item_id)
                    .and_then(|item| match &item.content {
                        ItemContent::FileChange { changes, .. } => Some(changes.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            },
            "item/permissions/requestApproval" => ApprovalKind::ToolUse {
                name: "request_permissions".into(),
                input: params.get("permissions").cloned().unwrap_or(Value::Null),
                detail: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Grant requested permissions")
                    .into(),
            },
            _ => {
                let _ = send_json(
                    &mut self.stdin,
                    &json!({ "id": id, "error": { "code": -32601, "message": format!("unsupported server request: {method}") } }),
                );
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("unsupported Codex server request: {method}"),
                    })
                    .await;
                return;
            }
        };
        let (options, replies, cancel_reply) = approval_choices(method, params);
        self.approvals.insert(
            key.clone(),
            PendingApproval {
                rpc_id: id,
                replies,
                cancel_reply,
                cancel_interrupt: (method == "item/permissions/requestApproval").then(
                    || json!({"threadId":params.get("threadId"),"turnId":params.get("turnId")}),
                ),
                item_id: (!item_id.is_empty()).then(|| item_id.to_owned()),
            },
        );
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {
                id: key,
                parent_item_id: None,
                turn_id,
                // A permissions request's reason is already its detail.
                reason: (!matches!(kind, ApprovalKind::ToolUse { .. }))
                    .then(|| params.get("reason").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_owned),
                blocked_path: None,
                kind,
                options,
            }))
            .await;
    }

    pub(super) async fn handle_elicitation_request(
        &mut self,
        request_id: String,
        rpc_id: Value,
        params: &Value,
    ) {
        let server_name = params
            .get("serverName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("MCP");
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if mode == "form"
            && params
                .pointer("/requestedSchema/properties")
                .and_then(Value::as_object)
                .is_some_and(|fields| fields.is_empty())
        {
            let mut choices = vec![(
                "accept".to_owned(),
                "Allow once".to_owned(),
                ApprovalOptionKind::AllowOnce,
                json!({"action":"accept", "content":{}}),
            )];
            if let Some(scopes) = params.pointer("/_meta/persist").and_then(Value::as_array) {
                for scope in scopes.iter().filter_map(Value::as_str) {
                    let label = match scope {
                        "session" => "Allow for this session",
                        "always" => "Always allow",
                        _ => continue,
                    };
                    choices.push((
                        format!("accept:{scope}"),
                        label.into(),
                        ApprovalOptionKind::AllowAlways,
                        json!({"action":"accept", "content":{}, "_meta":{"persist":scope}}),
                    ));
                }
            }
            choices.push((
                "decline".into(),
                "Decline".into(),
                ApprovalOptionKind::RejectOnce,
                json!({"action":"decline"}),
            ));
            let options = choices
                .iter()
                .map(|(id, label, kind, _)| ApprovalOption {
                    id: id.clone(),
                    label: label.clone(),
                    kind: *kind,
                })
                .collect();
            let replies = choices
                .into_iter()
                .map(|(id, _, _, reply)| (id, reply))
                .collect();
            self.approvals.insert(
                request_id.clone(),
                PendingApproval {
                    rpc_id,
                    replies,
                    cancel_reply: json!({"action":"cancel"}),
                    cancel_interrupt: None,
                    item_id: None,
                },
            );
            self.events
                .emit(AgentEvent::ApprovalRequested(ApprovalRequest {
                    id: request_id,
                    parent_item_id: None,
                    turn_id: params
                        .get("turnId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    kind: ApprovalKind::ToolUse {
                        name: server_name.into(),
                        input: params
                            .pointer("/_meta/tool_params")
                            .cloned()
                            .unwrap_or(Value::Null),
                        detail: params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP approval")
                            .into(),
                    },
                    reason: None,
                    blocked_path: None,
                    options,
                }))
                .await;
            return;
        }

        let parsed = match mode {
            "form" => parse_elicitation_form(params, server_name),
            "url" => Some(parse_elicitation_url(params, server_name)),
            _ => None,
        };

        let Some((questions, fields)) = parsed.filter(|(questions, _)| !questions.is_empty())
        else {
            let _ = send_json(
                &mut self.stdin,
                &json!({ "id": rpc_id, "result": { "action": "decline" } }),
            );
            let message = if mode == "form" {
                format!(
                    "codex: MCP server {server_name} sent an elicitation with no supported fields; declined"
                )
            } else {
                format!(
                    "codex: MCP server {server_name} sent an unsupported \"{mode}\" elicitation; declined"
                )
            };
            self.events.emit(AgentEvent::Warning { message }).await;
            return;
        };

        self.elicitations.insert(
            request_id.clone(),
            PendingElicitation {
                rpc_id,
                questions: questions.clone(),
                fields,
            },
        );
        self.events
            .emit(AgentEvent::UserInputRequested {
                request_id,
                parent_item_id: None,
                questions,
                delivery: UserInputDelivery::Blocking,
            })
            .await;
    }
}
