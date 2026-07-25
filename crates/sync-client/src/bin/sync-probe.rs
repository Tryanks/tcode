#[cfg(target_arch = "wasm32")]
fn main() {
    eprintln!("sync-probe is only available on native targets");
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::error::Error;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use agent::{ApprovalDecision, SessionCommand};
    use futures_util::{SinkExt as _, StreamExt as _};
    use sync_client::{Client, ClientConfig};
    use sync_protocol::{ClientFrame, ClientInfo, HostFrame};
    use tcode_services::settings::SettingsStore;
    use tcode_services::store::SessionStore;
    use tokio_tungstenite::tungstenite::Message;

    const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    enum Command {
        List,
        Watch {
            session_id: String,
        },
        Send {
            session_id: String,
            text: String,
        },
        Approve {
            session_id: String,
            request_id: String,
            decision: ApprovalDecision,
        },
    }

    struct Args {
        url: String,
        token: Option<String>,
        command: Command,
    }

    pub async fn run() -> Result<(), Box<dyn Error>> {
        let args = parse_args()?;
        let token = match args.token {
            Some(token) => token,
            None => default_token()?,
        };
        let (mut socket, _) = tokio_tungstenite::connect_async(&args.url).await?;
        let mut client = Client::new(ClientConfig {
            client: ClientInfo {
                client_id: format!("sync-probe-{}", std::process::id()),
                display_name: "tcode sync probe".into(),
                platform: std::env::consts::OS.into(),
                app_version: env!("CARGO_PKG_VERSION").into(),
            },
            token,
        });
        send_frame(&mut socket, client.connect()).await?;
        handshake(&mut socket, &mut client).await?;

        match args.command {
            Command::List => list(&mut socket, &mut client).await,
            Command::Watch { session_id } => watch(&mut socket, &mut client, session_id).await,
            Command::Send { session_id, text } => {
                send_turn(&mut socket, &mut client, session_id, text).await
            }
            Command::Approve {
                session_id,
                request_id,
                decision,
            } => approve(&mut socket, &mut client, session_id, request_id, decision).await,
        }
    }

    fn parse_args() -> Result<Args, Box<dyn Error>> {
        let mut positional = Vec::new();
        let mut url = None;
        let mut token = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--url" => url = Some(args.next().ok_or("--url requires a value")?),
                "--token" => token = Some(args.next().ok_or("--token requires a value")?),
                "-h" | "--help" => {
                    print_usage();
                    std::process::exit(0);
                }
                _ if arg.starts_with('-') => return Err(format!("unknown flag: {arg}").into()),
                _ => positional.push(arg),
            }
        }

        let url = url.ok_or("missing required --url")?;
        let command = match positional.as_slice() {
            [command] if command == "list" => Command::List,
            [command, session_id] if command == "watch" => Command::Watch {
                session_id: session_id.clone(),
            },
            [command, session_id, text] if command == "send" => Command::Send {
                session_id: session_id.clone(),
                text: text.clone(),
            },
            [command, session_id, request_id, decision] if command == "approve" => {
                Command::Approve {
                    session_id: session_id.clone(),
                    request_id: request_id.clone(),
                    decision: parse_decision(decision)?,
                }
            }
            _ => {
                print_usage();
                return Err("invalid command arguments".into());
            }
        };
        Ok(Args {
            url,
            token,
            command,
        })
    }

    fn parse_decision(value: &str) -> Result<ApprovalDecision, Box<dyn Error>> {
        match value {
            "approve" => Ok(ApprovalDecision::Approve),
            "approve-for-session" => Ok(ApprovalDecision::ApproveForSession),
            "deny" => Ok(ApprovalDecision::Deny),
            "cancel" => Ok(ApprovalDecision::Cancel),
            value if value.starts_with("option:") => Ok(ApprovalDecision::Option(
                value.trim_start_matches("option:").into(),
            )),
            _ => Err(format!(
                "invalid decision {value:?}; use approve, approve-for-session, deny, cancel, or option:<id>"
            )
            .into()),
        }
    }

    fn print_usage() {
        eprintln!(
            "usage: sync-probe <command> [arguments] --url <ws-url> [--token <token>]\n\
             commands:\n\
             \x20 list\n\
             \x20 watch <session-id>\n\
             \x20 send <session-id> <text>\n\
             \x20 approve <session-id> <request-id> <decision>"
        );
    }

    fn default_token() -> Result<String, Box<dyn Error>> {
        let root = SessionStore::open_default()?.root().clone();
        Ok(SettingsStore::new(root).load_or_create_sync_token()?)
    }

    async fn send_frame(socket: &mut Socket, frame: ClientFrame) -> Result<(), Box<dyn Error>> {
        socket.send(Message::Text(frame.encode()?.into())).await?;
        Ok(())
    }

    async fn recv_frame(socket: &mut Socket) -> Result<HostFrame, Box<dyn Error>> {
        loop {
            let message = tokio::time::timeout(REPLY_TIMEOUT, socket.next())
                .await
                .map_err(|_| "timed out waiting for the host")?
                .ok_or("host closed the connection")??;
            match message {
                Message::Text(text) => return Ok(HostFrame::decode(&text)?),
                Message::Close(_) => return Err("host closed the connection".into()),
                _ => {}
            }
        }
    }

    async fn handle(
        socket: &mut Socket,
        client: &mut Client,
        frame: HostFrame,
    ) -> Result<(), Box<dyn Error>> {
        for outgoing in client.handle(frame) {
            send_frame(socket, outgoing).await?;
        }
        Ok(())
    }

    async fn handshake(socket: &mut Socket, client: &mut Client) -> Result<(), Box<dyn Error>> {
        let frame = recv_frame(socket).await?;
        handle(socket, client, frame).await?;
        if let Some(reason) = client.refusal_reason() {
            return Err(format!("host refused the connection: {reason:?}").into());
        }
        if !client.is_ready() {
            return Err("host did not complete the sync handshake".into());
        }
        Ok(())
    }

    async fn list(socket: &mut Socket, client: &mut Client) -> Result<(), Box<dyn Error>> {
        send_frame(socket, client.request_sessions()).await?;
        loop {
            let frame = recv_frame(socket).await?;
            let is_list = matches!(frame, HostFrame::SessionList { .. });
            handle(socket, client, frame).await?;
            if is_list {
                break;
            }
        }

        println!("sessions: {}", client.sessions().len());
        for session in client.sessions() {
            println!(
                "{}\t{}\t{}\tworking={}\tawaiting-approval={}",
                session.session_id,
                session.title,
                session.provider.display_name(),
                session.working,
                session.awaiting_approval
            );
        }
        Ok(())
    }

    async fn subscribe_until_caught_up(
        socket: &mut Socket,
        client: &mut Client,
        session_id: &str,
        print_events: bool,
    ) -> Result<(), Box<dyn Error>> {
        let frame = client.subscribe(session_id);
        send_frame(socket, frame).await?;
        loop {
            let frame = recv_frame(socket).await?;
            if print_events {
                print_events_for(&frame, session_id);
            }
            handle(socket, client, frame).await?;
            if client
                .session(session_id)
                .is_some_and(|session| session.is_caught_up())
            {
                return Ok(());
            }
            if let Some(rejection) = client.command_rejections().last() {
                return Err(format!("host rejected command: {:?}", rejection.reason).into());
            }
        }
    }

    async fn watch(
        socket: &mut Socket,
        client: &mut Client,
        session_id: String,
    ) -> Result<(), Box<dyn Error>> {
        subscribe_until_caught_up(socket, client, &session_id, true).await?;
        println!("watching {session_id}");
        loop {
            let frame = recv_frame(socket).await?;
            print_events_for(&frame, &session_id);
            let ended = matches!(
                &frame,
                HostFrame::SessionEnded {
                    session_id: ended,
                    ..
                } if ended == &session_id
            );
            handle(socket, client, frame).await?;
            if ended {
                return Ok(());
            }
        }
    }

    async fn send_turn(
        socket: &mut Socket,
        client: &mut Client,
        session_id: String,
        text: String,
    ) -> Result<(), Box<dyn Error>> {
        subscribe_until_caught_up(socket, client, &session_id, false).await?;
        let delivery_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_nanos()
            .try_into()
            .unwrap_or(u64::MAX);
        let frame = client.command(
            &session_id,
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options: None,
                attachments: Vec::new(),
            },
        );
        send_frame(socket, frame).await?;

        loop {
            let frame = match recv_frame(socket).await {
                Ok(frame) => frame,
                Err(err) if err.to_string().contains("timed out waiting for the host") => {
                    println!("delivery_id {delivery_id} echoed by TurnAccepted: no (timed out)");
                    return Ok(());
                }
                Err(err) => return Err(err),
            };
            print_events_for(&frame, &session_id);
            handle(socket, client, frame).await?;
            let echoed = client.unacknowledged_turns(&session_id).next().is_none();
            if echoed {
                println!("delivery_id {delivery_id} echoed by TurnAccepted: yes");
                return Ok(());
            }
            if let Some(rejection) = client.command_rejections().last() {
                println!(
                    "delivery_id {delivery_id} echoed by TurnAccepted: no ({:?})",
                    rejection.reason
                );
                return Ok(());
            }
        }
    }

    async fn approve(
        socket: &mut Socket,
        client: &mut Client,
        session_id: String,
        request_id: String,
        decision: ApprovalDecision,
    ) -> Result<(), Box<dyn Error>> {
        subscribe_until_caught_up(socket, client, &session_id, false).await?;
        let pending = client.session(&session_id).is_some_and(|session| {
            session
                .timeline()
                .pending_approvals
                .iter()
                .any(|request| request.id == request_id)
        });
        if !pending {
            return Err(format!("approval request {request_id:?} is not pending").into());
        }
        let frame = client.command(
            &session_id,
            SessionCommand::RespondApproval {
                request_id: request_id.clone(),
                decision,
            },
        );
        send_frame(socket, frame).await?;
        println!("approval response sent for request {request_id}");
        Ok(())
    }

    fn print_events_for(frame: &HostFrame, session_id: &str) {
        let HostFrame::Events {
            session_id: received,
            events,
            ..
        } = frame
        else {
            return;
        };
        if received != session_id {
            return;
        }
        for event in events {
            println!("{}\t{}", event.seq, event_summary(&event.event));
        }
    }

    fn event_summary(event: &agent::AgentEvent) -> String {
        let Ok(mut value) = serde_json::to_value(event) else {
            return "unserializable event".into();
        };
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("event")
            .to_string();
        if let Some(object) = value.as_object_mut() {
            object.remove("type");
        }
        let detail = serde_json::to_string(&value).unwrap_or_default();
        let mut summary = if detail == "{}" {
            event_type
        } else {
            format!("{event_type} {detail}")
        };
        const LIMIT: usize = 180;
        if summary.chars().count() > LIMIT {
            summary = summary.chars().take(LIMIT - 1).collect::<String>() + "…";
        }
        summary
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::main]
async fn main() {
    if let Err(err) = native::run().await {
        eprintln!("sync-probe: {err}");
        std::process::exit(1);
    }
}
