//! Opening a paired machine's project in Zed. Zed's own SSH remote
//! development reaches the machine through this app's iroh connection to
//! it: Zed's ssh runs `tcode ssh-proxy`, which hands its bytes to this app
//! over a local endpoint, and the app carries them on an `ssh` stream to the
//! machine's SSH server. The pipe admits the Zed this app launched for a
//! machine, as far as [`grant`] can tell it apart.

mod endpoint;
mod grant;
mod process;
pub(crate) mod proxy;
mod zed_config;

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

use futures_lite::{AsyncReadExt as _, AsyncWriteExt as _};
use tcode_client::host::{
    HostFuture, MachineEditor, MachinePath, StreamTarget, Tunnel, TunnelOpener,
};

/// The longest machine id line the proxy may send.
const MAX_REQUEST: usize = 256;

/// Whether Zed reaches a machine through one ControlMaster connection,
/// which every other ssh command of its reuses (crates/remote's
/// `MasterProcess`); on Windows each command is a connection of its own.
const ZED_USES_A_MASTER: bool = cfg!(unix);

/// The proxy `pid`, its parent and its grandparent, as the system reports
/// them.
fn chain(pid: u32) -> Option<grant::Chain> {
    let read = |pid: u32, zed: bool| {
        let info = process::process(pid)?;
        Some((
            grant::Process {
                pid,
                exe: info.exe,
                args: info.args,
                started: info.started,
                zed: zed && process::is_zed(pid),
            },
            info.parent,
        ))
    };
    let (proxy, ssh) = read(pid, false)?;
    let (ssh, zed) = read(ssh?, false)?;
    let (zed, _) = read(zed?, true)?;
    Some(grant::Chain { proxy, ssh, zed })
}

/// Whether the process `key` names still runs.
fn alive(key: grant::ProcessKey) -> bool {
    process::process(key.pid).is_some_and(|info| info.started == key.started)
}

/// The desktop's side of Open in Zed for paired machines.
#[derive(Clone)]
pub(crate) struct ZedRemote(Arc<Inner>);

struct Inner {
    data_dir: PathBuf,
    grants: Mutex<grant::Grants>,
    /// Each machine's tunnels, as the window's attachment holds them.
    links: Mutex<HashMap<String, Weak<dyn TunnelOpener>>>,
    listening: OnceLock<Result<(), String>>,
}

impl ZedRemote {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self(Arc::new(Inner {
            data_dir,
            grants: Mutex::default(),
            links: Mutex::default(),
            listening: OnceLock::new(),
        }))
    }

    /// Bind the local endpoint once and serve it for the rest of the run.
    fn listen(&self) -> Result<(), String> {
        let inner = self.0.clone();
        self.0
            .listening
            .get_or_init(move || {
                let listener =
                    endpoint::Listener::bind(&inner.data_dir).map_err(|error| error.to_string())?;
                std::thread::Builder::new()
                    .name("tcode-ssh-pipe".into())
                    .spawn(move || {
                        loop {
                            match listener.accept() {
                                Ok(stream) => {
                                    let inner = inner.clone();
                                    let _ = std::thread::Builder::new()
                                        .name("tcode-ssh-pipe-connection".into())
                                        .spawn(move || serve(&inner, stream));
                                }
                                Err(error) => {
                                    log::error!("the ssh pipe stopped accepting: {error}");
                                    return;
                                }
                            }
                        }
                    })
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .clone()
    }
}

impl MachineEditor for ZedRemote {
    fn settings_file(&self) -> Option<PathBuf> {
        zed_config::zed_settings_path()
    }

    /// Tcode's ssh config and Zed's settings entries in place for every
    /// paired machine, the machine's grant window open, then `zed ssh://…`.
    fn open(&self, target: MachinePath) -> HostFuture<'static, Result<(), String>> {
        let prepared = self.prepare(&target);
        Box::pin(async move {
            let (cli, url, settings_changed) = prepared?;
            if settings_changed {
                // A running Zed picks up a changed settings.json by watching
                // the file and reloading it; an `ssh://` request it handles
                // before that reload would not find the entry's args. Only
                // the first open after a machine is paired changes them.
                smol::Timer::after(Duration::from_secs(1)).await;
            }
            tcode_services::desktop::open_in_zed_with(&cli, &url).map_err(|error| error.to_string())
        })
    }
}

impl ZedRemote {
    fn prepare(&self, target: &MachinePath) -> Result<(PathBuf, String, bool), String> {
        let inner = &self.0;
        let cli = tcode_services::desktop::zed_cli()
            .ok_or_else(|| tcode_ui::tr!("errors.zed_cli_missing").into_owned())?;
        self.listen()?;
        let exe = process::current_exe()
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| "Tcode could not find its own executable".to_owned())?;
        let hosts = tcode_traverse::hosts::load_hosts(&inner.data_dir).unwrap_or_default();
        let mut machines: Vec<(String, String)> = hosts
            .iter()
            .map(|host| (host.host_id.clone(), host.name.clone()))
            .collect();
        if !machines.iter().any(|(id, _)| *id == target.host_id) {
            machines.push((target.host_id.clone(), zed_config::alias(&target.host_id)));
        }
        let ids: Vec<String> = machines.iter().map(|(id, _)| id.clone()).collect();
        zed_config::write_ssh_config(&inner.data_dir, &exe, &ids)
            .map_err(|error| error.to_string())?;
        let settings_changed = update_zed_settings(&inner.data_dir, &machines)?;
        inner
            .links
            .lock()
            .unwrap()
            .insert(target.host_id.clone(), Arc::downgrade(&target.tunnels));
        inner
            .grants
            .lock()
            .unwrap()
            .open(&target.host_id, Instant::now(), alive);
        let url = machine_url(&zed_config::alias(&target.host_id), &target.path)?;
        Ok((cli, url, settings_changed))
    }
}

/// Add or refresh Zed's settings entry for each of `machines`, `(id,
/// name)`; whether the file changed.
fn update_zed_settings(
    data_dir: &std::path::Path,
    machines: &[(String, String)],
) -> Result<bool, String> {
    let path = zed_config::zed_settings_path()
        .ok_or_else(|| "Tcode could not find Zed's settings".to_owned())?;
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let config = zed_config::ssh_config_path(data_dir);
    let mut updated = None;
    for (id, name) in machines {
        let text: &str = updated.as_deref().unwrap_or(&current);
        if let Some(next) = zed_config::with_connection(text, &zed_config::alias(id), name, &config)
            .map_err(|error| format!("{}: {error}", path.display()))?
        {
            updated = Some(next);
        }
    }
    let Some(updated) = updated else {
        return Ok(false);
    };
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, updated).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(true)
}

/// `ssh://alias/path` for a path as the machine writes it, Windows drive
/// paths included.
fn machine_url(alias: &str, path: &str) -> Result<String, String> {
    let mut url = url::Url::parse(&format!("ssh://{alias}")).map_err(|error| error.to_string())?;
    let path = path.replace('\\', "/");
    url.set_path(&path);
    Ok(url.into())
}

/// One connection from a proxy: who it is, whether it may, then its bytes.
fn serve(inner: &Inner, mut stream: endpoint::LocalStream) {
    let Some(machine) = read_request(&mut stream) else {
        return;
    };
    match admit(inner, &machine, &stream) {
        Ok(tunnel) => {
            if stream.write_all(b"ok\n").is_ok() {
                splice(stream, tunnel);
            }
        }
        Err(reason) => {
            log::info!("ssh pipe to {machine} refused: {reason}");
            let _ = stream.write_all(format!("refused {reason}\n").as_bytes());
            stream.shutdown();
        }
    }
}

fn read_request(stream: &mut endpoint::LocalStream) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0];
    while line.len() < MAX_REQUEST {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        if byte[0] == b'\n' {
            return String::from_utf8(line).ok();
        }
        line.push(byte[0]);
    }
    None
}

fn admit(inner: &Inner, machine: &str, stream: &endpoint::LocalStream) -> Result<Tunnel, String> {
    let name = tcode_traverse::hosts::load_hosts(&inner.data_dir)
        .ok()
        .and_then(|hosts| hosts.into_iter().find(|host| host.host_id == machine))
        .map_or_else(|| machine.to_owned(), |host| host.name);
    let refused = |refusal: grant::Refusal| match refusal {
        grant::Refusal::NotZed => tcode_ui::tr!("editor.zed.pipe_not_zed").into_owned(),
        grant::Refusal::NotGranted => {
            tcode_ui::tr!("editor.zed.pipe_not_granted", machine = name.as_str()).into_owned()
        }
    };
    let pid = stream.peer_pid().map_err(|error| error.to_string())?;
    let own_exe = process::current_exe().ok_or_else(|| refused(grant::Refusal::NotZed))?;
    let chain = chain(pid).ok_or_else(|| refused(grant::Refusal::NotZed))?;
    let zed = grant::zed_behind(&chain, &own_exe, ZED_USES_A_MASTER).map_err(refused)?;
    inner
        .grants
        .lock()
        .unwrap()
        .admit(machine, zed, Instant::now(), alive)
        .map_err(refused)?;
    let not_connected =
        || tcode_ui::tr!("editor.zed.pipe_not_connected", machine = name.as_str()).into_owned();
    let tunnels = inner
        .links
        .lock()
        .unwrap()
        .get(machine)
        .and_then(Weak::upgrade)
        .ok_or_else(not_connected)?;
    futures_lite::future::block_on(tunnels.open(StreamTarget::Ssh)).map_err(|error| {
        match error.kind() {
            std::io::ErrorKind::NotConnected => not_connected(),
            _ => tcode_ui::tr!(
                "editor.zed.pipe_failed",
                machine = name.as_str(),
                reason = error.to_string()
            )
            .into_owned(),
        }
    })
}

/// Copy both ways until either side ends, then end the other.
fn splice(stream: endpoint::LocalStream, tunnel: Tunnel) {
    let Tunnel {
        mut read,
        mut write,
    } = tunnel;
    let Ok(mut upstream) = stream.try_clone() else {
        return;
    };
    let upload = std::thread::Builder::new()
        .name("tcode-ssh-pipe-upload".into())
        .spawn(move || {
            let mut buffer = vec![0; 32 * 1024];
            loop {
                let count = match upstream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let sent = futures_lite::future::block_on(async {
                    write.write_all(&buffer[..count]).await?;
                    write.flush().await
                });
                if sent.is_err() {
                    break;
                }
            }
            let _ = futures_lite::future::block_on(write.close());
            upstream.shutdown();
        });
    let mut downstream = stream;
    let mut buffer = vec![0; 32 * 1024];
    loop {
        let count = match futures_lite::future::block_on(read.read(&mut buffer)) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        if downstream.write_all(&buffer[..count]).is_err() {
            break;
        }
    }
    downstream.shutdown();
    if let Ok(upload) = upload {
        let _ = upload.join();
    }
}
