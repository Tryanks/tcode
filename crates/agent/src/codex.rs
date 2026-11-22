//! Codex provider: a small client for the newline-delimited JSON protocol used
//! by `codex app-server`.

use crate::{
    ApprovalPolicy, ApprovalReviewer, ForkPoint, LaterHistory, PermissionSandbox,
    PermissionSummary, RewindFailure, RewindFailureReason, RewindMode, Speed, approval_class,
};
use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};

use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::process::{
    ChildOutput, ProcessTree, StderrTail, send_json as write_json, spawn_line_reader,
};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, AskedQuestion, Attachment, ChangeCompleteness, DeltaKind, FileChange,
    FileChangeKind, Goal, GoalStatus, ItemContent, ItemStatus, LaunchEnv, ModelSpec,
    OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus, ProviderCommand,
    ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TokenUsage, ToolFamily, ToolOutput, TurnOptions, TurnStatus,
    UserInputDelivery, UserInputOption, UserInputQuestion, file_changes_from_unified_diff,
    permission_control, selection_str,
};

mod items;
mod notifications;
mod requests;
mod server_requests;
mod subagents;
pub use items::*;
use requests::*;
mod developer_instructions;
mod model_windows;
pub(crate) mod plugins;
pub use developer_instructions::PREVIEW_BROWSER_INSTRUCTIONS;

/// The `additionalContext` key [`SessionOptions::instructions`] ride under.
const INSTRUCTIONS_CONTEXT_KEY: &str = "tcode";

/// The id of Codex's permission control ([`permission_control`]).
const PERMISSIONS_OPTION_ID: &str = "permissions";

const ELICITATION_URL_ACK_LABEL: &str = "I've opened the link";
const ELICITATION_URL_CANCEL_LABEL: &str = "Cancel";

fn permission_params(value: &str, turn: bool) -> Result<Value, String> {
    let (sandbox, policy, reviewer) = match value {
        "ask" => ("workspace-write", "on-request", "user"),
        "auto_review" => ("workspace-write", "on-request", "auto_review"),
        "read_only" => ("read-only", "on-request", "user"),
        "full_access" => ("danger-full-access", "never", "user"),
        value
            if value
                .strip_prefix("profile:")
                .is_some_and(|id| !id.is_empty()) =>
        {
            return Ok(
                json!({"permissions": &value[8..], "approvalPolicy": "on-request", "approvalsReviewer": "user"}),
            );
        }
        _ => return Err(format!("unknown Codex permission value: {value}")),
    };
    let mut params = json!({"approvalPolicy": policy, "approvalsReviewer": reviewer});
    if turn {
        params["sandboxPolicy"] = match sandbox {
            "workspace-write" => {
                json!({"type":"workspaceWrite", "writableRoots":[], "networkAccess":false, "excludeTmpdirEnvVar":false, "excludeSlashTmp":false})
            }
            "read-only" => json!({"type":"readOnly", "networkAccess":false}),
            _ => json!({"type":"dangerFullAccess"}),
        };
    } else {
        params["sandbox"] = json!(sandbox);
    }
    Ok(params)
}

fn requested_permission(selections: &[OptionSelection]) -> String {
    selection_str(selections, "permissions").unwrap_or_else(|| "ask".into())
}

fn effective_permission(result: &Value) -> String {
    let profile = result
        .pointer("/activePermissionProfile/id")
        .and_then(Value::as_str);
    if let Some(id) = profile.filter(|id| !id.starts_with(':')) {
        return format!("profile:{id}");
    }
    let sandbox = result
        .get("sandboxPolicy")
        .or_else(|| result.get("sandbox"))
        .and_then(|sandbox| sandbox.get("type"))
        .and_then(Value::as_str);
    let policy = result.get("approvalPolicy").and_then(Value::as_str);
    let reviewer = result
        .get("approvalsReviewer")
        .and_then(Value::as_str)
        .unwrap_or("user");
    match (sandbox, policy, reviewer) {
        (Some("workspaceWrite"), Some("on-request"), "user") => "ask".into(),
        (Some("workspaceWrite"), Some("on-request"), "auto_review" | "guardian_subagent") => {
            "auto_review".into()
        }
        (Some("readOnly"), Some("on-request"), "user") => "read_only".into(),
        (Some("dangerFullAccess"), Some("never"), "user") => "full_access".into(),
        _ => format!(
            "effective:{}",
            json!({"approvalPolicy":result.get("approvalPolicy"), "approvalsReviewer":reviewer, "sandbox":result.get("sandboxPolicy").or_else(|| result.get("sandbox")), "activePermissionProfile":result.get("activePermissionProfile")})
        ),
    }
}

fn current_permission_option(effective: &str, settings: &Value) -> SelectOption {
    let sandbox = settings
        .get("sandboxPolicy")
        .or_else(|| settings.get("sandbox"))
        .and_then(|sandbox| sandbox.get("type"))
        .and_then(Value::as_str);
    // `v2/SandboxPolicy.ts`, `AskForApproval.ts` and `ApprovalsReviewer.ts`
    // (codex 0.167.0); the reviewer is `user` when unset.
    let sandbox = match sandbox {
        Some("readOnly") => Some(PermissionSandbox::ReadOnly),
        Some("workspaceWrite") => Some(PermissionSandbox::WorkspaceWrite),
        Some("dangerFullAccess") => Some(PermissionSandbox::FullAccess),
        Some("externalSandbox") => Some(PermissionSandbox::External),
        _ => None,
    };
    let approval = match settings.get("approvalPolicy") {
        Some(Value::String(policy)) => match policy.as_str() {
            "on-request" => Some(ApprovalPolicy::OnRequest),
            "never" => Some(ApprovalPolicy::Never),
            "untrusted" => Some(ApprovalPolicy::Untrusted),
            _ => None,
        },
        Some(Value::Object(policy)) if policy.contains_key("granular") => {
            Some(ApprovalPolicy::Granular)
        }
        _ => None,
    };
    let reviewer = match settings.get("approvalsReviewer").and_then(Value::as_str) {
        Some("auto_review" | "guardian_subagent") => Some(ApprovalReviewer::AutoReview),
        Some("user") | None => Some(ApprovalReviewer::User),
        _ => None,
    };
    let profile = settings
        .pointer("/activePermissionProfile/id")
        .and_then(Value::as_str)
        .or_else(|| effective.strip_prefix("profile:"));
    SelectOption {
        value: effective.into(),
        label: profile.unwrap_or("Current permissions").into(),
        description: None,
        unavailable: Some("Inherited permission configuration".into()),
        speed: None,
        summary: Some(PermissionSummary {
            profile: profile.map(str::to_owned),
            sandbox,
            approval,
            reviewer,
        }),
        approval: approval_class(ProviderKind::Codex, PERMISSIONS_OPTION_ID, effective),
        apply: None,
    }
}

fn permission_descriptor(
    profiles: &[Value],
    effective: &str,
    settings: &Value,
) -> OptionDescriptor {
    let mut descriptor = permission_control(ProviderKind::Codex).expect("Codex permission control");
    let OptionDescriptor::Select { options, .. } = &mut descriptor else {
        unreachable!()
    };
    for profile in profiles {
        let Some(id) = profile.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.starts_with(':') {
            if profile.get("allowed").and_then(Value::as_bool) == Some(false) {
                for option in options.iter_mut().filter(|option| match id {
                    ":workspace" => matches!(option.value.as_str(), "ask" | "auto_review"),
                    ":read-only" => option.value == "read_only",
                    ":danger-full-access" => option.value == "full_access",
                    _ => false,
                }) {
                    option.unavailable = Some("Not allowed by your configuration".into());
                }
            }
            continue;
        }
        let value = format!("profile:{id}");
        if options.iter().any(|option| option.value == value) {
            continue;
        }
        options.push(SelectOption {
            label: id.into(),
            description: profile
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.is_empty())
                .map(str::to_owned),
            unavailable: (profile.get("allowed").and_then(Value::as_bool) == Some(false))
                .then(|| "Not allowed by your configuration".into()),
            speed: None,
            summary: None,
            approval: approval_class(ProviderKind::Codex, PERMISSIONS_OPTION_ID, &value),
            apply: None,
            value,
        });
    }
    if !options.iter().any(|option| option.value == effective) {
        options.push(current_permission_option(effective, settings));
    }
    descriptor
}

/// Starts an app-server process and waits until its thread is ready.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    crate::spawn_session(
        ProviderKind::Codex,
        opts,
        run_actor,
        "codex actor exited before reporting startup status",
    )
    .await
}

/// Spawn `codex app-server`, page through `model/list` (initial `{}`, then
/// `{cursor}` until `nextCursor` is empty), and tear the process down.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    processes: &crate::SessionProcesses,
) -> Result<Vec<ModelSpec>, AgentError> {
    let (child, mut stdin, lines, mut stderr_tail) =
        spawn_server(binary_path.as_deref(), &[], &launch_env)?;
    let child = processes.hold(child);
    let windows = home_dir(&launch_env)
        .map(|home| model_windows::read(&home))
        .unwrap_or_default();
    let result = collect_models(&mut stdin, &lines, &windows).await;
    finish_one_shot(child, stdin, &mut stderr_tail, result).await
}

/// Read the account-wide rate limits from `codex app-server` and tear the
/// process down after the single request completes.
pub async fn read_rate_limits(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let (child, mut stdin, lines, mut stderr_tail) =
        spawn_server(binary_path.as_deref(), &[], &launch_env)?;
    let result = async {
        initialize(&mut stdin, &lines).await?;
        send_json(
            &mut stdin,
            &json!({ "id": 2, "method": "account/rateLimits/read", "params": {} }),
        )?;
        wait_for_response(&lines, 2).await
    }
    .await;
    finish_one_shot(child, stdin, &mut stderr_tail, result).await
}

/// Stop a server started for one request, and fold its death into the
/// request's error.
async fn finish_one_shot<T>(
    child: ProcessTree<Child>,
    stdin: BufWriter<ChildStdin>,
    stderr_tail: &mut StderrTail,
    result: Result<T, AgentError>,
) -> Result<T, AgentError> {
    match result {
        Ok(value) => {
            let mut child = child;
            stop_child(&mut child, stdin);
            Ok(value)
        }
        Err(err) => {
            let (mut child, status) = exit_status_once_gone(child, &err).await;
            stop_child(&mut child, stdin);
            Err(enrich_startup_error(err, status, stderr_tail))
        }
    }
}

async fn initialize(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
) -> Result<(), AgentError> {
    send_json(
        stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "tcode", "title": "Tcode", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
            }
        }),
    )?;
    wait_for_response(lines, 1).await?;
    send_json(stdin, &json!({ "method": "initialized" }))
}

/// Codex's home: `CODEX_HOME` as the CLI launched with `launch_env` sees it,
/// else `~/.codex`.
// What a new account copies of another's home: never `auth.json`, the
// sessions or the history, which belong to the account they were made in.
const INHERITED_CONFIG: [&str; 4] = ["config.toml", "AGENTS.md", "prompts", "skills"];

/// Copy the user's configuration from one `CODEX_HOME` into another. Blocking.
pub fn inherit_config(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut ancestors = std::collections::HashSet::new();
    INHERITED_CONFIG.iter().try_for_each(|name| {
        crate::claude::copy_resolved(&from.join(name), &to.join(name), &mut ancestors)
    })
}

pub fn home_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .child_path(ProviderKind::Codex, "CODEX_HOME")
        .or_else(|| {
            launch_env
                .child_home(ProviderKind::Codex)
                .map(|home| home.join(".codex"))
        })
}

async fn collect_models(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    windows: &HashMap<String, model_windows::ModelWindows>,
) -> Result<Vec<ModelSpec>, AgentError> {
    initialize(stdin, lines).await?;
    let entries = list_models_raw(stdin, lines, &mut 2, false, &mut Vec::new()).await?;
    Ok(entries
        .iter()
        .filter_map(|model| {
            let slug = model.get("model").and_then(Value::as_str);
            map_model(model, slug.and_then(|slug| windows.get(slug)))
        })
        .collect())
}

/// Every `model/list` entry, page after page (initial `{}`, then `{cursor}`
/// until `nextCursor` is empty); `include_hidden` adds the models hidden from
/// the picker.
async fn list_models_raw(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    next_id: &mut i64,
    include_hidden: bool,
    said: &mut Vec<String>,
) -> Result<Vec<Value>, AgentError> {
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        if include_hidden {
            params["includeHidden"] = json!(true);
        }
        let id = *next_id;
        *next_id += 1;
        send_json(
            stdin,
            &json!({ "id": id, "method": "model/list", "params": params }),
        )?;
        let response = wait_for_answer(lines, id, said).await?;
        if let Some(data) = response.get("data").and_then(Value::as_array) {
            entries.extend(data.iter().cloned());
        }
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(entries);
        }
    }
}

/// Map one `model/list` entry to a [`ModelSpec`]; `None` for hidden models.
fn map_model(model: &Value, windows: Option<&model_windows::ModelWindows>) -> Option<ModelSpec> {
    if model
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let id = model.get("model").and_then(Value::as_str)?.to_owned();
    let display_name = codex_display_name(
        model
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&id),
    );
    let is_default = model
        .get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut options = Vec::new();

    if let Some(efforts) = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
    {
        let select_options: Vec<SelectOption> = efforts
            .iter()
            .filter_map(|entry| {
                let value = entry
                    .get("reasoningEffort")
                    .and_then(Value::as_str)?
                    .to_owned();
                let label = reasoning_effort_label(&value);
                let description = entry
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Some(SelectOption {
                    unavailable: None,
                    value,
                    label,
                    description,
                    speed: None,
                    summary: None,
                    approval: None,
                    apply: None,
                })
            })
            .collect();
        if !select_options.is_empty() {
            let default_value = model
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            options.push(OptionDescriptor::Select {
                role: crate::OptionRole::Effort,
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: crate::EFFORT_OPTION_ID.into(),
                label: "Reasoning".into(),
                options: select_options,
                default_value,
                minimum: None,
            });
        }
    }

    let tiers = service_tiers(model);
    if !tiers.is_empty() {
        let default_value = model
            .get("defaultServiceTier")
            .and_then(Value::as_str)
            .filter(|d| tiers.iter().any(|t| t.value == *d))
            .map(str::to_owned)
            .unwrap_or_else(|| "default".into());
        let mut select_options = Vec::with_capacity(tiers.len() + 1);
        select_options.push(SelectOption {
            unavailable: None,
            value: "default".into(),
            label: "Standard".into(),
            description: None,
            speed: None,
            summary: None,
            approval: None,
            apply: None,
        });
        select_options.extend(tiers);
        options.push(OptionDescriptor::Select {
            role: crate::OptionRole::Speed,
            apply: Default::default(),
            recommended: None,
            permissive: None,
            id: "serviceTier".into(),
            label: "Service Tier".into(),
            options: select_options,
            default_value: Some(default_value),
            minimum: None,
        });
    }
    options.push(context_window_descriptor(windows));

    Some(ModelSpec {
        id,
        display_name,
        is_default,
        options,
    })
}

/// The `contextWindow` selector: the native window (default) and the largest
/// one Codex accepts for `model_context_window`. Without cached windows the
/// selector has no presets and the custom size stands alone.
fn context_window_descriptor(windows: Option<&model_windows::ModelWindows>) -> OptionDescriptor {
    let preset = |tokens: u64| {
        let label = crate::context_window::format(tokens);
        SelectOption {
            value: label.to_ascii_lowercase(),
            label,
            description: None,
            unavailable: None,
            speed: None,
            summary: None,
            approval: None,
            apply: None,
        }
    };
    let (options, default_value) = match windows {
        Some(windows) => {
            let mut options = vec![preset(windows.native)];
            if windows.largest > windows.native {
                options.push(preset(windows.largest));
            }
            let default_value = Some(options[0].value.clone());
            (options, default_value)
        }
        None => (Vec::new(), None),
    };
    OptionDescriptor::Select {
        role: crate::OptionRole::ContextWindow,
        apply: Default::default(),
        recommended: None,
        permissive: None,
        id: crate::context_window::OPTION_ID.into(),
        label: "Context Window".into(),
        options,
        default_value,
        minimum: None,
    }
}

fn service_tiers(model: &Value) -> Vec<SelectOption> {
    if let Some(tiers) = model.get("serviceTiers").and_then(Value::as_array)
        && !tiers.is_empty()
    {
        return tiers
            .iter()
            .filter_map(|tier| {
                let value = tier.get("id").and_then(Value::as_str)?.to_owned();
                let label = tier
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_owned();
                let description = tier
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Some(SelectOption {
                    unavailable: None,
                    speed: tier_speed(&value, &label),
                    value,
                    label,
                    description,
                    summary: None,
                    approval: None,
                    apply: None,
                })
            })
            .collect();
    }
    Vec::new()
}

/// Which service tiers are Codex's fast mode, as ChatGPT.app (26.1103) tells
/// them: the `priority` or `fast` id or a tier named Fast, and the
/// `ultrafast` id or a tier named Ultrafast.
fn tier_speed(id: &str, name: &str) -> Option<Speed> {
    let name = name.trim().to_ascii_lowercase();
    if matches!(id, "priority" | "fast") || name == "fast" {
        Some(Speed::Fast)
    } else if id == "ultrafast" || name == "ultrafast" {
        Some(Speed::Ultrafast)
    } else {
        None
    }
}

/// Format model ids for display: `gpt…` → `GPT…`, capitalizing the letter
/// after each hyphen.
fn codex_display_name(raw: &str) -> String {
    let base = if raw.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("gpt")) {
        format!("GPT{}", &raw[3..])
    } else {
        raw.to_owned()
    };
    let mut out = String::with_capacity(base.len());
    let mut after_hyphen = false;
    for c in base.chars() {
        if after_hyphen && c.is_ascii_lowercase() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
        after_hyphen = c == '-';
    }
    out
}

fn reasoning_effort_label(effort: &str) -> String {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// Selected reasoning effort (option id `reasoningEffort`).
fn codex_effort(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, crate::EFFORT_OPTION_ID)
}

/// What `model/list` says of each model's reasoning effort (`v2/Model.ts`).
#[derive(Default)]
struct ModelEfforts {
    /// Slug → its supported efforts and its default one.
    models: HashMap<String, (Vec<String>, String)>,
    /// The slug the list marks `isDefault`.
    default_model: Option<String>,
}

impl ModelEfforts {
    fn from_list(entries: &[Value]) -> Self {
        let mut efforts = Self::default();
        for entry in entries {
            let (Some(slug), Some(default)) = (
                entry.get("model").and_then(Value::as_str),
                entry.get("defaultReasoningEffort").and_then(Value::as_str),
            ) else {
                continue;
            };
            let supported = entry
                .get("supportedReasoningEfforts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("reasoningEffort").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            if entry.get("isDefault").and_then(Value::as_bool) == Some(true) {
                efforts.default_model = Some(slug.to_owned());
            }
            efforts
                .models
                .insert(slug.to_owned(), (supported, default.to_owned()));
        }
        efforts
    }

    /// The effort a turn of `model` runs at, in ChatGPT.app's order: the one
    /// chosen or configured when the model supports it, else the model's
    /// default; for a model the list does not have, the chosen or configured
    /// one, else the default model's default, and only then `medium`.
    fn resolve(&self, model: &str, chosen: Option<&str>) -> String {
        match self.models.get(model) {
            Some((supported, default)) => chosen
                .filter(|chosen| supported.iter().any(|effort| effort == chosen))
                .unwrap_or(default)
                .to_owned(),
            None => chosen
                .map(str::to_owned)
                .or_else(|| {
                    let default_model = self.default_model.as_ref()?;
                    Some(self.models.get(default_model)?.1.clone())
                })
                .unwrap_or_else(|| "medium".to_owned()),
        }
    }
}

/// Selected service tier (option id `serviceTier`).
fn codex_service_tier(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, "serviceTier")
}

/// Per-thread `config` overrides for `thread/start`, `thread/resume` and
/// `thread/fork`: nothing is written to the user's `config.toml`. A selected
/// context window becomes `model_context_window`, which Codex clamps to the
/// model's largest window and compacts against.
fn thread_config(selections: &[OptionSelection]) -> Option<Value> {
    let window = crate::context_window::selected(&[], selections)?;
    Some(json!({ "model_context_window": window }))
}

/// How soon after a turn ends the list is read, so a command ending with it
/// is not listed.
const BACKGROUND_TERMINAL_TURN_END_READ: std::time::Duration = std::time::Duration::from_secs(1);

/// How often a thread with a running background terminal reads the list again.
const BACKGROUND_TERMINAL_READ_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

enum PendingRequest {
    /// `thread/revert` for [`SessionCommand::Rewind`]; the checkpoint is the
    /// native id of the turn reverted with everything after it.
    Revert {
        checkpoint_id: String,
    },
    TurnStart {
        permission: String,
        settings: Option<Value>,
        /// The `cwd` the request carried.
        moved_to: Option<PathBuf>,
    },
    Interrupt,
    Steer {
        request_id: String,
        text: String,
    },
    SubagentMetadata {
        parent_id: String,
    },
    GuardianApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    InjectItems,
    Compact,
    /// `thread/goal/set` or `thread/goal/clear`; the result arrives again as
    /// `thread/goal/updated` / `thread/goal/cleared`.
    GoalChange,
    /// `thread/goal/get` at startup: the snapshot a resumed thread pushes may
    /// arrive while startup still waits for its own responses.
    GoalSnapshot,
    /// [`SessionCommand::ReadAccountUsage`]: the result goes to its asker.
    AccountUsage(Sender<Result<Value, String>>),
    BackgroundTerminalList,
    /// `thread/backgroundTerminals/terminate` or `clean`; either way the list
    /// is read again.
    BackgroundTerminalStop {
        method: &'static str,
    },
}

struct PendingApproval {
    rpc_id: Value,
    replies: HashMap<String, Value>,
    cancel_reply: Value,
    cancel_interrupt: Option<Value>,
    /// The command or file change the approval would let run.
    item_id: Option<String>,
}

struct PendingElicitation {
    rpc_id: Value,
    questions: Vec<UserInputQuestion>,
    fields: Vec<ElicitField>,
}

struct ElicitField {
    key: String,
    kind: ElicitFieldKind,
    /// Listed in the schema's `required`.
    required: bool,
    /// Display label → wire value for titled enums.
    values: HashMap<String, String>,
}

#[derive(Clone, Copy)]
enum ElicitFieldKind {
    Text,
    Number,
    Integer,
    Boolean,
    Enum,
    MultiEnum,
    UrlAck,
}

struct Actor {
    clock: crate::Clock,
    child: ProcessTree<Child>,
    stderr_tail: StderrTail,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    thread_id: String,
    cwd: PathBuf,
    /// The model turns run on, for `collaborationMode.settings.model`.
    model: String,
    /// The model a session without a chosen one runs on.
    default_model: String,
    /// Session reasoning effort (`reasoningEffort` selection), if any.
    effort: Option<String>,
    /// The effort Codex's configuration sets, if any: the thread's
    /// `reasoningEffort` as `thread/start`, `resume` or `fork` reported it.
    config_effort: Option<String>,
    model_efforts: ModelEfforts,
    /// Session service tier (`serviceTier` selection), if any.
    service_tier: Option<String>,
    /// What the thread started with that only a new thread changes.
    thread_config: Option<Value>,
    requested_permission: String,
    effective_permission: String,
    permission_descriptor: OptionDescriptor,
    next_id: i64,
    pending_requests: HashMap<i64, PendingRequest>,
    approvals: HashMap<String, PendingApproval>,
    guardian_denials: HashMap<String, Value>,
    /// Pending `item/tool/requestUserInput` requests: canonical request_id → the
    /// server-to-client JSON-RPC id we must reply to.
    user_inputs: HashMap<String, Value>,
    /// Pending `mcpServer/elicitation/request`s: canonical request_id → the
    /// JSON-RPC id and field typing needed to rebuild a typed response.
    elicitations: HashMap<String, PendingElicitation>,
    /// Open `request_user_input_async` questions of the running turn, under
    /// the request id of the latest asking message. The model keeps working;
    /// answers go back as a `turn/steer` carrying Codex's reply envelope, and
    /// the turn's end discards whatever is still unanswered.
    async_questions: Option<(String, Vec<UserInputQuestion>)>,
    items: HashMap<String, ThreadItem>,
    subagents: HashMap<String, CodexSubagent>,
    /// Subagents a completed turn left running, reported as backgrounded.
    backgrounded_subagents: HashSet<String>,
    background_terminals: Vec<crate::BackgroundTask>,
    /// The commands whose background terminals the user stopped.
    stopped_commands: HashSet<String>,
    /// What was written to each running command's stdin since its last
    /// finished line ([`typed_lines`]).
    terminal_input: HashMap<String, String>,
    /// When to read the list again, while any terminal runs.
    background_terminal_read: Option<std::time::Instant>,
    /// Stable parent capsule for each provider-native child thread. Codex 0.150
    /// gives completion activities a fresh item id, so the activity id cannot
    /// be the lifecycle identity after spawn.
    subagent_parent_by_thread: HashMap<String, String>,
    /// An activity item may come with both `item/started` and `item/completed`;
    /// its provider identity keeps it to one canonical transition.
    seen_subagent_activities: HashSet<String>,
    usage_by_turn: HashMap<String, TokenUsage>,
    active_turn: Option<String>,
    retry: Option<crate::TurnRetry>,
    last_turn_options: Option<TurnOptions>,
    interrupted: bool,
    /// Steers the server acknowledged but has not yet consumed, as
    /// `(request_id, text)`. `turn/steer` only enqueues the input; the turn
    /// loop drains it before its next model request and echoes it back as a
    /// `userMessage` item, which is the real acceptance signal.
    pending_steers: Vec<(String, String)>,
    /// The directory [`SessionCommand::MoveTo`] named, sent as `turn/start.cwd`
    /// until Codex takes a turn there: the thread runs there from then on.
    moved_to: Option<PathBuf>,
    /// [`SessionOptions::instructions`], sent as `turn/start.additionalContext`.
    instructions: Option<String>,
    /// A retry's `turn/start` with empty input is on its way: the turn it
    /// starts has no prompt to rewind to.
    continuing: bool,
    /// Turns that continue a failed one rather than answer a prompt.
    continuations: HashSet<String>,
    /// The `summary` of each `configWarning`. Codex says each again to the
    /// thread as a `warning` with the same text (0.169.0), shown once.
    config_warnings: HashSet<String>,
}

#[derive(Debug, Clone)]
struct CodexSubagent {
    agent_type: String,
    description: String,
    model: Option<String>,
    effort: Option<String>,
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let mut extra_args = mcp_args(&opts.mcp_servers);
    extra_args.extend(opts.extra_args.iter().cloned());
    let (child, mut stdin, lines, mut stderr_tail) =
        match spawn_server(opts.binary_path.as_deref(), &extra_args, &opts.launch_env) {
            Ok((child, stdin, lines, tail)) => (opts.processes.hold(child), stdin, lines, tail),
            Err(err) => {
                let _ = ready.send(Err(err)).await;
                return;
            }
        };

    let startup = initialize_and_open_thread(&opts, &mut stdin, &lines).await;
    let OpenedThread {
        thread_id,
        unrevertable,
        model,
        default_model,
        config_effort,
        service_tier: reported_tier,
        model_efforts,
        next_id,
        provider_commands,
        permission_descriptor,
        effective_permission,
        warnings,
        said,
    } = match startup {
        Ok(value) => value,
        Err(err) => {
            let (mut child, status) = exit_status_once_gone(child, &err).await;
            stop_child(&mut child, stdin);
            let err = enrich_startup_error(err, status, &mut stderr_tail);
            let _ = ready.send(Err(err)).await;
            return;
        }
    };

    let mut actor = Actor {
        clock: opts.clock.clone(),
        child,
        stderr_tail,
        stdin,
        lines,
        events,
        thread_id: thread_id.clone(),
        cwd: opts.cwd.clone(),
        model: model.clone(),
        default_model,
        effort: codex_effort(&opts.option_selections),
        config_effort,
        model_efforts,
        service_tier: codex_service_tier(&opts.option_selections),
        thread_config: thread_config(&opts.option_selections),
        requested_permission: requested_permission(&opts.option_selections),
        effective_permission,
        permission_descriptor,
        next_id,
        pending_requests: HashMap::new(),
        approvals: HashMap::new(),
        guardian_denials: HashMap::new(),
        user_inputs: HashMap::new(),
        elicitations: HashMap::new(),
        async_questions: None,
        items: HashMap::new(),
        subagents: HashMap::new(),
        backgrounded_subagents: HashSet::new(),
        background_terminals: Vec::new(),
        stopped_commands: HashSet::new(),
        terminal_input: HashMap::new(),
        background_terminal_read: None,
        subagent_parent_by_thread: HashMap::new(),
        seen_subagent_activities: HashSet::new(),
        usage_by_turn: HashMap::new(),
        active_turn: None,
        retry: None,
        last_turn_options: None,
        interrupted: false,
        pending_steers: Vec::new(),
        moved_to: None,
        instructions: opts.instructions.clone(),
        continuing: false,
        continuations: HashSet::new(),
        config_warnings: HashSet::new(),
    };

    let started = AgentEvent::SessionStarted {
        provider_session_id: thread_id.clone(),
        resume: ResumeCursor(json!({ "thread_id": thread_id })),
        model: Some(model),
    };
    if actor.events.send(started).await.is_err() {
        let _ = ready
            .send(Err(AgentError::Protocol("event channel closed".into())))
            .await;
        stop_child(&mut actor.child, actor.stdin);
        return;
    }
    actor
        .events
        .emit(AgentEvent::RewindAvailability {
            unavailable: unrevertable,
        })
        .await;
    actor.emit_permissions(None).await;
    let (model, effort) = (actor.model.clone(), actor.config_effort.clone());
    actor
        .emit_model_settings(&model, effort.as_deref(), reported_tier.as_deref())
        .await;
    // Replace the composer's cached `/` + `$` menu data even when discovery is
    // empty, so removed prompts/skills do not linger from a prior session.
    actor
        .events
        .emit(AgentEvent::ProviderCommands {
            commands: provider_commands,
        })
        .await;
    for message in warnings {
        actor.events.emit(AgentEvent::Warning { message }).await;
    }
    for line in said {
        actor.handle_line(&line).await;
    }
    let thread = json!({ "threadId": actor.thread_id });
    if let Err(err) = actor.request("thread/goal/get", thread, PendingRequest::GoalSnapshot) {
        log::debug!("Codex thread/goal/get not sent: {err}");
    }
    if ready.send(Ok(())).await.is_err() {
        stop_child(&mut actor.child, actor.stdin);
        return;
    }

    actor::run(actor, &commands).await;
}

impl SessionActor for Actor {
    type TransportItem = ChildOutput;

    fn transport(&self) -> &Receiver<Self::TransportItem> {
        &self.lines
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn clock(&self) -> &crate::Clock {
        &self.clock
    }

    fn command_failure_reason(&self) -> &'static str {
        "protocol write failed"
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options,
                attachments,
            } => {
                self.retry = None;
                self.last_turn_options = options.clone();
                self.interrupted = false;
                let params = self.build_turn_params(&text, options.as_ref(), &attachments);
                self.request(
                    "turn/start",
                    params,
                    PendingRequest::TurnStart {
                        permission: self.requested_permission.clone(),
                        settings: None,
                        moved_to: self.moved_to.clone(),
                    },
                )?;
                self.events
                    .emit(AgentEvent::TurnAccepted { delivery_id })
                    .await;
                Ok(())
            }
            SessionCommand::RetryTurn { turn_id } => {
                if self
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.turn_id == turn_id)
                {
                    self.retry_turn()?;
                }
                Ok(())
            }
            SessionCommand::Interrupt => {
                self.interrupted = true;
                if self.retry.is_some()
                    && !self
                        .pending_requests
                        .values()
                        .any(|request| matches!(request, PendingRequest::TurnStart { .. }))
                {
                    self.clear_retry().await;
                    return Ok(());
                }
                if self.active_turn.is_none()
                    && self
                        .pending_requests
                        .values()
                        .any(|request| matches!(request, PendingRequest::TurnStart { .. }))
                {
                    return Ok(());
                }
                let Some(turn_id) = self.active_turn.clone() else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: "cannot interrupt: no active Codex turn".into(),
                        })
                        .await;
                    return Ok(());
                };
                self.request(
                    "turn/interrupt",
                    json!({"threadId":self.thread_id,"turnId":turn_id}),
                    PendingRequest::Interrupt,
                )
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                if let Some(params) = self.guardian_denials.get(&request_id).cloned() {
                    match &decision {
                        ApprovalDecision::Option(id)
                            if id == "thread/approveGuardianDeniedAction" =>
                        {
                            self.request(
                                "thread/approveGuardianDeniedAction",
                                params,
                                PendingRequest::GuardianApproval {
                                    request_id,
                                    decision,
                                },
                            )?;
                        }
                        ApprovalDecision::Cancel => {
                            self.guardian_denials.remove(&request_id);
                            self.events
                                .emit(AgentEvent::ApprovalResolved {
                                    request_id,
                                    decision,
                                })
                                .await;
                            if let (Some(thread_id), Some(turn_id)) = (
                                params.get("threadId").and_then(Value::as_str),
                                params.pointer("/event/turn_id").and_then(Value::as_str),
                            ) {
                                self.request(
                                    "turn/interrupt",
                                    json!({"threadId":thread_id,"turnId":turn_id}),
                                    PendingRequest::Interrupt,
                                )?;
                            }
                        }
                        _ => return Err("unknown Codex guardian approval option".into()),
                    }
                    return Ok(());
                }
                let Some(pending) = self.approvals.get(&request_id) else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("unknown Codex approval request id: {request_id}"),
                        })
                        .await;
                    return Ok(());
                };
                let result = match &decision {
                    ApprovalDecision::Cancel => pending.cancel_reply.clone(),
                    ApprovalDecision::Feedback(_) => pending
                        .replies
                        .get("decline")
                        .cloned()
                        .unwrap_or_else(|| pending.cancel_reply.clone()),
                    ApprovalDecision::Option(id) => pending
                        .replies
                        .get(id)
                        .cloned()
                        .ok_or_else(|| format!("unknown Codex approval option: {id}"))?,
                };
                let interrupt = if decision == ApprovalDecision::Cancel {
                    pending.cancel_interrupt.clone()
                } else {
                    None
                };
                send_json(
                    &mut self.stdin,
                    &json!({ "id": pending.rpc_id, "result": result }),
                )
                .map_err(|e| e.to_string())?;
                self.approvals.remove(&request_id);
                self.events
                    .emit(AgentEvent::ApprovalResolved {
                        request_id,
                        decision,
                    })
                    .await;
                if let Some(params) = interrupt {
                    self.request("turn/interrupt", params, PendingRequest::Interrupt)?;
                }
                Ok(())
            }
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                if let Some(json_rpc_id) = self.user_inputs.remove(&request_id) {
                    // Native result shape: `{answers: {<qid>: {answers: [<strings>]}}}`
                    // — a single string is wrapped into a 1-element array.
                    let mut wire_answers = serde_json::Map::new();
                    for (qid, value) in &answers {
                        wire_answers
                            .insert(qid.clone(), json!({ "answers": strings(Some(value)) }));
                    }
                    send_json(
                        &mut self.stdin,
                        &json!({ "id": json_rpc_id, "result": { "answers": wire_answers } }),
                    )
                    .map_err(|e| e.to_string())?;
                } else if let Some(pending) = self.elicitations.get_mut(&request_id) {
                    let result = match elicitation_result(&pending.fields, &answers) {
                        Ok(result) => result,
                        Err(unusable) => {
                            // Nothing is sent until the answers fit the schema: a decline
                            // would tell the server the user refused.
                            for question in &mut pending.questions {
                                question.prefill = answers
                                    .get(&question.id)
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                            }
                            let headers: Vec<&str> = pending
                                .questions
                                .iter()
                                .filter(|question| unusable.contains(&question.id))
                                .map(|question| question.header.as_str())
                                .collect();
                            let questions = pending.questions.clone();
                            self.events
                                .emit(AgentEvent::Warning {
                                    message: format!(
                                        "Check these answers and try again: {}",
                                        headers.join(", ")
                                    ),
                                })
                                .await;
                            self.events
                                .emit(AgentEvent::UserInputRequested {
                                    request_id,
                                    questions,
                                    delivery: UserInputDelivery::Blocking,
                                })
                                .await;
                            return Ok(());
                        }
                    };
                    let rpc_id = pending.rpc_id.clone();
                    self.elicitations.remove(&request_id);
                    send_json(&mut self.stdin, &json!({ "id": rpc_id, "result": result }))
                        .map_err(|e| e.to_string())?;
                } else if self
                    .async_questions
                    .as_ref()
                    .is_some_and(|(id, _)| *id == request_id)
                {
                    let (_, questions) = self.async_questions.take().unwrap_or_default();
                    self.steer_async_question_reply(&request_id, &questions, &answers)
                        .await?;
                } else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("unknown Codex user-input request id: {request_id}"),
                        })
                        .await;
                    return Ok(());
                }
                self.events
                    .emit(AgentEvent::UserInputResolved {
                        request_id,
                        answers,
                    })
                    .await;
                Ok(())
            }
            // `turn/start` carries the model and effort on every turn, and the
            // service tier "for this turn and subsequent turns" (0.167.0); the
            // context window is thread config, set only when the thread starts.
            SessionCommand::ApplySettings {
                model,
                option_selections,
            } => {
                let applied = thread_config(&option_selections) == self.thread_config;
                if applied {
                    self.model = model.unwrap_or_else(|| self.default_model.clone());
                    self.effort = codex_effort(&option_selections);
                    self.service_tier = codex_service_tier(&option_selections);
                }
                self.events
                    .emit(AgentEvent::SettingsApplied { applied })
                    .await;
                Ok(())
            }
            SessionCommand::StopBackgroundTask { task_id } => {
                let stopped = self
                    .background_terminals
                    .iter()
                    .filter(|task| task_id.as_ref().is_none_or(|id| *id == task.id))
                    .filter_map(|task| task.item_id.clone());
                self.stopped_commands.extend(stopped);
                let thread_id = self.thread_id.clone();
                let (method, params) = match task_id {
                    Some(process_id) => (
                        "thread/backgroundTerminals/terminate",
                        json!({ "threadId": thread_id, "processId": process_id }),
                    ),
                    None => (
                        "thread/backgroundTerminals/clean",
                        json!({ "threadId": thread_id }),
                    ),
                };
                self.request(
                    method,
                    params,
                    PendingRequest::BackgroundTerminalStop { method },
                )
            }
            SessionCommand::SetOption { id, value } => {
                if id == "permissions" {
                    let value = value.as_str().ok_or("Codex permissions must be a string")?;
                    permission_params(value, true)?;
                    self.requested_permission = value.into();
                } else if id == crate::EFFORT_OPTION_ID {
                    self.effort = value.as_str().map(str::to_owned);
                } else if id == "serviceTier" {
                    self.service_tier = value.as_str().map(str::to_owned);
                }
                Ok(())
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                // Native same-turn steering: `turn/steer` injects the message
                // into the ALREADY-RUNNING turn (the model picks it up at its
                // next input checkpoint) and resolves with the *same* turnId —
                // no new turn is started, so there is no extra `turn/started`
                // and our turn accounting stays intact.
                //
                // `expectedTurnId` is a required precondition: the app-server
                // rejects the request when it does not match the active turn,
                // which is exactly the race we want to lose loudly rather than
                // silently start a second turn.
                let Some(turn_id) = self.active_turn.clone() else {
                    self.events
                        .emit(AgentEvent::SteerRejected { request_id })
                        .await;
                    return Ok(());
                };
                let thread_id = self.thread_id.clone();
                self.request(
                    "turn/steer",
                    json!({
                        "threadId": thread_id,
                        "expectedTurnId": turn_id,
                        "input": user_input(&text, &attachments),
                    }),
                    PendingRequest::Steer {
                        request_id,
                        text: requests::input_text(&text, &attachments),
                    },
                )
            }
            SessionCommand::Rewind {
                checkpoint_id,
                mode: RewindMode::Conversation,
            } => {
                // The server shuts the thread's runtime down first, which
                // interrupts a running turn, and reloads it under the same id.
                let params = json!({
                    "threadId": self.thread_id,
                    "beforeTurnId": checkpoint_id,
                });
                self.request(
                    "thread/revert",
                    params,
                    PendingRequest::Revert { checkpoint_id },
                )
            }
            command @ (SessionCommand::Rewind { .. }
            | SessionCommand::PreviewRewind { .. }
            | SessionCommand::UndoRewind { .. }) => {
                if let Some(event) = command
                    .rewind_refusal(RewindFailure::new(RewindFailureReason::Unsupported, None))
                {
                    self.events.emit(event).await;
                }
                Ok(())
            }
            // Progress arrives as the `contextCompaction` item, mid-turn or on its own.
            SessionCommand::Compact => self.request(
                "thread/compact/start",
                json!({ "threadId": self.thread_id }),
                PendingRequest::Compact,
            ),
            SessionCommand::SetGoal { objective, status } => {
                let mut params = json!({ "threadId": self.thread_id, "origin": "user" });
                if let Some(objective) = objective {
                    params["objective"] = json!(objective);
                }
                if let Some(status) = status {
                    params["status"] = json!(goal_status_wire(status));
                }
                self.request("thread/goal/set", params, PendingRequest::GoalChange)
            }
            SessionCommand::ClearGoal => self.request(
                "thread/goal/clear",
                json!({ "threadId": self.thread_id, "origin": "user" }),
                PendingRequest::GoalChange,
            ),
            SessionCommand::ReadAccountUsage { reply } => self.request(
                "account/rateLimits/read",
                json!({}),
                PendingRequest::AccountUsage(reply),
            ),
            SessionCommand::MoveToBackground { .. } | SessionCommand::RespondWorktree { .. } => {
                Ok(())
            }
            SessionCommand::MoveTo { cwd } => {
                self.cwd = cwd.clone();
                self.moved_to = Some(cwd);
                Ok(())
            }
            SessionCommand::Shutdown => Ok(()),
        }
    }

    async fn handle_transport(
        &mut self,
        item: Result<ChildOutput, smol::channel::RecvError>,
    ) -> TransportOutcome {
        match item {
            Ok(ChildOutput::Line(line)) => {
                self.handle_line(&line).await;
                TransportOutcome::Continue
            }
            Ok(ChildOutput::Eof) | Err(_) => {
                let status = self.child.try_wait().ok().flatten();
                TransportOutcome::Closed(match status {
                    Some(status) => format!("codex app-server exited with {status}"),
                    None => "codex app-server closed stdout".into(),
                })
            }
            Ok(ChildOutput::Error(err)) => TransportOutcome::Fatal(err),
        }
    }

    fn wake_at(&self) -> Option<std::time::Instant> {
        self.background_terminal_read
    }

    /// The actor loop calls this once [`Self::wake_at`] has come.
    async fn wake(&mut self) -> Result<(), String> {
        if self.background_terminal_read.take().is_some() {
            return self.read_background_terminals();
        }
        Ok(())
    }

    async fn settle_shutdown(&mut self) {
        self.clear_retry().await;
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        stop_child(&mut self.child, self.stdin);
        reason.map(|reason| describe_child_failure(reason, None, &mut self.stderr_tail))
    }
}

fn mcp_args(registrations: &[crate::McpRegistration]) -> Vec<String> {
    registrations
        .iter()
        .flat_map(|mcp| ["-c".to_string(), mcp.codex_config_override()])
        .collect()
}

/// Append the child's exit status and captured stderr to a process-death
/// message, so the error shows the provider's own words instead of a bare
/// "exited".
fn describe_child_failure(
    base: String,
    status: Option<std::process::ExitStatus>,
    stderr_tail: &mut StderrTail,
) -> String {
    let mut message = base;
    if let Some(status) = status {
        message.push_str(&format!(" ({status})"));
    }
    stderr_tail.append_to(message, "\nstderr:\n")
}

/// Fold the child's death into a startup error. Protocol errors and I/O
/// errors (an EPIPE means the child died mid-handshake) both make the process
/// itself the story; Spawn/Provider errors already carry their own
/// explanation and pass through untouched.
fn enrich_startup_error(
    err: AgentError,
    status: Option<std::process::ExitStatus>,
    stderr_tail: &mut StderrTail,
) -> AgentError {
    match err {
        AgentError::Protocol(message) => {
            AgentError::Protocol(describe_child_failure(message, status, stderr_tail))
        }
        AgentError::Io(io) => AgentError::Protocol(describe_child_failure(
            format!("I/O error talking to codex: {io}"),
            status,
            stderr_tail,
        )),
        other => other,
    }
}

type Spawned = (
    ProcessTree<Child>,
    BufWriter<ChildStdin>,
    Receiver<ChildOutput>,
    StderrTail,
);

fn spawn_server(
    binary_path: Option<&Path>,
    extra_args: &[String],
    launch_env: &LaunchEnv,
) -> Result<Spawned, AgentError> {
    // Absolute path: bare names break once a child sets its own cwd.
    let binary = crate::resolve_binary(binary_path, "codex")?;
    let mut cmd = crate::process::harness_command(&binary);
    cmd.arg("app-server")
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Per-provider environment (Settings → Harnesses): custom variables and the
    // `CODEX_HOME` override (the account-scoped "shadow home", when set).
    for (key, value) in launch_env.pairs(ProviderKind::Codex) {
        cmd.env(key, value);
    }
    if let Some(home) = &launch_env.home {
        // Codex refuses to start against a CODEX_HOME that does not exist.
        if let Err(err) = std::fs::create_dir_all(home) {
            log::warn!("could not create CODEX_HOME {}: {err}", home.display());
        }
    }
    crate::process::refuse_batch_line_breaks(cmd.get_program(), cmd.get_args())?;
    let mut child = ProcessTree::new(
        cmd.spawn()
            .map_err(|err| AgentError::Spawn(err.to_string()))?,
    );
    let stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Spawn("missing child stdin".into()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stderr".into()))?;
    let (rx, reader) = spawn_line_reader(
        stdout,
        "codex-app-server-stdout",
        Some("failed reading codex stdout"),
        false,
    );
    reader.map_err(|err| AgentError::Spawn(err.to_string()))?;
    let stderr_tail = StderrTail::default();
    stderr_tail
        .spawn(stderr, "codex-app-server-stderr", "codex app-server")
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    Ok((child, stdin, rx, stderr_tail))
}

struct OpenedThread {
    thread_id: String,
    /// Why `thread/revert` refuses this thread, when it does: only paginated
    /// threads that are written to disk can be reverted.
    unrevertable: Option<RewindFailureReason>,
    model: String,
    default_model: String,
    config_effort: Option<String>,
    /// `serviceTier` as the answer gave it: the tier in effect, `None` for none.
    service_tier: Option<String>,
    model_efforts: ModelEfforts,
    next_id: i64,
    provider_commands: Vec<ProviderCommand>,
    permission_descriptor: OptionDescriptor,
    effective_permission: String,
    warnings: Vec<String>,
    /// What Codex wrote while the thread opened besides the answers awaited:
    /// the configuration's warnings and deprecations, the thread's own
    /// notifications. The actor reads them first.
    said: Vec<String>,
}

async fn initialize_and_open_thread(
    opts: &SessionOptions,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
) -> Result<OpenedThread, AgentError> {
    initialize(stdin, lines).await?;

    let cwd = opts.cwd.to_string_lossy();
    let permission = requested_permission(&opts.option_selections);
    let (method, mut params) = if let Some(resume) = &opts.resume {
        resume_request(resume, opts.fork.as_ref(), &cwd)?
    } else {
        (
            "thread/start",
            json!({
                "cwd": cwd,
            }),
        )
    };
    params.as_object_mut().expect("thread params").extend(
        permission_params(&permission, false)
            .map_err(AgentError::Protocol)?
            .as_object()
            .expect("permission params")
            .clone(),
    );
    params["developerInstructions"] = json!(PREVIEW_BROWSER_INSTRUCTIONS);
    if let Some(model) = &opts.model {
        params["model"] = json!(model);
    }
    if let Some(tier) = codex_service_tier(&opts.option_selections) {
        params["serviceTier"] = json!(tier);
    }
    if let Some(config) = thread_config(&opts.option_selections) {
        params["config"] = config;
    }
    send_json(
        stdin,
        &json!({ "id": 2, "method": method, "params": params }),
    )?;
    let mut said = Vec::new();
    let result = wait_for_answer(lines, 2, &mut said).await?;
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AgentError::Protocol(format!("{method} response omitted thread.id: {result}"))
        })?
        .to_owned();
    let model = result
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AgentError::Protocol(format!("{method} response omitted model: {result}")))?
        .to_owned();
    let config_effort = result
        .get("reasoningEffort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let service_tier = result
        .get("serviceTier")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let effective_permission = effective_permission(&result);
    let unrevertable = (result.pointer("/thread/ephemeral") == Some(&json!(true))
        || result.pointer("/thread/historyMode") != Some(&json!("paginated")))
    .then_some(RewindFailureReason::Unsupported);

    // The thread is open either way: a failed listing is reported once the
    // session has started. The app-server expands a `$name` mention in plain
    // turn text itself; it has no request to list or run `~/.codex/prompts`
    // files, so those are not offered.
    let mut warnings = Vec::new();
    let mut next_id = 3;
    let profiles = request_permission_profiles(&opts.cwd, stdin, lines, &mut next_id, &mut said)
        .await
        .unwrap_or_else(|err| {
            warnings.push(format!("Codex permissionProfile/list failed: {err}"));
            Vec::new()
        });
    let permission_descriptor = permission_descriptor(&profiles, &effective_permission, &result);
    let provider_commands = request_codex_skills(&opts.cwd, stdin, lines, next_id, &mut said)
        .await
        .unwrap_or_else(|err| {
            warnings.push(format!("Codex skills/list failed: {err}"));
            Vec::new()
        });
    next_id += 1;
    let model_efforts = list_models_raw(stdin, lines, &mut next_id, true, &mut said)
        .await
        .map(|entries| ModelEfforts::from_list(&entries))
        .unwrap_or_else(|err| {
            warnings.push(format!("Codex model/list failed: {err}"));
            ModelEfforts::default()
        });
    // Without a chosen model Codex ran the configured one or its default; a
    // chosen one hides which, and the list's default stands in.
    let default_model = match &opts.model {
        None => model.clone(),
        Some(_) => model_efforts
            .default_model
            .clone()
            .unwrap_or_else(|| model.clone()),
    };
    Ok(OpenedThread {
        thread_id,
        unrevertable,
        model,
        default_model,
        config_effort,
        service_tier,
        model_efforts,
        next_id,
        provider_commands,
        permission_descriptor,
        effective_permission,
        warnings,
        said,
    })
}

fn resume_thread_id(resume: &ResumeCursor) -> Result<&str, AgentError> {
    resume.str_field(&["thread_id"]).ok_or_else(|| {
        AgentError::Protocol("Codex resume cursor is missing string field `thread_id`".into())
    })
}

/// `thread/resume`, or `thread/fork` when the session forks the thread.
fn resume_request(
    resume: &ResumeCursor,
    fork: Option<&ForkPoint>,
    cwd: &str,
) -> Result<(&'static str, Value), AgentError> {
    let mut params = json!({
        "threadId": resume_thread_id(resume)?,
        "cwd": cwd,
    });
    let method = match fork {
        None => "thread/resume",
        Some(ForkPoint::Tail) => "thread/fork",
        Some(ForkPoint::Before { checkpoint_id }) => {
            params["beforeTurnId"] = json!(checkpoint_id);
            "thread/fork"
        }
        Some(ForkPoint::Branch { .. }) => {
            return Err(AgentError::Provider(
                "Codex keeps no branches to go on in".into(),
            ));
        }
    };
    Ok((method, params))
}

async fn request_permission_profiles(
    cwd: &Path,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    next_id: &mut i64,
    said: &mut Vec<String>,
) -> Result<Vec<Value>, AgentError> {
    let mut cursor = Value::Null;
    let mut profiles = Vec::new();
    loop {
        let id = *next_id;
        *next_id += 1;
        send_json(
            stdin,
            &json!({"id":id, "method":"permissionProfile/list", "params":{"cwd":cwd.to_string_lossy(), "cursor":cursor}}),
        )?;
        let result = wait_for_answer(lines, id, said).await?;
        if let Some(entries) = result.get("data").and_then(Value::as_array) {
            profiles.extend(entries.iter().cloned());
        }
        cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.as_str().is_none_or(str::is_empty) {
            break;
        }
    }
    Ok(profiles)
}

/// Query `skills/list` for `cwd` and map the entries into `Skill`-kind
/// [`ProviderCommand`]s. The response shape (verified against codex 0.144.1) is
/// `{data: [{cwd, skills: [{name, description, interface:{…}}]}]}`.
async fn request_codex_skills(
    cwd: &Path,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    id: i64,
    said: &mut Vec<String>,
) -> Result<Vec<ProviderCommand>, AgentError> {
    send_json(
        stdin,
        &json!({ "id": id, "method": "skills/list", "params": { "cwds": [cwd.to_string_lossy()] } }),
    )?;
    let result = wait_for_answer(lines, id, said).await?;
    Ok(parse_codex_skills(&result))
}

/// Flatten a `skills/list` response into `Skill`-kind [`ProviderCommand`]s
/// (deduped by name, empty names dropped).
fn parse_codex_skills(result: &Value) -> Vec<ProviderCommand> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let Some(entries) = result.get("data").and_then(Value::as_array) else {
        return out;
    };
    for entry in entries {
        let Some(skills) = entry.get("skills").and_then(Value::as_array) else {
            continue;
        };
        for skill in skills {
            let Some(name) = skill.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !seen.insert(name.to_owned()) {
                continue;
            }
            let description = skill
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            out.push(ProviderCommand {
                name: name.to_owned(),
                description,
                kind: ProviderCommandKind::Skill,
            });
        }
    }
    out
}

/// Render a JSON-RPC error object with everything the server sent — message,
/// code, and the `data` payload — falling back to the raw JSON when even the
/// message is missing. Losing any of it makes provider failures undiagnosable.
/// Why `thread/revert` refused, from its JSON-RPC error.
fn revert_failure_reason(error: &Value) -> RewindFailureReason {
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    if message.contains("ephemeral") || message.contains("paginated") {
        RewindFailureReason::Unsupported
    } else if message.contains("no rollout") {
        RewindFailureReason::TargetNotFound
    } else {
        RewindFailureReason::Other
    }
}

fn describe_rpc_error(error: &Value) -> String {
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return error.to_string();
    };
    let mut out = message.to_owned();
    if let Some(code) = error.get("code").and_then(Value::as_i64) {
        out.push_str(&format!(" (code {code})"));
    }
    if let Some(data) = error.get("data").filter(|data| !data.is_null()) {
        out.push_str(&format!(": {data}"));
    }
    out
}

async fn wait_for_response(lines: &Receiver<ChildOutput>, id: i64) -> Result<Value, AgentError> {
    wait_for_answer(lines, id, &mut Vec::new()).await
}

/// The answer to request `id`. Every other line Codex writes meanwhile goes to
/// `said`, in order.
async fn wait_for_answer(
    lines: &Receiver<ChildOutput>,
    id: i64,
    said: &mut Vec<String>,
) -> Result<Value, AgentError> {
    loop {
        match lines
            .recv()
            .await
            .map_err(|_| AgentError::Protocol(STDOUT_CLOSED.into()))?
        {
            ChildOutput::Line(line) => {
                let value: Value = serde_json::from_str(&line).map_err(|err| {
                    AgentError::Protocol(format!("invalid JSON from codex: {err}: {line}"))
                })?;
                if value.get("id").and_then(Value::as_i64) != Some(id)
                    || value.get("method").is_some()
                {
                    said.push(line);
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(AgentError::Provider(describe_rpc_error(error)));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AgentError::Protocol(format!("response {id} omitted result")));
            }
            ChildOutput::Eof => {
                return Err(AgentError::Protocol(EXITED.into()));
            }
            ChildOutput::Error(err) => return Err(AgentError::Protocol(err)),
        }
    }
}

fn stop_child(child: &mut ProcessTree<Child>, stdin: BufWriter<ChildStdin>) {
    drop(stdin);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn send_json(stdin: &mut BufWriter<ChildStdin>, value: &Value) -> Result<(), AgentError> {
    write_json(stdin, value, None)
}

const STDOUT_CLOSED: &str = "codex stdout closed during startup";
const EXITED: &str = "codex exited during startup";
const EXITED_BEFORE_ANSWERING: &str = "codex exited before answering";

/// The exit status of a child that `err` shows has closed its pipes: its
/// stdout reached EOF, or writing to its stdin broke. It is exiting, and
/// becomes waitable a moment later; waiting keeps `stop_child`'s kill from
/// standing in for the status it exits with. A child any other failure
/// leaves is still running, and `stop_child` stops it.
async fn exit_status_once_gone(
    mut child: ProcessTree<Child>,
    err: &AgentError,
) -> (ProcessTree<Child>, Option<std::process::ExitStatus>) {
    let gone = match err {
        AgentError::Protocol(message) => {
            [EXITED, STDOUT_CLOSED, EXITED_BEFORE_ANSWERING].contains(&message.as_str())
        }
        AgentError::Io(error) => error.kind() == std::io::ErrorKind::BrokenPipe,
        _ => false,
    };
    if !gone {
        return (child, None);
    }
    crate::process::unblock(move || {
        let status = child.wait().ok();
        (child, status)
    })
    .await
}

impl Actor {
    async fn capture_read_image(&mut self, item: &mut ThreadItem) {
        use base64::Engine as _;
        let ItemContent::ImageRead { path, image } = &mut item.content else {
            return;
        };
        if let Some(ThreadItem {
            content: ItemContent::ImageRead {
                image: Some(saved), ..
            },
            ..
        }) = self.items.get(&item.id)
        {
            *image = Some(saved.clone());
            return;
        }
        let path = self.cwd.join(path);
        match smol::fs::read(&path).await {
            Ok(bytes) => {
                *image = crate::tool_output::image(
                    Some(match path.extension().and_then(|ext| ext.to_str()) {
                        Some("jpg" | "jpeg") => "image/jpeg",
                        Some("webp") => "image/webp",
                        Some("gif") => "image/gif",
                        _ => "image/png",
                    }),
                    &base64::engine::general_purpose::STANDARD.encode(bytes),
                );
                self.items.insert(item.id.clone(), item.clone());
            }
            Err(error) => log::warn!("could not preserve image read {}: {error}", path.display()),
        }
    }

    async fn handle_auto_review(&mut self, params: &Value) {
        let review = params.get("review").unwrap_or(&Value::Null);
        let status = review
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("inProgress");
        let rationale = review
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(id) = params.get("reviewId").and_then(Value::as_str) else {
            return;
        };
        let summary = auto_review_action_summary(params.get("action").unwrap_or(&Value::Null));
        let mut input = json!({"summary":summary, "targetItemId":params.get("targetItemId")});
        if let Some(risk) = review.get("riskLevel").filter(|risk| !risk.is_null()) {
            input["risk"] = risk.clone();
        }
        let family = ToolFamily::Review {
            action: summary,
            risk: review
                .get("riskLevel")
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        let item_status = match status {
            "inProgress" => ItemStatus::InProgress,
            "approved" => ItemStatus::Completed,
            _ => ItemStatus::Failed,
        };
        let item = ThreadItem {
            id: id.into(),
            // parent_item_id routes items to subagent mirrors; the reviewed action stays in this chat.
            parent_item_id: None,
            content: ItemContent::ToolCall {
                image_reads: Vec::new(),
                name: "Auto-reviewer".into(),
                input,
                output: (!rationale.is_empty()).then(|| rationale.to_owned()),
                status: item_status,
                family: Some(Box::new(family)),
            },
        };
        self.events
            .emit(if item_status == ItemStatus::InProgress {
                AgentEvent::ItemStarted(item)
            } else {
                AgentEvent::ItemCompleted(item)
            })
            .await;
        if status != "denied" {
            return;
        }
        let Some(event) = guardian_event(params) else {
            return;
        };
        let Some(id) = event.get("id").and_then(Value::as_str) else {
            return;
        };
        let request_id = format!("guardian:{id}");
        if self.guardian_denials.contains_key(&request_id) {
            return;
        }
        self.guardian_denials.insert(request_id.clone(), json!({
            "threadId":params.get("threadId").and_then(Value::as_str).unwrap_or(&self.thread_id),
            "event":event,
        }));
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {
                id: request_id,
                parent_item_id: None,
                turn_id: params
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind: ApprovalKind::ToolUse {
                    name: "Codex auto-review".into(),
                    input: params.get("action").cloned().unwrap_or(Value::Null),
                    detail: rationale.into(),
                },
                reason: None,
                blocked_path: None,
                options: vec![ApprovalOption {
                    id: "thread/approveGuardianDeniedAction".into(),
                    label: "Approve this action".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                }],
            }))
            .await;
    }

    /// The effort and service tier `model`'s thread is set to, as Codex
    /// reported them (`reasoningEffort` or `effort`, and `serviceTier`,
    /// 0.167.1): no effort runs the model's default, as ChatGPT.app takes it,
    /// and no tier the standard one.
    async fn emit_model_settings(&mut self, model: &str, effort: Option<&str>, tier: Option<&str>) {
        let effort = self.model_efforts.resolve(model, effort);
        self.events
            .emit(AgentEvent::ProviderOptions {
                descriptors: Vec::new(),
                selections: vec![
                    OptionSelection {
                        id: crate::EFFORT_OPTION_ID.into(),
                        value: json!(effort),
                    },
                    OptionSelection {
                        id: "serviceTier".into(),
                        value: json!(tier.unwrap_or("default")),
                    },
                ],
            })
            .await;
    }

    /// A `thread/settings/updated` the thread's settings changed by: its
    /// model, effort and tier here, its permissions by
    /// [`Self::emit_permissions`].
    async fn emit_thread_settings(&mut self, settings: &Value) {
        let model = settings
            .get("model")
            .and_then(Value::as_str)
            .map_or_else(|| self.model.clone(), str::to_owned);
        self.events
            .emit(AgentEvent::SessionModel {
                model: model.clone(),
            })
            .await;
        self.emit_model_settings(
            &model,
            settings.get("effort").and_then(Value::as_str),
            settings.get("serviceTier").and_then(Value::as_str),
        )
        .await;
    }

    async fn emit_permissions(&mut self, settings: Option<&Value>) {
        if let OptionDescriptor::Select { options, .. } = &mut self.permission_descriptor {
            let existing = options
                .iter_mut()
                .find(|option| option.value == self.effective_permission);
            let inherited = self
                .effective_permission
                .strip_prefix("effective:")
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or(Value::Null);
            match existing {
                Some(option)
                    if settings.is_some()
                        && option.unavailable.as_deref()
                            == Some("Inherited permission configuration") =>
                {
                    *option = current_permission_option(
                        &self.effective_permission,
                        settings.unwrap_or(&inherited),
                    );
                }
                None => options.push(current_permission_option(
                    &self.effective_permission,
                    settings.unwrap_or(&inherited),
                )),
                _ => {}
            }
        }
        self.events
            .emit(AgentEvent::ProviderOptions {
                descriptors: vec![self.permission_descriptor.clone()],
                selections: vec![OptionSelection {
                    id: "permissions".into(),
                    value: json!(self.effective_permission),
                }],
            })
            .await;
    }
    /// Compaction drops the developer message carrying the instructions, and Codex sends
    /// `additionalContext` again only when it changes; put it back as Codex rendered it, mid-turn
    /// included.
    fn restore_instructions(&mut self) {
        let Some(instructions) = &self.instructions else {
            return;
        };
        let params = json!({
            "threadId": self.thread_id,
            "items": [{
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": format!("<{INSTRUCTIONS_CONTEXT_KEY}>{instructions}</{INSTRUCTIONS_CONTEXT_KEY}>"),
                }],
            }],
        });
        if let Err(err) = self.request("thread/inject_items", params, PendingRequest::InjectItems) {
            log::warn!("codex: could not restore instructions after compaction: {err}");
        }
    }

    /// Codex says nothing when a terminal ends, so the list is read when a
    /// turn ends and again while any runs (`background_terminal_read`).
    fn read_background_terminals(&mut self) -> Result<(), String> {
        let thread_id = self.thread_id.clone();
        self.request(
            "thread/backgroundTerminals/list",
            json!({ "threadId": thread_id }),
            PendingRequest::BackgroundTerminalList,
        )
    }

    async fn on_background_terminals(&mut self, result: &Value) {
        let now = crate::now_unix_millis();
        let tasks: Vec<crate::BackgroundTask> = result
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|terminal| {
                let id = terminal.get("processId").and_then(Value::as_str)?;
                let known = self.background_terminals.iter().find(|task| task.id == id);
                Some(crate::BackgroundTask {
                    id: id.to_owned(),
                    name: terminal
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    kind: crate::BackgroundKind::Command,
                    item_id: terminal
                        .get("itemId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    started_at: known.map_or(now, |known| known.started_at),
                    started_reported: false,
                })
            })
            .collect();
        self.background_terminal_read =
            (!tasks.is_empty()).then(|| self.clock.now() + BACKGROUND_TERMINAL_READ_INTERVAL);
        if tasks == self.background_terminals {
            return;
        }
        // Codex returns a command's call once it yields and says nothing of
        // its terminal going on but this list: the call it names went on in
        // the background.
        let backgrounded: Vec<String> = tasks
            .iter()
            .filter_map(|task| task.item_id.clone())
            .filter(|item_id| {
                !self
                    .background_terminals
                    .iter()
                    .any(|known| known.item_id.as_ref() == Some(item_id))
            })
            .collect();
        for item_id in backgrounded {
            self.events
                .emit(AgentEvent::ItemBackgrounded {
                    item_id,
                    parent_item_id: None,
                })
                .await;
        }
        self.background_terminals = tasks.clone();
        let count = tasks.len();
        self.events
            .emit(AgentEvent::BackgroundTaskList { tasks })
            .await;
        self.events
            .emit(AgentEvent::BackgroundTasksChanged { count })
            .await;
    }

    fn request(&mut self, method: &str, params: Value, kind: PendingRequest) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        send_json(
            &mut self.stdin,
            &json!({ "id": id, "method": method, "params": params }),
        )
        .map_err(|e| e.to_string())?;
        self.pending_requests.insert(id, kind);
        Ok(())
    }

    async fn clear_retry(&mut self) {
        if self.retry.take().is_some() {
            self.events
                .emit(AgentEvent::TurnRetry { retry: None })
                .await;
        }
    }

    /// The user's Retry of a turn that failed on an overloaded server: the
    /// thread goes on with empty input, as ChatGPT.app's Retry does.
    fn retry_turn(&mut self) -> Result<(), String> {
        if self
            .pending_requests
            .values()
            .any(|request| matches!(request, PendingRequest::TurnStart { .. }))
        {
            return Ok(());
        }
        let mut params = self.build_turn_params("", self.last_turn_options.as_ref(), &[]);
        params["input"] = json!([]);
        self.continuing = true;
        self.request(
            "turn/start",
            params,
            PendingRequest::TurnStart {
                permission: self.requested_permission.clone(),
                settings: None,
                moved_to: self.moved_to.clone(),
            },
        )
    }

    fn build_turn_params(
        &self,
        text: &str,
        options: Option<&TurnOptions>,
        attachments: &[Attachment],
    ) -> Value {
        let effort = options
            .and_then(|o| o.effort.clone())
            .or_else(|| self.effort.clone());

        let mut params = json!({
            "threadId": self.thread_id,
            "input": user_input(text, attachments),
        });
        let permission = permission_params(&self.requested_permission, true)
            .expect("the thread start and SetOption accept only mappable permissions");
        params
            .as_object_mut()
            .expect("turn params")
            .extend(permission.as_object().expect("permission params").clone());
        if let Some(effort) = &effort {
            params["effort"] = json!(effort);
        }
        if let Some(cwd) = &self.moved_to {
            params["cwd"] = json!(cwd.to_string_lossy());
        }
        if let Some(tier) = &self.service_tier {
            params["serviceTier"] = json!(tier);
        }
        // Codex sends an entry to the model when its value changes, as a `<key>value</key>`
        // developer message beside the thread's developer instructions.
        if let Some(instructions) = &self.instructions {
            params["additionalContext"] = json!({
                INSTRUCTIONS_CONTEXT_KEY: {"kind": "application", "value": instructions},
            });
        }

        let chosen = effort.as_deref().or(self.config_effort.as_deref());
        params["collaborationMode"] = json!({
            "mode": "default",
            "settings": {
                "model": self.model,
                "reasoning_effort": self.model_efforts.resolve(&self.model, chosen),
                "developer_instructions": null,
            }
        });

        log::debug!(
            "codex turn/start: effort={:?} serviceTier={:?}",
            effort,
            self.service_tier
        );
        params
    }

    async fn handle_line(&mut self, line: &str) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!("invalid JSON from codex: {err}: {line}"),
                        fatal: false,
                    })
                    .await;
                return;
            }
        };
        if value.get("method").is_some() && value.get("id").is_some() {
            self.handle_server_request(&value).await;
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            self.handle_notification(method, value.get("params").unwrap_or(&Value::Null))
                .await;
        } else if let Some(id) = value.get("id").and_then(Value::as_i64) {
            let pending = self.pending_requests.remove(&id);
            let reread_background_terminals =
                matches!(pending, Some(PendingRequest::BackgroundTerminalStop { .. }));
            if let Some(PendingRequest::AccountUsage(reply)) = &pending {
                let result = match value.get("error") {
                    Some(error) => Err(describe_rpc_error(error)),
                    None => value
                        .get("result")
                        .cloned()
                        .ok_or_else(|| format!("response {id} omitted result")),
                };
                let _ = reply.try_send(result);
                return;
            }
            if let Some(PendingRequest::Revert { checkpoint_id }) = pending {
                self.finish_revert(checkpoint_id, &value).await;
                return;
            }
            // An experimental API: a build without it simply lists nothing.
            if let Some(PendingRequest::BackgroundTerminalList) = &pending {
                match value.get("result") {
                    Some(result) => self.on_background_terminals(result).await,
                    None => log::debug!("Codex background terminals unavailable: {value}"),
                }
                return;
            }
            if let Some(PendingRequest::GoalSnapshot) = &pending {
                // A build without goals, or with the feature off, rejects the
                // request; that is simply a thread without a goal.
                match value.pointer("/result/goal") {
                    Some(goal) => {
                        self.events
                            .emit(AgentEvent::GoalUpdated {
                                goal: map_goal(goal),
                            })
                            .await
                    }
                    None => log::debug!("Codex goal snapshot unavailable: {value}"),
                }
                return;
            }
            if let Some(PendingRequest::SubagentMetadata { parent_id }) = &pending {
                if let Some(thread) = value.pointer("/result/thread") {
                    self.update_subagent_metadata(parent_id, thread).await;
                } else {
                    log::warn!("Codex child metadata unavailable for {parent_id}: {value}");
                }
                return;
            }
            if let Some(PendingRequest::Steer { request_id, .. }) = &pending
                && (value.get("error").is_some() || value.get("result").is_none())
            {
                // `expectedTurnId` no longer matches: the turn ended before the
                // steer reached the server, so the input was never enqueued.
                self.events
                    .emit(AgentEvent::SteerRejected {
                        request_id: request_id.clone(),
                    })
                    .await;
            } else if let Some(error) = value.get("error") {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!(
                            "Codex request {} failed: {}",
                            pending_name(pending.as_ref()),
                            describe_rpc_error(error)
                        ),
                        fatal: false,
                    })
                    .await;
                self.fail_rejected_turn_start(pending.as_ref()).await;
            } else if value.get("result").is_none() {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!(
                            "Codex request {} returned no result",
                            pending_name(pending.as_ref())
                        ),
                        fatal: false,
                    })
                    .await;
                self.fail_rejected_turn_start(pending.as_ref()).await;
            } else {
                match pending {
                    Some(PendingRequest::TurnStart {
                        permission,
                        settings,
                        moved_to,
                    }) => {
                        if moved_to.is_some() && moved_to == self.moved_to {
                            self.moved_to = None;
                        }
                        if let Some(turn_id) =
                            value.pointer("/result/turn/id").and_then(Value::as_str)
                        {
                            self.active_turn.get_or_insert_with(|| turn_id.to_owned());
                        }
                        self.effective_permission = settings
                            .as_ref()
                            .map(effective_permission)
                            .unwrap_or(permission);
                        self.emit_permissions(settings.as_ref()).await;
                        if let Some(settings) = &settings {
                            self.emit_thread_settings(settings).await;
                        }
                    }
                    Some(PendingRequest::Steer { request_id, text }) => {
                        self.pending_steers.push((request_id, text));
                    }
                    Some(PendingRequest::GuardianApproval {
                        request_id,
                        decision,
                    }) => {
                        self.guardian_denials.remove(&request_id);
                        self.events
                            .emit(AgentEvent::ApprovalResolved {
                                request_id,
                                decision,
                            })
                            .await;
                    }
                    _ => {}
                }
            }
            if reread_background_terminals {
                let _ = self.read_background_terminals();
            }
        }
    }

    /// `thread/revert` answered. The reverted thread's runtime was shut down
    /// and reloaded: a turn it interrupted is over even when its end was not
    /// relayed, and the server cancelled the requests it had open.
    async fn finish_revert(&mut self, checkpoint_id: String, response: &Value) {
        let event = if let Some(error) = response.get("error") {
            AgentEvent::RewindFailed {
                checkpoint_id,
                mode: RewindMode::Conversation,
                failure: RewindFailure::new(
                    revert_failure_reason(error),
                    describe_rpc_error(error),
                ),
            }
        } else {
            if let Some(turn_id) = self.active_turn.take() {
                self.clear_retry().await;
                self.events
                    .emit(AgentEvent::TurnCompleted {
                        turn_id,
                        status: TurnStatus::Interrupted,
                        usage: None,
                    })
                    .await;
            }
            self.interrupted = false;
            self.async_questions = None;
            self.pending_steers.clear();
            self.approvals.clear();
            self.guardian_denials.clear();
            self.user_inputs.clear();
            self.elicitations.clear();
            AgentEvent::RewindCompleted {
                checkpoint_id,
                mode: RewindMode::Conversation,
                prefill: None,
                prefill_attachments: Vec::new(),
                later_history: LaterHistory::Truncated,
                files: None,
                incomplete: None,
                undo_token: None,
            }
        };
        self.events.emit(event).await;
    }

    /// Codex echoes user input as a `userMessage` item only once the turn loop
    /// has drained it into the model context. Match the echo's text against
    /// the acknowledged steers (oldest first) rather than assuming FIFO, since
    /// the turn's own prompt echo can land after a fast steer was sent.
    async fn accept_echoed_steer(&mut self, item: &Value) {
        let echoed = user_message_text(item);
        let Some(position) = self
            .pending_steers
            .iter()
            .position(|(_, text)| *text == echoed)
        else {
            return;
        };
        let (request_id, _) = self.pending_steers.remove(position);
        self.events
            .emit(AgentEvent::SteerAccepted { request_id })
            .await;
    }

    /// A `turn/start` the runtime already accepted was rejected by the server
    /// (JSON-RPC error or missing result): no `turn/started` will follow, so no
    /// `turn/completed` ever arrives and the runtime's turn flag would stay set
    /// forever. Synthesize the terminal event the protocol will never send. The
    /// `active_turn` guard keeps a late duplicate response from failing a turn
    /// that did start.
    async fn fail_rejected_turn_start(&mut self, pending: Option<&PendingRequest>) {
        if !matches!(pending, Some(PendingRequest::TurnStart { .. })) || self.active_turn.is_some()
        {
            return;
        }
        self.clear_retry().await;
        self.events
            .emit(AgentEvent::TurnCompleted {
                turn_id: String::new(),
                status: TurnStatus::Failed,
                usage: None,
            })
            .await;
    }

    /// `serverRequest/resolved`: Codex no longer waits on one of its requests.
    /// One Tcode answered is already gone; one still here was settled by Codex
    /// itself, as when the turn it belongs to ends or is interrupted.
    async fn on_server_request_resolved(&mut self, params: &Value) {
        let Some(id) = params.get("requestId") else {
            return;
        };
        let request_id = request_id_string(id);
        let event = if let Some(approval) = self.approvals.remove(&request_id) {
            self.events
                .emit(AgentEvent::ApprovalResolved {
                    request_id,
                    decision: ApprovalDecision::Cancel,
                })
                .await;
            // Codex started the item when it asked, and on settling the
            // request itself, as a turn's end does, drops the item without
            // completing it (`on_command_execution_request_approval_response`
            // and its file-change twin in `bespoke_event_handling.rs`
            // return on a turn-transition error): it never ran.
            let Some(mut item) = approval
                .item_id
                .and_then(|item_id| self.items.get(&item_id).cloned())
                .filter(|item| item_status(&item.content) == ItemStatus::InProgress)
            else {
                return;
            };
            if let ItemContent::CommandExecution { status, .. }
            | ItemContent::FileChange { status, .. } = &mut item.content
            {
                *status = ItemStatus::Interrupted;
            } else {
                return;
            }
            self.items.insert(item.id.clone(), item.clone());
            AgentEvent::ItemCompleted(item)
        } else if self.user_inputs.remove(&request_id).is_some()
            || self.elicitations.remove(&request_id).is_some()
        {
            AgentEvent::UserInputResolved {
                request_id,
                answers: serde_json::Map::new(),
            }
        } else {
            return;
        };
        self.events.emit(event).await;
    }

    /// `item/commandExecution/terminalInteraction`: what was written to a
    /// running command's stdin. Each line it finishes joins the command's
    /// `input`.
    async fn on_terminal_interaction(&mut self, params: &Value) {
        let (Some(item_id), Some(stdin)) = (
            params.get("itemId").and_then(Value::as_str),
            params.get("stdin").and_then(Value::as_str),
        ) else {
            return;
        };
        let buffer = self.terminal_input.entry(item_id.to_owned()).or_default();
        let lines = typed_lines(buffer, stdin);
        if buffer.is_empty() {
            self.terminal_input.remove(item_id);
        }
        if lines.is_empty() {
            return;
        }
        let Some(item) = self.items.get_mut(item_id) else {
            return;
        };
        let ItemContent::CommandExecution { input, .. } = &mut item.content else {
            return;
        };
        input.extend(lines);
        let item = item.clone();
        self.events.emit(AgentEvent::ItemUpdated(item)).await;
    }

    async fn emit_delta(&self, params: &Value, kind: DeltaKind) {
        if let (Some(item_id), Some(text)) = (
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) {
            self.events
                .emit(AgentEvent::Delta {
                    item_id: item_id.into(),
                    kind,
                    text: text.into(),
                })
                .await;
        }
    }
}

#[cfg(test)]
mod tests;
