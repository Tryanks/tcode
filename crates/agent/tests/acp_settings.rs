//! Wire-level ACP catalog probe, model/option setup and live `ApplySettings`
//! tests using this `harness = false` binary as the subprocess agent so
//! `libtest` writes nothing to its JSON-RPC stdout.

use std::io::{BufRead as _, BufReader, Write as _};

use agent::{
    ACP_MODE_OPTION_ID, AcpAgent, AcpCatalog, AcpLaunch, AgentEvent, EFFORT_OPTION_ID, LaunchEnv,
    OptionDescriptor, OptionRole, OptionSelection, ProviderKind, SessionCommand, SessionOptions,
    SessionProcesses, start_session,
};
use serde_json::{Value, json};

const ROLE: &str = "TCODE_TEST_SETTINGS_ACP_AGENT";
/// Set, the fake agent advertises `sessionCapabilities.close`.
const CLOSES: &str = "TCODE_TEST_SETTINGS_ACP_CLOSES";
/// Set, the fake agent answers `session/new` that it needs signing in.
const SIGNED_OUT: &str = "TCODE_TEST_SETTINGS_ACP_SIGNED_OUT";

fn main() {
    if let Ok(port) = std::env::var(ROLE) {
        run_fake_agent(port.parse::<u16>().unwrap());
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "establish_applies_model_and_options_and_leaves_the_model_to_the_model_picker",
            establish_applies_model_and_options_and_leaves_the_model_to_the_model_picker,
        ),
        (
            "probe_reads_session_new_in_a_directory_of_its_own_and_closes_the_session",
            probe_reads_session_new_in_a_directory_of_its_own_and_closes_the_session,
        ),
        (
            "probe_of_a_signed_out_agent_fails_without_signing_in",
            probe_of_a_signed_out_agent_fails_without_signing_in,
        ),
        (
            "stale_model_at_establish_warns_and_carries_on",
            stale_model_at_establish_warns_and_carries_on,
        ),
        (
            "apply_settings_updates_model_options_and_mode_live",
            apply_settings_updates_model_options_and_mode_live,
        ),
    ];
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ignored_only = args.iter().any(|arg| arg == "--ignored");
    if args.iter().any(|arg| arg == "--list") {
        if !ignored_only {
            for (name, _) in tests {
                println!("{name}: test");
            }
        }
        return;
    }
    if ignored_only {
        return;
    }
    let exact = args.iter().any(|arg| arg == "--exact");
    let filters: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(i, arg)| {
            !arg.starts_with('-')
                && !matches!(
                    i.checked_sub(1)
                        .and_then(|prev| args.get(prev))
                        .map(String::as_str),
                    Some("--format" | "--color" | "--test-threads")
                )
        })
        .map(|(_, arg)| arg.as_str())
        .collect();
    for (name, run) in tests {
        let selected = filters.is_empty()
            || filters.iter().any(|filter| {
                if exact {
                    *name == *filter
                } else {
                    name.contains(filter)
                }
            });
        if selected {
            run();
            println!("test {name} ... ok");
        }
    }
}

fn config_options_json(model: &str, effort: &str, web_search: bool) -> Value {
    json!([
        {
            "id": "selected_model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": model,
            "options": [
                { "value": "model-a", "name": "Model A" },
                { "value": "model-b", "name": "Model B" }
            ]
        },
        {
            "id": "thought_level",
            "name": "Thinking Level",
            "category": "thought_level",
            "type": "select",
            "currentValue": effort,
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "high", "name": "High" }
            ]
        },
        {
            "id": "web_search",
            "name": "Web search",
            "type": "boolean",
            "currentValue": web_search
        }
    ])
}

fn run_fake_agent(port: u16) {
    let mut log = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();

    let mut model = "model-a".to_string();
    let mut effort = "low".to_string();
    let mut web_search = false;

    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let msg: Value = serde_json::from_str(&line).unwrap();
        let method = msg["method"].as_str().unwrap_or_default();
        writeln!(
            log,
            "{}",
            json!({
                "method": method,
                "params": msg["params"],
                // Whether the agent runs in the directory the request names.
                "in_cwd": msg["params"]["cwd"].as_str().is_some_and(|cwd| {
                    std::fs::canonicalize(cwd).ok() == std::env::current_dir().ok()
                }),
            })
        )
        .unwrap();
        log.flush().unwrap();

        let id = msg["id"].clone();
        let reply = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": if std::env::var_os(CLOSES).is_some() {
                        json!({ "sessionCapabilities": { "close": {} } })
                    } else {
                        json!({})
                    },
                    "authMethods": [{ "id": "browser", "name": "Sign in with a browser" }]
                }
            }),
            "session/new" if std::env::var_os(SIGNED_OUT).is_some() => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": "Authentication required" }
            }),
            "session/new" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "sessionId": "s1",
                    "modes": {
                        "currentModeId": "default",
                        "availableModes": [
                            { "id": "default", "name": "Default" },
                            { "id": "auto", "name": "Auto" }
                        ]
                    },
                    "configOptions": config_options_json(&model, &effort, web_search)
                }
            }),
            "session/set_config_option" => {
                let config_id = msg["params"]["configId"].as_str().unwrap_or_default();
                let value = &msg["params"]["value"];
                let error = match config_id {
                    "selected_model" => match value.as_str() {
                        Some(v @ ("model-a" | "model-b")) => {
                            model = v.to_string();
                            None
                        }
                        _ => Some("unknown model"),
                    },
                    "thought_level" => match value.as_str() {
                        Some(v @ ("low" | "high")) => {
                            effort = v.to_string();
                            None
                        }
                        _ => Some("unknown effort"),
                    },
                    "web_search" => match value.as_bool() {
                        Some(v) => {
                            web_search = v;
                            None
                        }
                        None => Some("expected boolean"),
                    },
                    _ => Some("unknown configId"),
                };
                match error {
                    Some(message) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": message }
                    }),
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "configOptions": config_options_json(&model, &effort, web_search)
                        }
                    }),
                }
            }
            "session/set_mode" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            }),
        };
        writeln!(stdout, "{reply}").unwrap();
        stdout.flush().unwrap();
    }
}

fn spawn_session(
    model: Option<&str>,
    option_selections: Vec<OptionSelection>,
) -> (
    agent::SessionHandle,
    SessionProcesses,
    BufReader<std::net::TcpStream>,
) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let processes = SessionProcesses::default();
    let opts = SessionOptions {
        cwd: std::env::temp_dir(),
        model: model.map(str::to_string),
        resume: None,
        fork: None,
        binary_path: None,
        option_selections,
        mcp_servers: Vec::new(),
        launch_env: LaunchEnv::default(),
        extra_args: Vec::new(),
        acp: Some(AcpAgent {
            id: "fake-settings".into(),
            name: "Fake Settings".into(),
            launch: AcpLaunch::Binary {
                command: std::env::current_exe().unwrap(),
                args: Vec::new(),
                env: vec![(ROLE.into(), port.to_string())],
            },
        }),
        instructions: None,
        processes: processes.clone(),
        clock: Default::default(),
        worktrees: Vec::new(),
    };
    let start = smol::spawn(start_session(ProviderKind::Acp, opts));
    let (stream, _) = listener.accept().unwrap();
    let handle = smol::block_on(start).expect("fake ACP agent starts");
    (handle, processes, BufReader::new(stream))
}

fn read_requests(reader: &mut BufReader<std::net::TcpStream>, count: usize) -> Vec<Value> {
    let mut requests = Vec::with_capacity(count);
    let mut line = String::new();
    for _ in 0..count {
        line.clear();
        reader.read_line(&mut line).unwrap();
        requests.push(serde_json::from_str(&line).unwrap());
    }
    requests
}

fn descriptor_id(descriptor: &OptionDescriptor) -> &str {
    match descriptor {
        OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
    }
}

fn descriptor_role(descriptor: &OptionDescriptor) -> OptionRole {
    match descriptor {
        OptionDescriptor::Select { role, .. } | OptionDescriptor::Boolean { role, .. } => *role,
    }
}

/// Establishing an ACP session sends `session/set_config_option` for the
/// requested model and select/boolean options and `session/set_mode` for a
/// non-config mode. The model is the model picker's, so `ProviderOptions`
/// leaves it out; the mode is the agent's permission control.
fn establish_applies_model_and_options_and_leaves_the_model_to_the_model_picker() {
    let (handle, processes, mut wire) = spawn_session(
        Some("model-b"),
        vec![
            OptionSelection {
                id: EFFORT_OPTION_ID.into(),
                value: json!("high"),
            },
            OptionSelection {
                id: "acp:cfg:web_search".into(),
                value: json!(true),
            },
            OptionSelection {
                id: ACP_MODE_OPTION_ID.into(),
                value: json!("auto"),
            },
        ],
    );

    // initialize, session/new, set_config_option(model), set_config_option(effort),
    // set_config_option(web_search), set_mode(auto).
    let requests = read_requests(&mut wire, 6);
    assert_eq!(requests[0]["method"], "initialize");
    assert_eq!(requests[1]["method"], "session/new");
    assert_eq!(requests[2]["method"], "session/set_config_option");
    assert_eq!(requests[2]["params"]["configId"], "selected_model");
    assert_eq!(requests[2]["params"]["value"], "model-b");
    assert_eq!(requests[3]["method"], "session/set_config_option");
    assert_eq!(requests[3]["params"]["configId"], "thought_level");
    assert_eq!(requests[3]["params"]["value"], "high");
    assert_eq!(requests[4]["method"], "session/set_config_option");
    assert_eq!(requests[4]["params"]["configId"], "web_search");
    assert_eq!(requests[4]["params"]["value"], true);
    assert_eq!(requests[5]["method"], "session/set_mode");
    assert_eq!(requests[5]["params"]["modeId"], "auto");

    let mut session_model = None;
    let mut provider_options = None;
    while let Ok(event) = handle.events.try_recv() {
        match event {
            AgentEvent::SessionStarted { model, .. } => session_model = model,
            AgentEvent::ProviderOptions {
                descriptors,
                selections,
            } => provider_options = Some((descriptors, selections)),
            _ => {}
        }
    }
    assert_eq!(session_model.as_deref(), Some("model-b"));
    let (options, selections) = provider_options.expect("ProviderOptions emitted");
    let roles: Vec<(&str, OptionRole)> = options
        .iter()
        .map(|option| (descriptor_id(option), descriptor_role(option)))
        .collect();
    assert_eq!(
        roles,
        [
            (ACP_MODE_OPTION_ID, OptionRole::Permission),
            (EFFORT_OPTION_ID, OptionRole::Effort),
            ("acp:cfg:web_search", OptionRole::Model),
        ]
    );
    assert!(!selections.iter().any(|s| s.id == "acp:model"));
    assert_eq!(
        selections
            .iter()
            .find(|s| s.id == EFFORT_OPTION_ID)
            .map(|s| &s.value),
        Some(&json!("high"))
    );
    assert_eq!(
        selections
            .iter()
            .find(|s| s.id == "acp:cfg:web_search")
            .map(|s| &s.value),
        Some(&json!(true))
    );
    assert_eq!(
        selections
            .iter()
            .find(|s| s.id == ACP_MODE_OPTION_ID)
            .map(|s| &s.value),
        Some(&json!("auto"))
    );

    processes.stop();
    drop(handle);
}

/// Probe the fake agent, launched with `env`: what the probe returned and
/// every request the agent was sent, read once the agent is gone.
fn probe(env: &[&str]) -> (Result<AcpCatalog, agent::AgentError>, Vec<Value>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut env: Vec<(String, String)> = env
        .iter()
        .map(|name| ((*name).to_owned(), "1".to_owned()))
        .collect();
    env.push((ROLE.into(), port.to_string()));
    let agent = AcpAgent {
        id: "fake-settings".into(),
        name: "Fake Settings".into(),
        launch: AcpLaunch::Binary {
            command: std::env::current_exe().unwrap(),
            args: Vec::new(),
            env,
        },
    };
    let processes = SessionProcesses::default();
    let probed = smol::spawn(async move {
        agent::acp::probe_catalog(agent, Vec::new(), LaunchEnv::default(), &processes).await
    });
    let (stream, _) = listener.accept().unwrap();
    let probed = smol::block_on(probed);
    let requests = BufReader::new(stream)
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    (probed, requests)
}

/// An agent's models and options exist before its first message only as
/// its `session/new` answer: the probe asks for one session, with no MCP
/// servers, in a directory made for it and gone afterwards, so no project's
/// configuration runs, and closes the session where the agent can.
fn probe_reads_session_new_in_a_directory_of_its_own_and_closes_the_session() {
    let (probed, requests) = probe(&[CLOSES]);
    let catalog = probed.expect("the probe lists the agent's catalog");

    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialize", "session/new", "session/close"]);
    let new = &requests[1];
    assert_eq!(new["params"]["mcpServers"], json!([]));
    assert_eq!(new["in_cwd"], true, "the agent runs where its session is");
    let cwd = std::path::PathBuf::from(new["params"]["cwd"].as_str().unwrap());
    assert!(!cwd.exists(), "{} outlived the probe", cwd.display());
    assert_eq!(requests[2]["params"]["sessionId"], "s1");

    let models: Vec<(&str, &str, bool)> = catalog
        .models
        .iter()
        .map(|model| {
            (
                model.id.as_str(),
                model.display_name.as_str(),
                model.is_default,
            )
        })
        .collect();
    assert_eq!(
        models,
        [("model-a", "Model A", true), ("model-b", "Model B", false)]
    );
    assert_eq!(catalog.default_model(), Some("model-a"));
    // Each option defaults to where a new session starts.
    let options: Vec<(&str, OptionRole, Value)> = catalog
        .options
        .iter()
        .map(|option| match option {
            OptionDescriptor::Select {
                id,
                role,
                default_value,
                ..
            } => (id.as_str(), *role, json!(default_value)),
            OptionDescriptor::Boolean {
                id,
                role,
                default_value,
                ..
            } => (id.as_str(), *role, json!(default_value)),
        })
        .collect();
    assert_eq!(
        options,
        [
            (ACP_MODE_OPTION_ID, OptionRole::Permission, json!("default")),
            (EFFORT_OPTION_ID, OptionRole::Effort, json!("low")),
            ("acp:cfg:web_search", OptionRole::Model, json!(false)),
        ]
    );
}

/// Signing in is the agent's interactive flow, which a thread the user
/// starts runs; a probe in the background never begins it. An agent that
/// cannot close sessions is not asked to.
fn probe_of_a_signed_out_agent_fails_without_signing_in() {
    let (probed, requests) = probe(&[SIGNED_OUT]);
    let error = probed.expect_err("a signed-out agent lists nothing");
    assert!(
        error.to_string().contains("requires authentication"),
        "{error}"
    );
    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialize", "session/new"]);
}

/// A stale or rejected model at establish warns and carries on instead of
/// failing session startup, and still restores the remaining options.
fn stale_model_at_establish_warns_and_carries_on() {
    let (handle, processes, mut wire) = spawn_session(
        Some("stale-model"),
        vec![OptionSelection {
            id: EFFORT_OPTION_ID.into(),
            value: json!("high"),
        }],
    );

    let requests = read_requests(&mut wire, 4);
    assert_eq!(requests[2]["method"], "session/set_config_option");
    assert_eq!(requests[2]["params"]["configId"], "selected_model");
    assert_eq!(requests[2]["params"]["value"], "stale-model");
    assert_eq!(requests[3]["method"], "session/set_config_option");
    assert_eq!(requests[3]["params"]["configId"], "thought_level");
    assert_eq!(requests[3]["params"]["value"], "high");

    let mut warnings = Vec::new();
    let mut session_model = None;
    while let Ok(event) = handle.events.try_recv() {
        match event {
            AgentEvent::Warning { message } => warnings.push(message),
            AgentEvent::SessionStarted { model, .. } => session_model = model,
            _ => {}
        }
    }
    assert!(
        warnings.iter().any(|w| w.contains("stale-model")),
        "expected a warning naming stale-model, got {warnings:?}"
    );
    assert_eq!(session_model.as_deref(), Some("model-a"));

    processes.stop();
    drop(handle);
}

/// `SessionCommand::ApplySettings` applies live model, config-option and mode
/// changes over `session/set_config_option` / `session/set_mode`, and reports
/// `SettingsApplied { applied: false }` with a warning when rejected.
fn apply_settings_updates_model_options_and_mode_live() {
    let (handle, processes, mut wire) = spawn_session(None, Vec::new());
    let _startup = read_requests(&mut wire, 2);
    while handle.events.try_recv().is_ok() {}

    smol::block_on(async {
        handle
            .commands
            .send(SessionCommand::ApplySettings {
                model: Some("model-b".into()),
                option_selections: vec![
                    OptionSelection {
                        id: EFFORT_OPTION_ID.into(),
                        value: json!("high"),
                    },
                    OptionSelection {
                        id: ACP_MODE_OPTION_ID.into(),
                        value: json!("auto"),
                    },
                ],
            })
            .await
            .unwrap();

        let mut saw_model = None;
        let mut saw_applied = None;
        while saw_applied.is_none() {
            match handle.events.recv().await.unwrap() {
                AgentEvent::SessionModel { model } => saw_model = Some(model),
                AgentEvent::SettingsApplied { applied } => saw_applied = Some(applied),
                _ => {}
            }
        }
        assert_eq!(saw_model.as_deref(), Some("model-b"));
        assert_eq!(saw_applied, Some(true));

        let live = read_requests(&mut wire, 3);
        assert_eq!(live[0]["method"], "session/set_config_option");
        assert_eq!(live[0]["params"]["configId"], "selected_model");
        assert_eq!(live[0]["params"]["value"], "model-b");
        assert_eq!(live[1]["method"], "session/set_config_option");
        assert_eq!(live[1]["params"]["configId"], "thought_level");
        assert_eq!(live[1]["params"]["value"], "high");
        assert_eq!(live[2]["method"], "session/set_mode");
        assert_eq!(live[2]["params"]["modeId"], "auto");

        handle
            .commands
            .send(SessionCommand::ApplySettings {
                model: Some("stale-model".into()),
                option_selections: Vec::new(),
            })
            .await
            .unwrap();

        let mut saw_warning = false;
        let mut rejected = None;
        while rejected.is_none() {
            match handle.events.recv().await.unwrap() {
                AgentEvent::Warning { message } if message.contains("stale-model") => {
                    saw_warning = true;
                }
                AgentEvent::SettingsApplied { applied } => rejected = Some(applied),
                _ => {}
            }
        }
        assert!(saw_warning, "rejected live model must emit a warning");
        assert_eq!(rejected, Some(false));
    });

    processes.stop();
    drop(handle);
}
