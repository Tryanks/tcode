//! Tunnels between two in-process endpoints over loopback: Preview's bytes,
//! half-close, dial failures, the hello rule, the per-connection bound and
//! fail-fast while reconnecting, and the machine's SSH server on an `ssh`
//! stream.
use std::{
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tcode_client::{
    ConnectionState,
    host::{StreamTarget, Transport, Tunnel, TunnelOpener},
    pairing::PairedHost,
};
use tcode_traverse::{
    DeviceIdentity, HostConfig, HostMux, TraverseHost,
    wire::{self, ClientLine, DeviceClaim, HostLine},
};

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-tunnel-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A machine whose host answers nothing: tunnels do not touch the mux.
struct Machine {
    host: Option<TraverseHost>,
    _backend: (
        async_channel::Receiver<tcode_client::attachment::AttachmentMessage>,
        async_channel::Sender<String>,
    ),
    _dir: TestDir,
}

impl Machine {
    fn start() -> Self {
        let dir = TestDir::new("host");
        let (to_host, host_rx) =
            async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
        let (host_tx, from_host) = async_channel::unbounded::<String>();
        let host = TraverseHost::start(
            HostMux::new(to_host, from_host),
            HostConfig {
                host_name: "Tunnel Host".into(),
                data_dir: dir.0.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        Self {
            host: Some(host),
            _backend: (host_rx, host_tx),
            _dir: dir,
        }
    }

    fn host(&self) -> &TraverseHost {
        self.host.as_ref().unwrap()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
    }
}

/// A paired device attached to the machine, with its tunnel opener.
struct Device {
    transport: Transport,
    tunnels: std::sync::Arc<dyn TunnelOpener>,
    _dir: TestDir,
}

fn attach(machine: &Machine) -> Device {
    let dir = TestDir::new("device");
    let device = DeviceIdentity::load_or_create(&dir.0).unwrap();
    device.set_details("phone".into(), None);
    let minted = machine.host().new_invitation();
    let paired: PairedHost = tcode_traverse::pair_blocking(
        &minted.invite,
        Some(std::net::Ipv4Addr::LOCALHOST.into()),
        &device,
    )
    .unwrap();
    let transport = tcode_traverse::connect(&paired, &device);
    wait_syncing(&transport);
    let tunnels = transport.current_host.as_ref().unwrap().tunnels().unwrap();
    Device {
        transport,
        tunnels,
        _dir: dir,
    }
}

/// The next message on `channel`, or `None` once `deadline` passes, which
/// only a failing test reaches.
fn next<T>(channel: &async_channel::Receiver<T>, deadline: Instant) -> Option<T> {
    tcode_traverse::block_on(async {
        tokio::time::timeout_at(deadline.into(), channel.recv()).await
    })
    .ok()?
    .ok()
}

fn wait_state(transport: &Transport, wanted: impl Fn(&ConnectionState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while let Some(state) = next(&transport.state, deadline) {
        if wanted(&state) {
            return;
        }
        seen.push(state);
    }
    panic!("did not observe the wanted state; saw {seen:?}");
}

fn wait_syncing(transport: &Transport) {
    wait_state(transport, |state| {
        matches!(state, ConnectionState::Syncing { .. })
    });
}

fn wait_reconnecting(transport: &Transport) {
    wait_state(transport, |state| {
        matches!(state, ConnectionState::Reconnecting { .. })
    });
}

fn open(device: &Device, host: &str, port: u16) -> io::Result<Tunnel> {
    tcode_traverse::block_on(device.tunnels.open(StreamTarget::Tcp {
        host: host.to_owned(),
        port,
    }))
}

fn read_all(tunnel: &mut Tunnel) -> Vec<u8> {
    let mut bytes = Vec::new();
    tcode_traverse::block_on(tunnel.read.read_to_end(&mut bytes)).unwrap();
    bytes
}

#[test]
fn tunnel_carries_bytes_both_ways_and_preserves_half_close_in_each_direction() {
    let machine = Machine::start();
    let device = attach(&machine);

    // Device finishes first: the service sees EOF after the request, then
    // answers on its still-open write side.
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = service.local_addr().unwrap().port();
    let service = std::thread::spawn(move || {
        let (mut stream, _) = service.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        assert_eq!(request, b"request\0\xff");
        stream.write_all(b"reply after EOF").unwrap();
    });
    let mut tunnel = open(&device, "localhost", port).unwrap();
    tcode_traverse::block_on(async {
        tunnel.write.write_all(b"request\0\xff").await.unwrap();
        tunnel.write.close().await.unwrap();
    });
    assert_eq!(read_all(&mut tunnel), b"reply after EOF");
    service.join().unwrap();

    // Service finishes first: the device reads EOF, then still delivers its
    // request over the open half.
    let service = TcpListener::bind("[::1]:0").unwrap();
    let port = service.local_addr().unwrap().port();
    let service = std::thread::spawn(move || {
        let (mut stream, _) = service.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(b"response first").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut request = String::new();
        stream.read_to_string(&mut request).unwrap();
        assert_eq!(request, "request after remote EOF");
    });
    let mut tunnel = open(&device, "::1", port).unwrap();
    assert_eq!(read_all(&mut tunnel), b"response first");
    tcode_traverse::block_on(async {
        tunnel
            .write
            .write_all(b"request after remote EOF")
            .await
            .unwrap();
        tunnel.write.close().await.unwrap();
    });
    service.join().unwrap();
}

#[test]
fn closed_ports_answer_connect_failed_and_a_reconnecting_attachment_fails_fast() {
    let machine = Machine::start();
    let device = attach(&machine);
    let port = {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        reserved.local_addr().unwrap().port()
    };
    let error = open(&device, "127.0.0.1", port).unwrap_err();
    let refused = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(error.to_string(), refused.to_string());

    drop(machine);
    wait_reconnecting(&device.transport);
    let error = open(&device, "127.0.0.1", port).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotConnected);
}

#[test]
fn a_connect_before_hello_is_refused_and_tunnels_per_connection_are_bounded() {
    let machine = Machine::start();
    // A raw paired endpoint that skips hello.
    let secret = iroh::SecretKey::generate();
    let raw = tcode_traverse::block_on(
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .secret_key(secret)
            .transport_config(wire::transport_config())
            .bind(),
    )
    .unwrap();
    let minted = machine.host().new_invitation();
    let addr = iroh::EndpointAddr::from_parts(
        minted.invite.host_id.parse().unwrap(),
        [iroh::TransportAddr::Ip(std::net::SocketAddr::from((
            std::net::Ipv4Addr::LOCALHOST,
            minted.invite.port,
        )))],
    );
    tcode_traverse::block_on(async {
        let connection = raw.connect(addr.clone(), wire::ALPN_PAIR).await.unwrap();
        let (mut send, recv) = connection.open_bi().await.unwrap();
        wire::write_line(
            &mut send,
            &ClientLine::Pair {
                space: None,
                secret: minted.invite.secret.clone(),
                device: DeviceClaim {
                    name: "raw".into(),
                    platform: None,
                },
            },
        )
        .await
        .unwrap();
        send.finish().unwrap();
        let mut reader = wire::reader(recv);
        assert!(matches!(
            wire::read_control::<HostLine>(&mut reader).await.unwrap(),
            HostLine::Paired { .. }
        ));
        connection.close(0_u32.into(), b"done");

        let connection = raw.connect(addr, wire::ALPN_MAIN).await.unwrap();
        let (mut send, recv) = connection.open_bi().await.unwrap();
        wire::write_line(
            &mut send,
            &ClientLine::Connect {
                host: "127.0.0.1".into(),
                port: 1,
            },
        )
        .await
        .unwrap();
        let mut reader = wire::reader(recv);
        assert_eq!(
            wire::read_control::<HostLine>(&mut reader).await.unwrap(),
            HostLine::Refused {
                reason: "hello required".into()
            }
        );
        connection.close(0_u32.into(), b"done");
    });
    tcode_traverse::block_on(raw.close());

    let device = attach(&machine);
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = service.local_addr().unwrap().port();
    let held = std::thread::spawn(move || {
        let mut streams = Vec::new();
        for _ in 0..wire::MAX_TUNNELS {
            streams.push(service.accept().unwrap().0);
        }
        streams
    });
    let tunnels: Vec<Tunnel> = (0..wire::MAX_TUNNELS)
        .map(|_| open(&device, "127.0.0.1", port).unwrap())
        .collect();
    let error = open(&device, "127.0.0.1", port).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "too many tunnels");
    drop(tunnels);
    drop(held.join().unwrap());
    // The slots are released with the tunnels.
    let deadline = Instant::now() + Duration::from_secs(10);
    let reopened = loop {
        let service = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = service.local_addr().unwrap().port();
        let accept = std::thread::spawn(move || service.accept().map(|_| ()));
        match open(&device, "127.0.0.1", port) {
            Ok(tunnel) => {
                accept.join().unwrap().unwrap();
                break tunnel;
            }
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                let _ = TcpStream::connect(("127.0.0.1", port));
                let _ = accept.join();
                assert!(Instant::now() < deadline, "tunnel slots were not released");
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("{error}"),
        }
    };
    drop(reopened);
}

/// A tunnel as the tokio streams russh speaks over.
struct TokioTunnel(Tunnel);

impl tokio::io::AsyncRead for TokioTunnel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let read = std::task::ready!(futures_lite::AsyncRead::poll_read(
            std::pin::Pin::new(&mut self.get_mut().0.read),
            cx,
            buf.initialize_unfilled(),
        ))?;
        buf.advance(read);
        std::task::Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for TokioTunnel {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        futures_lite::AsyncWrite::poll_write(
            std::pin::Pin::new(&mut self.get_mut().0.write),
            cx,
            buf,
        )
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        futures_lite::AsyncWrite::poll_flush(std::pin::Pin::new(&mut self.get_mut().0.write), cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        futures_lite::AsyncWrite::poll_close(std::pin::Pin::new(&mut self.get_mut().0.write), cx)
    }
}

/// An SSH client that trusts the machine's key: the iroh connection
/// already authenticated it.
struct TrustingClient;

impl russh::client::Handler for TrustingClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[test]
fn an_ssh_stream_runs_commands_through_the_machine_shell_and_takes_sftp_writes() {
    let machine = Machine::start();
    let device = attach(&machine);
    let tunnel = tcode_traverse::block_on(device.tunnels.open(StreamTarget::Ssh)).unwrap();
    let files = TestDir::new("sftp");
    let uploaded = files.0.join("uploaded.bin");
    let (stdout, stderr, exit_status) = tcode_traverse::block_on(async {
        let mut ssh = russh::client::connect_stream(
            std::sync::Arc::new(russh::client::Config::default()),
            TokioTunnel(tunnel),
            TrustingClient,
        )
        .await
        .unwrap();
        assert!(ssh.authenticate_none("anyone").await.unwrap().success());

        // Words every shell the machine may have (sh, zsh, cmd.exe) reads
        // alike.
        let mut exec = ssh.channel_open_session().await.unwrap();
        exec.exec(true, "echo hello && echo oops 1>&2 && exit 3")
            .await
            .unwrap();
        let (mut stdout, mut stderr, mut exit_status) = (Vec::new(), Vec::new(), None);
        while let Some(message) = exec.wait().await {
            match message {
                russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus {
                    exit_status: status,
                } => exit_status = Some(status),
                _ => {}
            }
        }

        // A session that asks for a terminal runs in one.
        #[cfg(unix)]
        {
            let mut pty = ssh.channel_open_session().await.unwrap();
            pty.request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
                .await
                .unwrap();
            pty.exec(true, "tty").await.unwrap();
            let mut output = Vec::new();
            while let Some(message) = pty.wait().await {
                if let russh::ChannelMsg::Data { data } = message {
                    output.extend_from_slice(&data);
                }
            }
            let output = String::from_utf8_lossy(&output);
            assert!(output.trim().starts_with("/dev/"), "{output}");
        }

        let sftp_channel = ssh.channel_open_session().await.unwrap();
        sftp_channel.request_subsystem(true, "sftp").await.unwrap();
        let sftp = russh_sftp::client::SftpSession::new(sftp_channel.into_stream())
            .await
            .unwrap();
        let mut file = sftp
            .create(uploaded.to_string_lossy().into_owned())
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut file, b"zed-remote-server\0\xff")
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::shutdown(&mut file).await.unwrap();
        drop(file);
        sftp.close().await.unwrap();
        (stdout, stderr, exit_status)
    });
    assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");
    assert_eq!(String::from_utf8_lossy(&stderr).trim(), "oops");
    assert_eq!(exit_status, Some(3));
    assert_eq!(
        std::fs::read(&uploaded).unwrap(),
        b"zed-remote-server\0\xff"
    );
}
