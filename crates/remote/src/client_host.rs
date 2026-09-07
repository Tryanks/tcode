//! Native implementation of the transport-agnostic client host contract.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tcode_client::host::{
    ClientHost, ClientPreferences, DiscoveredHost, HostFuture, PairRequest, Transport,
};
use tcode_client::pairing::PairedHost;

type QrScanner = dyn Fn() -> HostFuture<'static, Result<String, String>>;
type HostBrowser = dyn Fn() -> HostFuture<'static, Vec<DiscoveredHost>>;
type MulticastLock = dyn Fn(bool) + Send + Sync;
type EditorOpener = dyn Fn(&Path) -> Result<(), String>;

/// Native clients share hosts.json, mobile.json, pairing, and transport policy.
pub struct NativeClientHost {
    data_dir: PathBuf,
    default_device_name: String,
    qr_scanner: Option<Box<QrScanner>>,
    browser: Option<Box<HostBrowser>>,
    multicast_lock: Option<Arc<MulticastLock>>,
    editor: Option<Box<EditorOpener>>,
}

impl NativeClientHost {
    pub fn new(data_dir: PathBuf, device_name: impl Into<String>) -> Self {
        Self {
            data_dir,
            default_device_name: device_name.into(),
            qr_scanner: None,
            browser: None,
            multicast_lock: None,
            editor: None,
        }
    }

    /// `TCODE_DATA_DIR`, else the platform data dir; hostname as device name.
    pub fn from_env() -> Self {
        Self::from_env_with_device_name(default_device_name())
    }

    /// Uses the normal platform data directory with a platform-provided name.
    pub fn from_env_with_device_name(device_name: impl Into<String>) -> Self {
        let data_dir = match std::env::var_os("TCODE_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("tcode"),
        };
        Self::new(data_dir, device_name)
    }

    pub fn with_qr_scanner(
        mut self,
        scanner: impl Fn() -> HostFuture<'static, Result<String, String>> + 'static,
    ) -> Self {
        self.qr_scanner = Some(Box::new(scanner));
        self
    }

    pub fn with_multicast_lock(mut self, lock: impl Fn(bool) + Send + Sync + 'static) -> Self {
        self.multicast_lock = Some(Arc::new(lock));
        self
    }

    pub fn with_browser(
        mut self,
        browser: impl Fn() -> HostFuture<'static, Vec<DiscoveredHost>> + 'static,
    ) -> Self {
        self.browser = Some(Box::new(browser));
        self
    }

    /// Supply the external-editor launcher. It is injected because launching a
    /// process belongs to the composition root that already owns the process
    /// helpers, not to this transport crate.
    pub fn with_editor_opener(
        mut self,
        open: impl Fn(&Path) -> Result<(), String> + 'static,
    ) -> Self {
        self.editor = Some(Box::new(open));
        self
    }

    fn prefs_path(&self) -> PathBuf {
        self.data_dir.join("mobile.json")
    }

    fn prefs(&self) -> serde_json::Value {
        fs::read(self.prefs_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}))
    }

    fn write_prefs(&self, prefs: &serde_json::Value) {
        let result = serde_json::to_vec_pretty(prefs)
            .map_err(io::Error::other)
            .and_then(|bytes| write_private(&self.data_dir, "mobile.json", &bytes));
        if let Err(error) = result {
            log::error!("could not write mobile.json: {error}");
        }
    }
}

impl ClientHost for NativeClientHost {
    fn device_name(&self) -> String {
        self.load_preferences()
            .device_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.default_device_name.clone())
    }

    fn load_preferences(&self) -> ClientPreferences {
        let prefs = self.prefs();
        let value = |key: &str| prefs.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        ClientPreferences {
            appearance: value("appearance"),
            language: value("language"),
            device_name: value("device_name"),
            navigation: prefs
                .get("navigation")
                .filter(|value| !value.is_null())
                .cloned(),
        }
    }

    fn save_preferences(&self, preferences: &ClientPreferences) {
        let mut prefs = self.prefs();
        prefs["appearance"] = serde_json::json!(preferences.appearance);
        prefs["language"] = serde_json::json!(preferences.language);
        prefs["device_name"] = serde_json::json!(preferences.device_name);
        prefs["navigation"] = serde_json::json!(preferences.navigation);
        self.write_prefs(&prefs);
    }

    fn load_hosts(&self) -> Vec<PairedHost> {
        crate::client::load_hosts(&self.data_dir).unwrap_or_else(|error| {
            log::error!("could not read hosts.json: {error}");
            Vec::new()
        })
    }

    fn save_hosts(&self, hosts: &[PairedHost]) {
        if let Err(error) = crate::client::save_hosts(&self.data_dir, hosts) {
            log::error!("could not write hosts.json: {error}");
        }
    }

    fn last_host_id(&self) -> Option<String> {
        self.prefs()
            .get("last_host_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        let mut prefs = self.prefs();
        prefs["last_host_id"] = match host_id {
            Some(id) => serde_json::Value::String(id.to_owned()),
            None => serde_json::Value::Null,
        };
        self.write_prefs(&prefs);
    }

    fn pair(&self, request: PairRequest) -> HostFuture<'_, Result<PairedHost, String>> {
        let device_name = self.device_name();
        let (sender, receiver) = async_channel::bounded(1);
        let spawn = std::thread::Builder::new()
            .name("tcode-pair".into())
            .spawn(move || {
                let result = crate::client::pair(&request.origin, &request.code, &device_name);
                let _ = sender.send_blocking(result);
            });
        Box::pin(async move {
            if let Err(error) = spawn {
                return Err(format!("failed to start pairing: {error}"));
            }
            receiver
                .recv()
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
        })
    }

    fn open_in_editor(&self, path: &Path) -> Option<Result<(), String>> {
        self.editor.as_ref().map(|open| open(path))
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        let client = crate::client::connect(host.clone(), self.device_name());
        Transport {
            to_host: client.to_host,
            from_host: client.from_host,
            state: client.state,
        }
    }

    fn browse_hosts(&self) -> HostFuture<'_, Vec<DiscoveredHost>> {
        if let Some(browser) = &self.browser {
            return browser();
        }
        #[cfg(target_os = "ios")]
        return Box::pin(async { Vec::new() });
        #[cfg(not(target_os = "ios"))]
        {
            let lock = self.multicast_lock.clone();
            let (sender, receiver) = async_channel::bounded(1);
            let spawn = std::thread::Builder::new()
                .name("tcode-mdns-browse".into())
                .spawn(move || {
                    struct Guard(Option<Arc<MulticastLock>>);
                    impl Drop for Guard {
                        fn drop(&mut self) {
                            if let Some(lock) = &self.0 {
                                lock(false);
                            }
                        }
                    }
                    if let Some(lock) = &lock {
                        lock(true);
                    }
                    let _guard = Guard(lock);
                    let hosts = crate::discovery::browse(std::time::Duration::from_secs(3))
                        .into_iter()
                        .map(|beacon| DiscoveredHost {
                            host_id: beacon.host_id,
                            name: beacon.name,
                            origin: tcode_client::pairing::lan_origin(&beacon.addr, beacon.port),
                        })
                        .collect();
                    let _ = sender.send_blocking(hosts);
                });
            Box::pin(async move {
                if spawn.is_err() {
                    return Vec::new();
                }
                receiver.recv().await.unwrap_or_default()
            })
        }
    }

    fn refresh_origin(&self, host_id: &str) -> HostFuture<'_, Option<String>> {
        let host_id = host_id.to_owned();
        Box::pin(async move {
            let previous = self
                .load_hosts()
                .into_iter()
                .find(|host| host.host_id == host_id)?;
            if !lan_http_origin(&previous.origin) {
                return None;
            }
            let hint = self.browse_hosts().await.into_iter().find(|hint| {
                hint.host_id == host_id
                    && hint.origin != previous.origin
                    && lan_http_origin(&hint.origin)
            })?;
            // Reload after browsing so concurrent pairing/preferences never get overwritten.
            let mut hosts = self.load_hosts();
            let saved = hosts.iter_mut().find(|host| host.host_id == host_id)?;
            if saved.origin != previous.origin {
                return None;
            }
            saved.origin = hint.origin.clone();
            crate::client::save_hosts(&self.data_dir, &hosts)
                .map_err(|error| {
                    log::error!("could not persist refreshed machine address: {error}");
                })
                .ok()?;
            Some(hint.origin)
        })
    }

    fn supports_qr(&self) -> bool {
        self.qr_scanner.is_some()
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<String, String>> {
        self.qr_scanner.as_ref().map_or_else(
            || Box::pin(async { Err("unsupported".into()) }) as HostFuture<'_, _>,
            |scanner| scanner(),
        )
    }
}

fn lan_http_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" || tcode_client::pairing::parse_origin(origin).is_err() {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_link_local(),
        Some(url::Host::Ipv6(ip)) => ip.is_unique_local() || ip.is_unicast_link_local(),
        Some(url::Host::Domain(name)) => name.ends_with(".local"),
        None => false,
    }
}

/// This machine's name, shared by desktop, headless, and native-client defaults.
pub fn default_device_name() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(resolve_default_device_name).clone()
}

fn resolve_default_device_name() -> String {
    resolve_device_name(
        ["HOSTNAME", "HOST", "COMPUTERNAME"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .find_map(|name| clean_name(Some(name))),
        macos_computer_name(),
        system_host_name(),
        fs::read_to_string("/etc/hostname").ok(),
    )
}

fn resolve_device_name(
    env_name: Option<String>,
    computer_name: Option<String>,
    host_name: Option<String>,
    etc_hostname: Option<String>,
) -> String {
    clean_name(env_name)
        .or_else(|| clean_name(computer_name))
        .or_else(|| {
            clean_name(host_name).and_then(|name| clean_name(Some(strip_local_suffix(name))))
        })
        .or_else(|| clean_name(etc_hostname))
        .unwrap_or_else(|| "tcode".into())
}

fn clean_name(name: Option<String>) -> Option<String> {
    name.map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

fn strip_local_suffix(name: String) -> String {
    name.strip_suffix(".local").unwrap_or(&name).to_owned()
}

#[cfg(target_os = "macos")]
fn macos_computer_name() -> Option<String> {
    use core_foundation::{base::TCFType as _, string::CFString};
    use system_configuration_sys::dynamic_store_copy_specific::SCDynamicStoreCopyComputerName;

    let mut encoding = 0;
    // SAFETY: a null store requests the current system value. The returned
    // CFString follows the Create Rule and is transferred to the wrapper.
    let name = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null_mut(), &mut encoding) };
    (!name.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(name) }.to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_computer_name() -> Option<String> {
    None
}

#[cfg(unix)]
fn system_host_name() -> Option<String> {
    let mut bytes = [0_u8; 256];
    // SAFETY: `bytes` is a valid writable buffer and its exact length is
    // supplied to gethostname. It starts zeroed so the terminator is findable.
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return None;
    }
    let length = bytes.iter().position(|byte| *byte == 0)?;
    String::from_utf8(bytes[..length].to_vec()).ok()
}

#[cfg(not(unix))]
fn system_host_name() -> Option<String> {
    None
}

fn write_private(data_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = data_dir.join(name);
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    use io::Write as _;
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn discovery_refresh_matches_identity_persists_origin_and_preserves_token() {
        let dir = TestDir::new();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        let client = NativeClientHost::new(dir.0.clone(), "phone").with_browser(move || {
            observed.set(observed.get() + 1);
            Box::pin(async {
                vec![
                    DiscoveredHost {
                        host_id: "wrong".into(),
                        name: "Wrong".into(),
                        origin: "http://192.168.1.99:47420".into(),
                    },
                    DiscoveredHost {
                        host_id: "right".into(),
                        name: "Right".into(),
                        origin: "http://192.168.1.25:47420".into(),
                    },
                ]
            })
        });
        let saved = PairedHost {
            host_id: "right".into(),
            name: "My machine".into(),
            origin: "http://192.168.1.24:47420".into(),
            token: "unchanged-token".into(),
            last_connected_unix: Some(42),
        };
        client.save_hosts(std::slice::from_ref(&saved));
        assert_eq!(
            smol::block_on(client.refresh_origin("right")).as_deref(),
            Some("http://192.168.1.25:47420")
        );
        let refreshed = client.load_hosts().remove(0);
        assert_eq!(refreshed.token, saved.token);
        assert_eq!(refreshed.name, saved.name);
        assert_eq!(refreshed.last_connected_unix, Some(42));
        assert_eq!(calls.get(), 1);
        let mut tunnel = refreshed;
        tunnel.origin = "https://tunnel.example.com".into();
        client.save_hosts(&[tunnel]);
        assert_eq!(smol::block_on(client.refresh_origin("right")), None);
        assert_eq!(calls.get(), 1, "HTTPS must not browse");
    }

    #[test]
    fn device_name_prefers_environment_override() {
        assert_eq!(
            resolve_device_name(
                Some(" Explicit name ".into()),
                Some("Friendly Mac".into()),
                Some("host.local".into()),
                Some("etc-host".into()),
            ),
            "Explicit name"
        );
    }

    #[test]
    fn device_name_strips_local_suffix_from_system_hostname() {
        assert_eq!(
            resolve_device_name(None, None, Some("studio.local".into()), None),
            "studio"
        );
        assert_eq!(
            resolve_device_name(None, None, Some("studio.localdomain".into()), None),
            "studio.localdomain"
        );
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "tcode-native-client-host-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_and_updates_existing_mobile_preferences_without_losing_last_host() {
        let dir = TestDir::new();
        fs::write(
            dir.0.join("mobile.json"),
            br#"{
                "appearance": "dark",
                "language": "zh-CN",
                "device_name": "My phone",
                "last_host_id": "host-before-client-seam",
                "future_field": {"preserve": true}
            }"#,
        )
        .unwrap();
        let host = NativeClientHost::new(dir.0.clone(), "fallback");

        assert_eq!(
            host.load_preferences(),
            ClientPreferences {
                appearance: Some("dark".into()),
                language: Some("zh-CN".into()),
                device_name: Some("My phone".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            host.last_host_id().as_deref(),
            Some("host-before-client-seam")
        );
        host.save_preferences(&ClientPreferences {
            appearance: Some("light".into()),
            language: None,
            device_name: Some("Renamed".into()),
            navigation: Some(serde_json::json!({"history": ["hosts", "threads"]})),
        });

        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("mobile.json")).unwrap()).unwrap();
        assert_eq!(saved["last_host_id"], "host-before-client-seam");
        assert_eq!(saved["future_field"]["preserve"], true);
        assert_eq!(saved["appearance"], "light");
        assert_eq!(
            host.load_preferences().navigation.unwrap()["history"],
            serde_json::json!(["hosts", "threads"])
        );
        assert!(saved["language"].is_null());

        host.set_last_host_id(Some("next-host"));
        assert_eq!(host.last_host_id().as_deref(), Some("next-host"));
        assert_eq!(host.device_name(), "Renamed");
    }
}
