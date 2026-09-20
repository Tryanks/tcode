//! Native implementation of the transport-agnostic client host contract.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tcode_client::host::{
    ClientHost, ClientPreferences, DiscoveredHost, HostFuture, PairRequest, Transport,
    persistent_device_id,
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
    platform: Option<String>,
    qr_scanner: Option<Box<QrScanner>>,
    browser: Option<Box<HostBrowser>>,
    multicast_lock: Option<Arc<MulticastLock>>,
    editor: Option<Box<EditorOpener>>,
}

impl NativeClientHost {
    /// The platform starts as this operating system's description; mobile
    /// bootstraps replace it with [`NativeClientHost::with_platform`].
    pub fn new(data_dir: PathBuf, device_name: impl Into<String>) -> Self {
        Self {
            data_dir,
            default_device_name: device_name.into(),
            platform: default_device_platform(),
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

    /// Operating system name and version reported to hosts, e.g. `Android 15`.
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
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

    fn device_id(&self) -> String {
        let mut prefs = self.prefs();
        let stored = prefs
            .get("device_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        persistent_device_id(stored, |id| {
            prefs["device_id"] = serde_json::Value::String(id.to_owned());
            self.write_prefs(&prefs);
        })
    }

    fn device_platform(&self) -> Option<String> {
        self.platform.clone()
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

    fn outbox_storage(&self, host_id: &str) -> Option<Arc<dyn tcode_client::outbox::Storage>> {
        // Host IDs are opaque; encoding their bytes prevents path traversal.
        let name: String = host_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Some(Arc::new(FileOutbox {
            data_dir: self.data_dir.clone(),
            name: format!("outbox-{name}.json"),
        }))
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

    fn remember_host(&self, host: PairedHost) {
        if let Err(error) = crate::client::update_hosts(&self.data_dir, |hosts| {
            tcode_client::pairing::remember_host(hosts, host);
        }) {
            log::error!("could not save paired machine: {error}");
        }
    }

    fn remove_host(&self, host_id: &str) {
        if let Err(error) = crate::client::update_hosts(&self.data_dir, |hosts| {
            hosts.retain(|host| host.host_id != host_id);
        }) {
            log::error!("could not remove paired machine: {error}");
        }
    }

    fn stamp_connected(&self, host_id: &str, timestamp: u64) {
        if let Err(error) = crate::client::update_hosts(&self.data_dir, |hosts| {
            if let Some(host) = hosts.iter_mut().find(|host| host.host_id == host_id) {
                host.last_connected_unix = Some(timestamp);
            }
        }) {
            log::error!("could not record machine connection: {error}");
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
        let device = self.device_identity();
        Box::pin(async move { crate::client::pair_request(request, &device).await })
    }

    fn open_in_editor(&self, path: &Path) -> Option<Result<(), String>> {
        self.editor.as_ref().map(|open| open(path))
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        let client = crate::client::connect(
            host.clone(),
            self.device_identity(),
            Some(self.data_dir.clone()),
        );
        Transport {
            to_host: client.to_host,
            from_host: client.from_host,
            state: client.state,
            current_host: Some(client.current_host),
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

    fn discover_origins(&self, host_id: &str) -> HostFuture<'_, Vec<String>> {
        let host_id = host_id.to_owned();
        Box::pin(async move {
            // The transport never downgrades an HTTPS pairing to a plain LAN
            // address, so browsing for one would only cost multicast traffic.
            let plain = self.load_hosts().iter().any(|host| {
                host.host_id == host_id && crate::client::plain_http_origin(&host.origin)
            });
            if !plain {
                return Vec::new();
            }
            self.browse_hosts()
                .await
                .into_iter()
                .filter(|hint| {
                    hint.host_id == host_id && crate::client::plain_http_origin(&hint.origin)
                })
                .map(|hint| hint.origin)
                .collect()
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
        .unwrap_or_else(|| "Tcode".into())
}

/// This operating system's name and version, e.g. `macOS 26.0`,
/// `Windows 10.0.26100` or an `/etc/os-release` pretty name. `None` where the
/// platform bootstrap supplies it instead.
pub fn default_device_platform() -> Option<String> {
    static PLATFORM: OnceLock<Option<String>> = OnceLock::new();
    PLATFORM
        .get_or_init(resolve_default_device_platform)
        .clone()
}

#[cfg(target_os = "macos")]
fn resolve_default_device_platform() -> Option<String> {
    Some(os_with_version("macOS", macos_product_version()))
}

#[cfg(windows)]
fn resolve_default_device_platform() -> Option<String> {
    Some(os_with_version("Windows", windows_version()))
}

#[cfg(target_os = "linux")]
fn resolve_default_device_platform() -> Option<String> {
    Some(
        fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| os_release_pretty_name(&contents))
            .unwrap_or_else(|| "Linux".into()),
    )
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn resolve_default_device_platform() -> Option<String> {
    None
}

#[cfg(any(target_os = "macos", windows))]
fn os_with_version(os: &str, version: Option<String>) -> String {
    match clean_name(version) {
        Some(version) => format!("{os} {version}"),
        None => os.to_owned(),
    }
}

#[cfg(any(target_os = "linux", test))]
fn os_release_pretty_name(contents: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("PRETTY_NAME="))
        .and_then(|value| clean_name(Some(value.trim().trim_matches('"').to_owned())))
}

#[cfg(target_os = "macos")]
fn macos_product_version() -> Option<String> {
    let name = c"kern.osproductversion";
    let mut length = 0_usize;
    // SAFETY: a null buffer asks sysctl for the value's length only.
    let probed = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if probed != 0 || length == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    // SAFETY: `bytes` holds `length` writable bytes and `length` is passed in
    // and out by pointer, so sysctl never writes past the buffer.
    let read = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    bytes.truncate(length.min(bytes.len()));
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).ok()
}

#[cfg(windows)]
fn windows_version() -> Option<String> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOW that outlives the
    // call; RtlGetVersion only fills its fields. Unlike GetVersionEx it is not
    // capped by the application manifest's compatibility declarations.
    if unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(format!(
        "{}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    ))
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

struct FileOutbox {
    data_dir: PathBuf,
    name: String,
}

impl tcode_client::outbox::Storage for FileOutbox {
    fn load(&self) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError> {
        match fs::read(self.data_dir.join(&self.name)) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(tcode_client::outbox::storage_error)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(tcode_client::outbox::storage_error(error)),
        }
    }
    fn save(
        &self,
        entries: &[tcode_client::outbox::Entry],
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let bytes = serde_json::to_vec(entries).map_err(tcode_client::outbox::storage_error)?;
        let temporary = format!("{}.tmp", self.name);
        write_private(&self.data_dir, &temporary, &bytes)
            .and_then(|()| {
                fs::rename(
                    self.data_dir.join(&temporary),
                    self.data_dir.join(&self.name),
                )
            })
            .map_err(tcode_client::outbox::storage_error)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn discovery_hints_are_filtered_to_the_machine_identity_and_plain_origins() {
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
                    DiscoveredHost {
                        host_id: "right".into(),
                        name: "Right".into(),
                        origin: "https://right.example.com".into(),
                    },
                ]
            })
        });
        let mut saved = PairedHost {
            host_id: "right".into(),
            name: "My machine".into(),
            origin: "http://192.168.1.24:47420".into(),
            candidates: Vec::new(),
            token: "unchanged-token".into(),
            identity_key: None,
            last_connected_unix: Some(42),
        };
        client.save_hosts(std::slice::from_ref(&saved));
        assert_eq!(
            smol::block_on(client.discover_origins("right")),
            vec!["http://192.168.1.25:47420".to_owned()]
        );
        assert_eq!(calls.get(), 1);
        assert!(smol::block_on(client.discover_origins("unknown")).is_empty());
        saved.origin = "https://tunnel.example.com".into();
        client.save_hosts(&[saved]);
        assert!(smol::block_on(client.discover_origins("right")).is_empty());
        assert_eq!(calls.get(), 1, "HTTPS must not browse");
    }

    #[test]
    fn connection_stamp_preserves_an_address_update_already_in_progress() {
        let dir = TestDir::new();
        let client = NativeClientHost::new(dir.0.clone(), "phone");
        client.remember_host(PairedHost {
            host_id: "machine".into(),
            name: "Machine".into(),
            origin: "http://192.168.31.5:47420".into(),
            candidates: Vec::new(),
            token: "token".into(),
            identity_key: None,
            last_connected_unix: None,
        });
        let (locked, received) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let transport_dir = dir.0.clone();
        let transport = std::thread::spawn(move || {
            crate::client::update_hosts(&transport_dir, |hosts| {
                hosts[0].promote_origin("http://192.168.1.161:47420");
                locked.send(()).unwrap();
                released.recv().unwrap();
            })
            .unwrap();
        });
        received.recv().unwrap();
        let (started, starting) = std::sync::mpsc::channel();
        let (finished, completion) = std::sync::mpsc::channel();
        let stamp_dir = dir.0.clone();
        let stamp = std::thread::spawn(move || {
            let client = NativeClientHost::new(stamp_dir, "phone");
            started.send(()).unwrap();
            client.stamp_connected("machine", 1234);
            finished.send(()).unwrap();
        });
        starting.recv().unwrap();
        assert!(
            completion
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "the connection stamp must wait for the address transaction"
        );
        release.send(()).unwrap();
        transport.join().unwrap();
        stamp.join().unwrap();
        let saved = client.load_hosts().remove(0);
        assert_eq!(saved.origin, "http://192.168.1.161:47420");
        assert_eq!(saved.candidates, vec!["http://192.168.31.5:47420"]);
        assert_eq!(saved.last_connected_unix, Some(1234));
    }

    #[test]
    fn device_name_uses_the_first_nonempty_source_and_strips_only_local_host_suffix() {
        for (sources, expected) in [
            (
                [
                    Some(" Explicit name "),
                    Some("Friendly Mac"),
                    Some("host.local"),
                    Some("etc-host"),
                ],
                "Explicit name",
            ),
            (
                [Some(" "), Some("Friendly Mac"), Some("host.local"), None],
                "Friendly Mac",
            ),
            (
                [None, Some(""), Some("studio.local"), Some("etc-host")],
                "studio",
            ),
            (
                [None, None, Some("studio.localdomain"), None],
                "studio.localdomain",
            ),
            ([None, None, Some(".local"), Some(" etc-host ")], "etc-host"),
            ([None, None, None, None], "Tcode"),
        ] {
            let [environment, computer, host, etc] = sources.map(|value| value.map(str::to_owned));
            assert_eq!(
                resolve_device_name(environment, computer, host, etc),
                expected,
                "{sources:?}"
            );
        }
    }

    #[test]
    fn os_release_pretty_name_is_unquoted() {
        let os_release = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(
            os_release_pretty_name(os_release).as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(
            os_release_pretty_name("ID=alpine\nPRETTY_NAME=\"\"\n"),
            None
        );
    }

    #[test]
    fn device_id_is_minted_once_and_shared_by_later_instances() {
        let dir = TestDir::new();
        let first = NativeClientHost::new(dir.0.clone(), "phone");
        let id = first.device_id();
        assert!(tcode_client::host::valid_device_id(&id));
        assert_eq!(first.device_id(), id);
        first.set_last_host_id(Some("host"));
        let second = NativeClientHost::new(dir.0.clone(), "phone");
        assert_eq!(second.device_id(), id);
        assert_eq!(second.last_host_id().as_deref(), Some("host"));
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("tcode-native-client-host-{}", uuid::Uuid::new_v4()));
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
