//! Wire-level ACP session tests using this `harness = false` binary as the
//! subprocess agent so `libtest` writes nothing to its JSON-RPC stdout.

use std::io::{BufRead as _, Read as _, Write as _};

use agent::{
    AcpAgent, AcpLaunch, AgentEvent, LaunchEnv, McpRegistration, ProviderKind, SessionOptions,
    SessionProcesses, start_session,
};
use serde_json::{Value, json};

const ROLE: &str = "TCODE_TEST_HTTP_MCP_AGENT";

fn main() {
    if let Ok(spec) = std::env::var(ROLE) {
        run_fake_agent(&spec);
        return;
    }
    const TEST_NAME: &str = "session_new_sends_http_mcp_servers_by_capability_or_agent_info";
    let args: Vec<String> = std::env::args().skip(1).collect();
    let ignored_only = args.iter().any(|arg| arg == "--ignored");
    if args.iter().any(|arg| arg == "--list") {
        if !ignored_only {
            println!("{TEST_NAME}: test");
        }
        return;
    }
    if ignored_only {
        return;
    }
    session_new_sends_http_mcp_servers_by_capability_or_agent_info();
    println!("test {TEST_NAME} ... ok");
}

fn run_fake_agent(spec: &str) {
    let (mode, port) = spec.split_once(':').unwrap();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();

    let mut line = String::new();
    stdin.read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(init["method"], "initialize");
    let (caps, agent_info) = match mode {
        "agy" => (
            json!({}),
            json!({ "name": "Antigravity ACP Server", "version": "1.5.13" }),
        ),
        "http" => (
            json!({ "mcpCapabilities": { "http": true } }),
            json!({ "name": "Other ACP Server", "version": "1.0.0" }),
        ),
        _ => (
            json!({}),
            json!({ "name": "Other ACP Server", "version": "1.0.0" }),
        ),
    };
    writeln!(
        stdout,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": init["id"],
            "result": {
                "protocolVersion": 1,
                "agentCapabilities": caps,
                "agentInfo": agent_info,
            }
        })
    )
    .unwrap();
    stdout.flush().unwrap();

    line.clear();
    stdin.read_line(&mut line).unwrap();
    let new_session: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(new_session["method"], "session/new");
    let mut report =
        std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    serde_json::to_writer(&mut report, &new_session["params"]["mcpServers"]).unwrap();
    drop(report);

    writeln!(
        stdout,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": new_session["id"],
            "result": { "sessionId": "s1" }
        })
    )
    .unwrap();
    stdout.flush().unwrap();
    let _ = std::io::copy(&mut stdin, &mut std::io::sink());
}

/// Tcode's MCP servers are loopback HTTP endpoints: `session/new` sends them
/// to an agent whose `initialize` declares `mcpCapabilities.http`, or to
/// Antigravity ACP Server (which omits the capability yet accepts them), and
/// omits them otherwise.
fn session_new_sends_http_mcp_servers_by_capability_or_agent_info() {
    let registrations = vec![
        McpRegistration {
            name: McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: "http://127.0.0.1:5321/mcp".into(),
            bearer_token: "tok".into(),
        },
        McpRegistration {
            name: McpRegistration::SERVER_NAME_CONTROL.into(),
            url: "http://127.0.0.1:5322/mcp".into(),
            bearer_token: "other".into(),
        },
    ];
    let offered = json!([
        {"type": "http", "name": "tcode_preview", "url": "http://127.0.0.1:5321/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer tok"}]},
        {"type": "http", "name": "tcode_control", "url": "http://127.0.0.1:5322/mcp",
            "headers": [{"name": "Authorization", "value": "Bearer other"}]},
    ]);

    for (mode, expected) in [
        ("plain", json!([])),
        ("agy", offered.clone()),
        ("http", offered),
    ] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let processes = SessionProcesses::default();
        let opts = SessionOptions {
            cwd: std::env::temp_dir(),
            model: None,
            resume: None,
            fork: None,
            binary_path: None,
            option_selections: Vec::new(),
            mcp_servers: registrations.clone(),
            launch_env: LaunchEnv::default(),
            extra_args: Vec::new(),
            acp: Some(AcpAgent {
                id: format!("fake-{mode}"),
                name: format!("Fake {mode}"),
                launch: AcpLaunch::Binary {
                    command: std::env::current_exe().unwrap(),
                    args: Vec::new(),
                    env: vec![(ROLE.into(), format!("{mode}:{port}"))],
                },
            }),
            instructions: None,
            processes: processes.clone(),
            clock: Default::default(),
            worktrees: Vec::new(),
        };
        let handle = smol::block_on(start_session(ProviderKind::Acp, opts))
            .unwrap_or_else(|err| panic!("mode {mode}: {err}"));
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = String::new();
        stream.read_to_string(&mut received).unwrap();
        let actual: Value = serde_json::from_str(&received).unwrap();
        assert_eq!(actual, expected, "mode {mode}");
        if mode == "plain" {
            let mut saw_not_attached = false;
            while let Ok(event) = handle.events.try_recv() {
                if matches!(event, AgentEvent::McpServersNotAttached) {
                    saw_not_attached = true;
                }
            }
            assert!(
                saw_not_attached,
                "omitting HTTP MCP servers must emit McpServersNotAttached"
            );
        }
        processes.stop();
        drop(handle);
    }
}
