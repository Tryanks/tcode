//! Headless end-to-end probe for provider clients.
//!
//! Catalog mode: `probe --list-models <codex|claude|pi|opencode|cursor|grok> [--binary <path>]`.
//! Plugin mode: `probe plugins <provider> [--home <dir>] [--project <dir>] [<op> …]`
//! lists the native plugin catalog after running the optional operation:
//! `install|update <id> <scope> [--accept <sha256>]`, `uninstall|enable|disable <id> <scope>`,
//! `add-marketplace <source>` or `remove-marketplace <name>`. `--home` isolates
//! the provider's native state (`CODEX_HOME` for Codex, `GROK_HOME` for Grok;
//! for Claude Code `CLAUDE_CONFIG_DIR`).
//! Turn mode: `probe <provider> <prompt> [cwd] [acp-command args…] [flags]`.
//! Flags are `--binary <path>`, `--model <id>`, `--option <id>=<value>`, `--effort <value>`,
//! `--resume <cursor-json>`, `--fork`, `--fork-at <checkpoint>`, `--leave-questions` (user-input requests
//! stay unanswered), `--plan-feedback <words>` (the first plan decision is
//! answered with these words instead of approved), `--mcp <name> <url> <token>` (an HTTP MCP server registered
//! as tcode registers its own), `--linger <seconds>` (stay open until no turn has
//! run for that long, to see turns the agent starts itself), `--follow-up
//! <seconds> <prompt>` (send another turn that long after the first completes,
//! running turn or not), `--apply-settings <model> <effort>` (ask the running
//! process to take them just before that follow-up), `--move-to <dir>` (send
//! the session to that directory just before that follow-up), `--stop-background
//! <seconds>` (stop all background work that long after the first turn
//! completes), `--interrupt-after
//! <seconds>`, `--steer <message>`, and
//! `--image <path>`. Only one of the last three may be used.

use std::path::PathBuf;
use std::time::Duration;

use agent::{
    AcpAgent, AcpLaunch, AgentEvent, ApprovalDecision, Attachment, ForkPoint, ItemContent,
    LaunchEnv, McpRegistration, OptionSelection, PluginContext, PluginOp, PluginScope,
    ProviderKind, ResumeCursor, SessionCommand, SessionOptions, TurnOptions, TurnStatus,
    list_models, list_plugins, run_plugin_op, start_session,
};
use base64::Engine as _;

#[derive(Default)]
enum ProbeMode {
    #[default]
    Standard,
    Interrupt(Duration),
    Steer(String),
    Image(Attachment),
    /// The prompt is the objective of a goal rather than a turn.
    Goal,
}

fn usage() -> ! {
    eprintln!(
        "usage: probe <codex|claude|pi|opencode|cursor|grok|acp> <prompt> [cwd] \
         [--option <id>=<value>] [acp-command args…] [flags]"
    );
    eprintln!(
        "       probe --list-models <codex|claude|pi|opencode|cursor|grok> [--binary <path>]"
    );
    eprintln!("       probe plugins <provider> [--home <dir>] [--project <dir>] [<op> …]");
    std::process::exit(2);
}

fn parse_scope(arg: Option<String>) -> PluginScope {
    match arg.as_deref() {
        Some("user") => PluginScope::User,
        Some("project") => PluginScope::Project,
        Some("local") => PluginScope::Local,
        Some("managed") => PluginScope::Managed,
        _ => usage(),
    }
}

fn plugins(mut args: impl Iterator<Item = String>) -> i32 {
    let provider = parse_provider(args.next().as_deref());
    let mut home = None;
    let mut project = None;
    let mut accept = None;
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => home = args.next().map(PathBuf::from),
            "--project" => project = args.next().map(PathBuf::from),
            "--accept" => accept = args.next(),
            _ => positional.push(arg),
        }
    }
    let context = PluginContext {
        binary_path: None,
        launch_env: LaunchEnv {
            env: Vec::new(),
            home: home.clone(),
        },
        cwd: project
            .clone()
            .or(home)
            .unwrap_or_else(|| std::env::current_dir().unwrap()),
        project: project.is_some(),
    };
    let mut positional = positional.into_iter();
    let op = positional.next().map(|verb| {
        let mut id = || positional.next().unwrap_or_else(|| usage());
        match verb.as_str() {
            "install" => PluginOp::Install {
                id: id(),
                scope: parse_scope(positional.next()),
                accept_command: accept.clone(),
            },
            "update" => PluginOp::Update {
                id: id(),
                scope: parse_scope(positional.next()),
                accept_command: accept.clone(),
            },
            "uninstall" => PluginOp::Uninstall {
                id: id(),
                scope: parse_scope(positional.next()),
            },
            "enable" | "disable" => PluginOp::SetEnabled {
                id: id(),
                scope: parse_scope(positional.next()),
                enabled: verb == "enable",
            },
            "add-marketplace" => PluginOp::AddMarketplace { source: id() },
            "remove-marketplace" => PluginOp::RemoveMarketplace { name: id() },
            _ => usage(),
        }
    });
    smol::block_on(async move {
        if let Some(op) = op {
            eprintln!("probe: {op:?} in {}", context.cwd.display());
            match run_plugin_op(provider, &context, &op).await {
                Ok(outcome) => println!("outcome: {outcome:#?}"),
                Err(error) => println!("outcome: error: {error}"),
            }
        }
        match list_plugins(provider, &context).await {
            Ok(listing) => {
                let listing = serde_json::json!({
                    "marketplaces": listing.marketplaces,
                    "marketplace_actions": listing.marketplace_actions,
                    "entries": listing.entries,
                });
                println!("{}", serde_json::to_string_pretty(&listing).unwrap());
                0
            }
            Err(error) => {
                eprintln!("list_plugins failed: {error}");
                1
            }
        }
    })
}

fn parse_provider(arg: Option<&str>) -> ProviderKind {
    match arg {
        Some("codex") => ProviderKind::Codex,
        Some("claude") => ProviderKind::ClaudeCode,
        Some("pi") => ProviderKind::Pi,
        Some("opencode") => ProviderKind::OpenCode,
        Some("cursor") => ProviderKind::Cursor,
        Some("grok") => ProviderKind::Grok,
        Some("acp") => ProviderKind::Acp,
        _ => usage(),
    }
}

fn set_mode(mode: &mut ProbeMode, replacement: ProbeMode) {
    if !matches!(mode, ProbeMode::Standard) {
        eprintln!("use only one of --interrupt-after, --steer, --image and --goal");
        std::process::exit(2);
    }
    *mode = replacement;
}

fn image_attachment(path: PathBuf) -> Attachment {
    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
        eprintln!("failed to read {}: {error}", path.display());
        std::process::exit(2);
    });
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let media_type = match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "png" => "image/png",
        other => {
            eprintln!("probe: unknown image extension {other:?}; use png, jpg, webp or gif");
            std::process::exit(2);
        }
    };
    eprintln!(
        "probe: image={} ({} bytes, {media_type})",
        path.display(),
        bytes.len()
    );
    Attachment {
        media_type: media_type.into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        source_path: None,
    }
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("plugins") {
        std::process::exit(plugins(args.into_iter().skip(1)));
    }
    if args.first().map(String::as_str) == Some("--list-models") {
        let provider = parse_provider(args.get(1).map(String::as_str));
        let binary = match args.get(2).map(String::as_str) {
            Some("--binary") => Some(args.get(3).map(PathBuf::from).unwrap_or_else(|| usage())),
            Some(_) => usage(),
            None => None,
        };
        let exit_code = smol::block_on(async move {
            match list_models(provider, binary, Default::default(), Default::default()).await {
                Ok(models) => {
                    println!("{}", serde_json::to_string_pretty(&models).unwrap());
                    0
                }
                Err(error) => {
                    eprintln!("list_models failed: {error}");
                    1
                }
            }
        });
        std::process::exit(exit_code);
    }

    let mut binary = None;
    let mut model = None;
    let mut effort = None;
    let mut option_selections = Vec::new();
    let mut resume = None;
    let mut fork = None;
    let mut leave_questions = false;
    let mut plan_feedback = None;
    let mut mcp_servers = Vec::new();
    let mut continuation = Continuation::default();
    let mut probe_mode = ProbeMode::Standard;
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => binary = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage())),
            "--model" => model = Some(args.next().unwrap_or_else(|| usage())),
            "--effort" => effort = Some(args.next().unwrap_or_else(|| usage())),
            "--option" => {
                let option = args.next().unwrap_or_else(|| usage());
                let (id, value) = option.split_once('=').unwrap_or_else(|| usage());
                option_selections.push(OptionSelection {
                    id: id.into(),
                    value: serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value)),
                });
            }
            "--resume" => {
                let cursor = args.next().unwrap_or_else(|| usage());
                resume = Some(ResumeCursor(serde_json::from_str(&cursor).unwrap_or_else(
                    |error| {
                        eprintln!("--resume takes the JSON of a resume cursor: {error}");
                        std::process::exit(2);
                    },
                )));
            }
            "--fork" => fork = Some(ForkPoint::Tail),
            "--fork-at" => {
                fork = Some(ForkPoint::Before {
                    checkpoint_id: args.next().unwrap_or_else(|| usage()),
                });
            }
            "--goal" => set_mode(&mut probe_mode, ProbeMode::Goal),
            "--leave-questions" => leave_questions = true,
            "--plan-feedback" => plan_feedback = args.next(),
            "--stop-background" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                continuation.stop_background =
                    Some(Duration::from_secs(seconds.unwrap_or_else(|| usage())));
            }
            "--move-to" => {
                continuation.move_to =
                    Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage()));
            }
            "--apply-settings" => {
                let model = args.next().unwrap_or_else(|| usage());
                let effort = args.next().unwrap_or_else(|| usage());
                continuation.apply = Some((model, effort));
            }
            "--linger" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                continuation.linger = Some(Duration::from_secs(seconds.unwrap_or_else(|| usage())));
            }
            "--follow-up" => {
                let seconds = args.next().and_then(|value| value.parse().ok());
                let seconds = seconds.unwrap_or_else(|| usage());
                let prompt = args.next().unwrap_or_else(|| usage());
                continuation.follow_up = Some((Duration::from_secs(seconds), prompt));
            }
            "--mcp" => {
                let mut next = || args.next().unwrap_or_else(|| usage());
                mcp_servers.push(McpRegistration {
                    name: next(),
                    url: next(),
                    bearer_token: next(),
                });
            }
            "--interrupt-after" => {
                let seconds = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("--interrupt-after requires a number of seconds");
                        std::process::exit(2);
                    });
                set_mode(
                    &mut probe_mode,
                    ProbeMode::Interrupt(Duration::from_secs(seconds)),
                );
            }
            "--steer" => {
                let message = args.next().unwrap_or_else(|| usage());
                set_mode(&mut probe_mode, ProbeMode::Steer(message));
            }
            "--image" => {
                let path = args.next().map(PathBuf::from).unwrap_or_else(|| usage());
                set_mode(&mut probe_mode, ProbeMode::Image(image_attachment(path)));
            }
            _ => positional.push(arg),
        }
    }

    let mut positional = positional.into_iter();
    let provider = parse_provider(positional.next().as_deref());
    let prompt = positional.next().unwrap_or_else(|| usage());
    let cwd = positional.next().map(PathBuf::from).unwrap_or_else(|| {
        if matches!(probe_mode, ProbeMode::Steer(_) | ProbeMode::Image(_)) {
            std::env::temp_dir()
        } else {
            std::env::current_dir().unwrap()
        }
    });
    let remaining: Vec<String> = positional.collect();
    let acp = if provider == ProviderKind::Acp {
        let mut launch = remaining.into_iter();
        let command = launch.next().unwrap_or_else(|| {
            eprintln!("the acp provider requires a command and optional arguments");
            usage()
        });
        Some(AcpAgent {
            id: "probe".into(),
            name: command.clone(),
            launch: AcpLaunch::Custom {
                command,
                args: launch.collect(),
                env: Vec::new(),
            },
        })
    } else {
        if !remaining.is_empty() {
            usage();
        }
        None
    };
    let exit_code = smol::block_on(run_probe(
        provider,
        prompt,
        cwd,
        option_selections,
        effort,
        probe_mode,
        acp,
        mcp_servers,
        continuation,
        Resumption {
            resume,
            fork,
            leave_questions,
            plan_feedback,
        },
        binary,
        model,
    ));
    std::process::exit(exit_code);
}

struct Resumption {
    resume: Option<ResumeCursor>,
    fork: Option<ForkPoint>,
    leave_questions: bool,
    plan_feedback: Option<String>,
}

#[derive(Default)]
struct Continuation {
    linger: Option<Duration>,
    follow_up: Option<(Duration, String)>,
    apply: Option<(String, String)>,
    move_to: Option<PathBuf>,
    stop_background: Option<Duration>,
}

#[allow(clippy::too_many_arguments)]
async fn run_probe(
    provider: ProviderKind,
    prompt: String,
    cwd: PathBuf,
    mut option_selections: Vec<OptionSelection>,
    effort: Option<String>,
    probe_mode: ProbeMode,
    acp: Option<AcpAgent>,
    mcp_servers: Vec<McpRegistration>,
    continuation: Continuation,
    mut resumption: Resumption,
    binary_path: Option<PathBuf>,
    model: Option<String>,
) -> i32 {
    option_selections.extend(effort.iter().map(|value| OptionSelection {
        id: "reasoningEffort".into(),
        value: serde_json::Value::String(value.clone()),
    }));
    let opts = SessionOptions {
        cwd,
        model,
        resume: resumption.resume,
        fork: resumption.fork,
        binary_path,
        option_selections,
        mcp_servers,
        launch_env: Default::default(),
        extra_args: Vec::new(),
        acp,
        instructions: None,
        processes: Default::default(),
        clock: Default::default(),
    };
    let handle = match start_session(provider, opts).await {
        Ok(handle) => handle,
        Err(error) => {
            if provider == ProviderKind::Acp {
                println!("START FAILED: {error}");
            } else {
                eprintln!("failed to start session: {error}");
            }
            return 1;
        }
    };
    let attachments = match &probe_mode {
        ProbeMode::Image(attachment) => vec![attachment.clone()],
        _ => Vec::new(),
    };
    let first = if matches!(probe_mode, ProbeMode::Goal) {
        SessionCommand::SetGoal {
            objective: Some(prompt),
            status: None,
        }
    } else {
        SessionCommand::SendTurn {
            delivery_id: 0,
            text: prompt,
            options: Some(TurnOptions { effort }),
            attachments,
        }
    };
    handle
        .commands
        .send(first)
        .await
        .expect("session command channel closed before first turn");

    if let ProbeMode::Interrupt(delay) = &probe_mode {
        let commands = handle.commands.clone();
        let delay = *delay;
        smol::spawn(async move {
            smol::Timer::after(delay).await;
            eprintln!("--- sending Interrupt ---");
            commands.send(SessionCommand::Interrupt).await.ok();
        })
        .detach();
    }
    // The steer goes in once the first turn is at work on something.
    let mut steer = match &probe_mode {
        ProbeMode::Steer(message) => Some(message.clone()),
        _ => None,
    };
    // The follow-up's delay ends here; what it sends goes from the loop.
    let (follow_up_due, due) = smol::channel::unbounded::<()>();

    let mut assistant = String::new();
    let mut turns_started = 0;
    let mut turns_completed = 0;
    let mut first_status = None;
    let mut steer_accepted = false;
    let lingering = continuation.linger;
    let mut follow_up = continuation.follow_up;
    let mut apply = continuation.apply;
    let mut move_to = continuation.move_to;
    let mut stop_background = continuation.stop_background;
    // From scheduling the follow-up until it is sent.
    let mut awaiting_follow_up = None::<(Option<PathBuf>, String)>;
    let mut applying = false;
    let mut running = false;
    let mut closing = false;
    loop {
        enum Input {
            Event(Option<Box<AgentEvent>>),
            Due,
            Quiet,
        }
        let quiet = lingering.filter(|_| {
            turns_completed > 0 && !running && !closing && awaiting_follow_up.is_none()
        });
        let input = smol::future::or(
            async { Input::Event(handle.events.recv().await.ok().map(Box::new)) },
            smol::future::or(
                async {
                    match due.recv().await {
                        Ok(()) => Input::Due,
                        Err(_) => std::future::pending().await,
                    }
                },
                async {
                    match quiet {
                        Some(quiet) => {
                            smol::Timer::after(quiet).await;
                            Input::Quiet
                        }
                        None => std::future::pending().await,
                    }
                },
            ),
        )
        .await;
        let event = match input {
            Input::Event(event) => event.map(|event| *event),
            Input::Quiet => {
                closing = true;
                handle.commands.send(SessionCommand::Shutdown).await.ok();
                continue;
            }
            Input::Due => {
                if let Some((model, effort)) = apply.take() {
                    eprintln!("probe: APPLY -> {model} {effort}");
                    applying = true;
                    handle
                        .commands
                        .send(SessionCommand::ApplySettings {
                            model: Some(model),
                            option_selections: vec![OptionSelection {
                                id: "reasoningEffort".into(),
                                value: serde_json::json!(effort),
                            }],
                        })
                        .await
                        .ok();
                } else if let Some((cwd, text)) = awaiting_follow_up.take() {
                    send_follow_up(&handle.commands, cwd, text).await;
                }
                continue;
            }
        };
        let Some(event) = event else { break };
        match &event {
            AgentEvent::TurnStarted { .. } => running = true,
            AgentEvent::TurnCompleted { .. } => running = false,
            _ => {}
        }

        if provider == ProviderKind::Acp {
            match &event {
                AgentEvent::Delta { kind, text, .. } => println!("DELTA {kind:?}: {text:?}"),
                AgentEvent::ApprovalRequested(request) => {
                    println!("APPROVAL {:?} options={:?}", request.kind, request.options);
                }
                AgentEvent::TurnCompleted { status, usage, .. } => {
                    println!("TURN {status:?} usage={usage:?}");
                }
                other => println!("{other:?}"),
            }
        } else if !matches!(
            (&probe_mode, &event),
            (ProbeMode::Interrupt(_), AgentEvent::Delta { .. })
        ) && !matches!(probe_mode, ProbeMode::Image(_) | ProbeMode::Steer(_))
        {
            println!("{}", serde_json::to_string(&event).unwrap());
        }
        match &event {
            AgentEvent::ApprovalRequested(request) => {
                let feedback = matches!(request.kind, agent::ApprovalKind::Plan { .. })
                    .then(|| resumption.plan_feedback.take())
                    .flatten();
                handle
                    .commands
                    .send(SessionCommand::RespondApproval {
                        request_id: request.id.clone(),
                        decision: feedback.map(ApprovalDecision::Feedback).unwrap_or_else(|| {
                            request
                                .options
                                .iter()
                                .find(|option| option.kind == agent::ApprovalOptionKind::AllowOnce)
                                .map(|option| ApprovalDecision::Option(option.id.clone()))
                                .unwrap_or(ApprovalDecision::Cancel)
                        }),
                    })
                    .await
                    .ok();
            }
            // Worktrees are the host's to make; the probe has none to give.
            AgentEvent::WorktreeRequested { request_id, .. } => {
                handle
                    .commands
                    .send(SessionCommand::RespondWorktree {
                        request_id: request_id.clone(),
                        answer: agent::WorktreeAnswer::Refused {
                            error: "the probe makes no worktrees".into(),
                        },
                    })
                    .await
                    .ok();
            }
            AgentEvent::UserInputRequested { questions, .. } if resumption.leave_questions => {
                eprintln!("probe: leaving {} question(s) unanswered", questions.len());
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } => {
                let answers = questions
                    .iter()
                    .map(|question| {
                        let answer = question
                            .options
                            .first()
                            .map(|option| option.label.clone())
                            .unwrap_or_default();
                        eprintln!(
                            "probe: user-input {:?} header={:?} options={:?} -> answering {:?}",
                            question.question,
                            question.header,
                            question
                                .options
                                .iter()
                                .map(|option| &option.label)
                                .collect::<Vec<_>>(),
                            answer
                        );
                        (question.id.clone(), serde_json::Value::String(answer))
                    })
                    .collect();
                handle
                    .commands
                    .send(SessionCommand::RespondUserInput {
                        request_id: request_id.clone(),
                        answers,
                    })
                    .await
                    .ok();
            }
            AgentEvent::ItemCompleted(item) => {
                if let ItemContent::AssistantMessage { text, .. } = &item.content {
                    assistant.push_str(text);
                    assistant.push('\n');
                }
            }
            AgentEvent::ProviderCommands { commands }
                if matches!(probe_mode, ProbeMode::Image(_)) =>
            {
                eprintln!("probe: provider reported {} command(s)", commands.len());
            }
            AgentEvent::Warning { message } if matches!(probe_mode, ProbeMode::Steer(_)) => {
                eprintln!("probe: WARNING: {message}");
            }
            AgentEvent::Error { message, fatal }
                if matches!(probe_mode, ProbeMode::Image(_) | ProbeMode::Steer(_)) =>
            {
                eprintln!("probe: provider error (fatal={fatal}): {message}");
                if *fatal {
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
            }
            AgentEvent::ItemStarted(item)
                if turns_started > 0
                    && turns_completed == 0
                    && !matches!(item.content, ItemContent::UserMessage { .. }) =>
            {
                if let Some(message) = steer.take() {
                    eprintln!("probe: STEERING (mid-turn) -> {message:?}");
                    handle
                        .commands
                        .send(SessionCommand::Steer {
                            request_id: "probe-steer-1".into(),
                            text: message,
                            attachments: Vec::new(),
                        })
                        .await
                        .ok();
                }
            }
            AgentEvent::SettingsApplied { applied } if applying => {
                applying = false;
                eprintln!("probe: settings applied: {applied}");
                if let Some((cwd, text)) = awaiting_follow_up.take() {
                    send_follow_up(&handle.commands, cwd, text).await;
                }
            }
            AgentEvent::TurnStarted { turn_id } => {
                turns_started += 1;
                if matches!(probe_mode, ProbeMode::Steer(_)) {
                    eprintln!("probe: TurnStarted {turn_id} (#{turns_started})");
                }
            }
            AgentEvent::SteerAccepted { request_id }
                if request_id == "probe-steer-1" && turns_completed == 0 =>
            {
                steer_accepted = true;
                eprintln!("probe: SteerAccepted {request_id} before TurnCompleted");
            }
            AgentEvent::TurnCompleted {
                status, turn_id, ..
            } => {
                turns_completed += 1;
                first_status.get_or_insert(*status);
                if matches!(probe_mode, ProbeMode::Steer(_)) {
                    eprintln!(
                        "probe: TurnCompleted {turn_id} status={status:?} (#{turns_completed})"
                    );
                }
                if let Some(delay) = stop_background.take() {
                    let commands = handle.commands.clone();
                    smol::spawn(async move {
                        smol::Timer::after(delay).await;
                        eprintln!("probe: STOP BACKGROUND");
                        commands
                            .send(SessionCommand::StopBackgroundTask { task_id: None })
                            .await
                            .ok();
                    })
                    .detach();
                }
                if let Some((delay, text)) = follow_up.take() {
                    awaiting_follow_up = Some((move_to.take(), text));
                    let due = follow_up_due.clone();
                    smol::spawn(async move {
                        smol::Timer::after(delay).await;
                        due.send(()).await.ok();
                    })
                    .detach();
                }
                if lingering.is_none() && awaiting_follow_up.is_none() {
                    handle.commands.send(SessionCommand::Shutdown).await.ok();
                }
            }
            AgentEvent::Error { fatal: true, .. } => {
                handle.commands.send(SessionCommand::Shutdown).await.ok();
            }
            AgentEvent::SessionClosed { .. } => break,
            _ => {}
        }
    }
    handle.commands.send(SessionCommand::Shutdown).await.ok();

    match probe_mode {
        ProbeMode::Standard if provider == ProviderKind::Acp => {
            println!("session closed");
            i32::from(first_status != Some(TurnStatus::Completed))
        }
        ProbeMode::Standard | ProbeMode::Goal => i32::from(first_status.is_none()),
        ProbeMode::Interrupt(_) => match first_status {
            Some(TurnStatus::Interrupted) => {
                eprintln!("OK: turn was interrupted");
                0
            }
            other => {
                eprintln!("FAIL: expected Interrupted, got {other:?}");
                1
            }
        },
        ProbeMode::Image(_) => {
            println!("ASSISTANT: {}", assistant.trim());
            i32::from(first_status != Some(TurnStatus::Completed))
        }
        ProbeMode::Steer(message) => {
            let marker = message
                .split_whitespace()
                .next_back()
                .unwrap_or_default()
                .trim_matches(|character: char| !character.is_alphanumeric())
                .to_uppercase();
            let steered = !marker.is_empty() && assistant.to_uppercase().contains(&marker);
            println!("--- transcript ---\n{}", assistant.trim());
            println!("--- steering marker {marker} present: {steered} ---");
            println!("--- steer acceptance before completion observed: {steer_accepted} ---");
            i32::from(first_status != Some(TurnStatus::Completed) || !steered || !steer_accepted)
        }
    }
}

/// The follow-up turn, in the directory `--move-to` names when it does.
async fn send_follow_up(
    commands: &smol::channel::Sender<SessionCommand>,
    cwd: Option<PathBuf>,
    text: String,
) {
    if let Some(cwd) = cwd {
        eprintln!("probe: MOVE TO -> {}", cwd.display());
        commands.send(SessionCommand::MoveTo { cwd }).await.ok();
    }
    eprintln!("probe: FOLLOW-UP -> {text:?}");
    commands
        .send(SessionCommand::SendTurn {
            delivery_id: 1,
            text,
            options: None,
            attachments: Vec::new(),
        })
        .await
        .ok();
}
