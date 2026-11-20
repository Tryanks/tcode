//! Wire-level ACP model/option setup and live `ApplySettings` tests using this
//! `harness = false` binary as the subprocess agent so `libtest` writes nothing
//! to its JSON-RPC stdout.

use std::io::{BufRead as _, BufReader, Write as _};

use agent::{
    ACP_MODE_OPTION_ID, AcpAgent, AcpLaunch, AgentEvent, EFFORT_OPTION_ID, LaunchEnv,
    OptionDescriptor, OptionSelection, ProviderKind, SessionCommand, SessionOptions,
    SessionProcesses, start_session,
};
use serde_json::{Value, json};

const ROLE: &str = "TCODE_TEST_SETTINGS_ACP_AGENT";

fn main() {
    if let Ok(port) = std::env::var(ROLE) {
        run_fake_agent(port.parse::<u16>().unwrap());
        return;
    }
    let tests: &[(&str, fn())] = &[
        (
            "establish_applies_model_options_and_hides_owned_model",
            establish_applies_model_options_and_hides_owned_model,
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
            json!({ "method": method, "params": msg["params"] })
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
                    "agentCapabilities": {}
                }
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

/// Establishing an ACP session sends `session/set_config_option` for the
/// requested model and select/boolean options, `session/set_mode` for a
/// non-config mode, reports `SessionModel`, and hides `acp:model` from
/// `ProviderOptions`.
fn establish_applies_model_options_and_hides_owned_model() {
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
    let ids: Vec<&str> = options.iter().map(descriptor_id).collect();
    assert!(
        !ids.contains(&"acp:model"),
        "Registry owns acp:model so it must not appear in ProviderOptions: {ids:?}"
    );
    assert!(ids.contains(&ACP_MODE_OPTION_ID));
    assert!(ids.contains(&EFFORT_OPTION_ID));
    assert!(ids.contains(&"acp:cfg:web_search"));
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
