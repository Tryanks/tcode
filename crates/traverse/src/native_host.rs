//! Native implementation of the transport-agnostic client host contract,
//! shared by the desktop, iOS and Android bootstraps.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tcode_client::host::{
    ClientHost, ClientPreferences, DeviceFailure, FileFailure, HostFuture, PickedFile, Transport,
};
use tcode_client::pairing::{PairInvite, PairedHost};

use crate::{
    identity::DeviceIdentity,
    lan::{Browse, LanOptions, SystemBrowser},
};

type QrScanner = dyn Fn() -> HostFuture<'static, Result<Option<String>, DeviceFailure>>;
type ImagePicker = dyn Fn(usize) -> HostFuture<'static, Result<Vec<PickedFile>, DeviceFailure>>;
type FilePicker = dyn Fn() -> HostFuture<'static, Result<Vec<PickedFile>, DeviceFailure>>;
type EditorOpener = dyn Fn(&Path) -> Result<(), String>;
type MulticastLock = dyn Fn(bool) + Send + Sync;

/// Native clients share hosts.json, mobile.json, device.json, pairing, and
/// transport policy.
pub struct NativeClientHost {
    speech: Option<std::rc::Rc<dyn tcode_client::speech::SpeechRecognizer>>,
    words: Option<std::rc::Rc<dyn tcode_client::words::WordSegmenter>>,
    data_dir: PathBuf,
    default_device_name: String,
    platform: Option<String>,
    qr_scanner: Option<Box<QrScanner>>,
    image_picker: Option<Box<ImagePicker>>,
    file_picker: Option<Box<FilePicker>>,
    editor: Option<Box<EditorOpener>>,
    machine_editor: Option<std::rc::Rc<dyn tcode_client::host::MachineEditor>>,
    multicast_lock: Option<Arc<MulticastLock>>,
    system_browser: Option<Arc<SystemBrowser>>,
    device: OnceLock<Result<DeviceIdentity, String>>,
}

impl NativeClientHost {
    /// The platform starts as this operating system's description; mobile
    /// bootstraps replace it with [`NativeClientHost::with_platform`].
    pub fn new(data_dir: PathBuf, device_name: impl Into<String>) -> Self {
        Self {
            speech: None,
            words: None,
            data_dir,
            default_device_name: device_name.into(),
            platform: default_device_platform(),
            qr_scanner: None,
            image_picker: None,
            file_picker: None,
            editor: None,
            machine_editor: None,
            multicast_lock: None,
            system_browser: None,
            device: OnceLock::new(),
        }
    }

    /// The device's Traverse identity, carrying the current name and platform.
    fn device(&self) -> Result<DeviceIdentity, String> {
        let device = self
            .device
            .get_or_init(|| {
                let device = DeviceIdentity::load_or_create(&self.data_dir).map_err(|error| {
                    format!("could not open {}: {error}", crate::identity::DEVICE_FILE)
                })?;
                device.set_lan_options(LanOptions {
                    browse: match &self.system_browser {
                        Some(browser) => Browse::System(browser.clone()),
                        None => Browse::DnsSd,
                    },
                    multicast_lock: self.multicast_lock.clone(),
                });
                Ok(device)
            })
            .clone()?;
        device.set_details(self.device_name(), self.device_platform());
        Ok(device)
    }

    /// The platform saw connectivity change or the app return to the
    /// foreground: rebind the device endpoint's paths and probe every live
    /// transport now. Before the first connection there is nothing to
    /// notify, so the identity is not loaded for this.
    pub fn network_changed(&self) {
        if let Some(Ok(device)) = self.device.get() {
            device.network_changed();
        }
    }

    /// The saved machines changed: the device endpoint, if it is up, follows
    /// their Traverse instances.
    fn hosts_changed(&self) {
        if let Some(Ok(device)) = self.device.get() {
            device.hosts_changed();
        }
    }

    /// Operating system name and version reported to hosts, e.g. `Android 15`.
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    pub fn with_qr_scanner(
        mut self,
        scanner: impl Fn() -> HostFuture<'static, Result<Option<String>, DeviceFailure>> + 'static,
    ) -> Self {
        self.qr_scanner = Some(Box::new(scanner));
        self
    }

    /// Segment words with `segmenter` instead of what the client finds on
    /// its own.
    pub fn with_word_segmenter(
        mut self,
        segmenter: impl tcode_client::words::WordSegmenter + 'static,
    ) -> Self {
        self.words = Some(std::rc::Rc::new(segmenter));
        self
    }

    pub fn with_speech_recognizer(
        mut self,
        recognizer: impl tcode_client::speech::SpeechRecognizer + 'static,
    ) -> Self {
        self.speech = Some(std::rc::Rc::new(recognizer));
        self
    }

    /// The platform's own media picker; see [`ClientHost::pick_images`].
    pub fn with_image_picker(
        mut self,
        picker: impl Fn(usize) -> HostFuture<'static, Result<Vec<PickedFile>, DeviceFailure>> + 'static,
    ) -> Self {
        self.image_picker = Some(Box::new(picker));
        self
    }

    /// The platform's own file picker; see [`ClientHost::pick_files`].
    pub fn with_file_picker(
        mut self,
        picker: impl Fn() -> HostFuture<'static, Result<Vec<PickedFile>, DeviceFailure>> + 'static,
    ) -> Self {
        self.file_picker = Some(Box::new(picker));
        self
    }

    /// Held around every DNS-SD browse: Android delivers multicast to an
    /// app only while it holds the Wi-Fi multicast lock. Called from the
    /// transport's threads.
    pub fn with_multicast_lock(mut self, lock: impl Fn(bool) + Send + Sync + 'static) -> Self {
        self.multicast_lock = Some(Arc::new(lock));
        self
    }

    /// Browse DNS-SD through the platform instead of a socket of our own;
    /// iOS reserves raw multicast for entitled apps.
    pub fn with_system_browser(mut self, browser: Arc<SystemBrowser>) -> Self {
        self.system_browser = Some(browser);
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

    /// Supply the editor that opens paired machines' directories; see
    /// [`ClientHost::machine_editor`]. Injected for the same reason as
    /// [`Self::with_editor_opener`].
    pub fn with_machine_editor(
        mut self,
        editor: impl tcode_client::host::MachineEditor + 'static,
    ) -> Self {
        self.machine_editor = Some(std::rc::Rc::new(editor));
        self
    }

    fn prefs_path(&self) -> PathBuf {
        self.data_dir.join(PREFS_FILE)
    }

    /// The saved preferences; a missing file is none.
    fn prefs(&self) -> Result<serde_json::Value, FileFailure> {
        let read = match fs::read(self.prefs_path()) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(io::Error::other)
                .and_then(|value| {
                    if value.is_object() {
                        Ok(value)
                    } else {
                        Err(io::Error::new(io::ErrorKind::InvalidData, "not an object"))
                    }
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
            Err(error) => Err(error),
        };
        read.map_err(|error| FileFailure::new(PREFS_FILE, error))
    }

    /// Change the saved preferences. A file that cannot be read is left as
    /// it is rather than replaced by the change alone.
    fn update_prefs(&self, update: impl FnOnce(&mut serde_json::Value)) -> Result<(), FileFailure> {
        let mut prefs = self.prefs()?;
        update(&mut prefs);
        serde_json::to_vec_pretty(&prefs)
            .map_err(io::Error::other)
            .and_then(|bytes| write_private(&self.data_dir, &self.prefs_path(), &bytes))
            .map_err(|error| FileFailure::new(PREFS_FILE, error))
    }
}

const PREFS_FILE: &str = "mobile.json";
const HOSTS_FILE: &str = "hosts.json";

impl ClientHost for NativeClientHost {
    fn speech_recognizer(&self) -> Option<std::rc::Rc<dyn tcode_client::speech::SpeechRecognizer>> {
        self.speech.clone()
    }

    fn word_segmenter(&self) -> Option<std::rc::Rc<dyn tcode_client::words::WordSegmenter>> {
        self.words.clone().or_else(tcode_client::words::system)
    }

    /// The default name while the preferences cannot be read; the window
    /// reports that file.
    fn device_name(&self) -> String {
        self.load_preferences()
            .ok()
            .and_then(|preferences| preferences.device_name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.default_device_name.clone())
    }

    /// The device's `EndpointId`; the machine authenticates it on every
    /// connection.
    fn device_id(&self) -> String {
        self.device()
            .map(|device| device.endpoint_id().to_string())
            .unwrap_or_else(|error| {
                log::error!("{error}");
                String::new()
            })
    }

    fn device_platform(&self) -> Option<String> {
        self.platform.clone()
    }

    /// The whole record: a value of the wrong kind, or one this build does
    /// not know, makes it unreadable. Keys it has no field for are left to
    /// whoever wrote them.
    fn load_preferences(&self) -> Result<ClientPreferences, FileFailure> {
        serde_json::from_value(self.prefs()?).map_err(|error| FileFailure::new(PREFS_FILE, error))
    }

    fn save_preferences(&self, preferences: &ClientPreferences) -> Result<(), FileFailure> {
        self.load_preferences()?;
        self.update_prefs(|prefs| {
            prefs["appearance"] = serde_json::json!(preferences.appearance);
            prefs["language"] = serde_json::json!(preferences.language);
            prefs["device_name"] = serde_json::json!(preferences.device_name);
            prefs["navigation"] = serde_json::json!(preferences.navigation);
            prefs["remote_attachment_limit_mib"] =
                serde_json::json!(preferences.remote_attachment_limit_mib);
            prefs["right_panel_width"] = serde_json::json!(preferences.right_panel_width);
            prefs["sidebar_collapsed"] = serde_json::json!(preferences.sidebar_collapsed);
            prefs["sidebar_layout"] = serde_json::json!(preferences.sidebar_layout);
            prefs["workspace"] = serde_json::json!(preferences.workspace);
        })
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

    fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
        crate::hosts::load_hosts(&self.data_dir)
            .map_err(|error| FileFailure::new(HOSTS_FILE, error))
    }

    fn save_hosts(&self, hosts: &[PairedHost]) -> Result<(), FileFailure> {
        let saved = crate::hosts::save_hosts(&self.data_dir, hosts)
            .map_err(|error| FileFailure::new(HOSTS_FILE, error));
        self.hosts_changed();
        saved
    }

    fn saved_hosts_changes(&self) -> Option<async_channel::Receiver<()>> {
        Some(crate::hosts::watch(&self.data_dir))
    }

    fn remember_host(&self, host: PairedHost) -> Result<(), FileFailure> {
        let saved = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            tcode_client::pairing::remember_host(hosts, host);
        })
        .map_err(|error| FileFailure::new(HOSTS_FILE, error));
        self.hosts_changed();
        saved
    }

    fn remove_host(&self, host_id: &str) -> Result<(), FileFailure> {
        let removed = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            hosts.retain(|host| host.host_id != host_id);
        })
        .map_err(|error| FileFailure::new(HOSTS_FILE, error));
        self.hosts_changed();
        removed
    }

    fn stamp_connected(&self, host_id: &str, timestamp: u64) {
        if let Err(error) = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            if let Some(host) = hosts.iter_mut().find(|host| host.host_id == host_id) {
                host.last_connected_unix = Some(timestamp);
            }
        }) {
            log::error!("could not record machine connection: {error}");
        }
    }

    fn local_machine_id(&self) -> Option<String> {
        crate::identity::existing_machine_id(&self.data_dir).map(|id| id.to_string())
    }

    /// None while mobile.json cannot be read; the window reports that file.
    fn last_host_id(&self) -> Option<String> {
        self.prefs()
            .ok()?
            .get("last_host_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        let saved = self.update_prefs(|prefs| {
            prefs["last_host_id"] = match host_id {
                Some(id) => serde_json::Value::String(id.to_owned()),
                None => serde_json::Value::Null,
            };
        });
        if let Err(error) = saved {
            log::error!("could not save the machine to reopen: {error}");
        }
    }

    /// Pairing runs on the Traverse runtime; the UI executor only awaits the
    /// result.
    fn pair(
        &self,
        invite: PairInvite,
        address: Option<std::net::IpAddr>,
    ) -> HostFuture<'_, Result<PairedHost, crate::PairError>> {
        let device = self.device();
        Box::pin(async move {
            let device = device.map_err(crate::PairError::Device)?;
            let (done, result) = async_channel::bounded(1);
            crate::runtime().spawn(async move {
                let _ = done
                    .send(crate::pair(&invite, address, &device).await)
                    .await;
            });
            result
                .recv()
                .await
                .unwrap_or_else(|error| Err(crate::PairError::Lost(error.to_string())))
        })
    }

    fn open_in_editor(&self, path: &Path) -> Option<Result<(), String>> {
        self.editor.as_ref().map(|open| open(path))
    }

    fn machine_editor(&self) -> Option<std::rc::Rc<dyn tcode_client::host::MachineEditor>> {
        self.machine_editor.clone()
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        match self.device() {
            Ok(device) => crate::connect(host, &device),
            Err(error) => {
                // A link that reports itself offline instead of a panic in
                // the shell; the profile directory is the thing to fix.
                log::error!("{error}");
                let (to_host, _) = async_channel::unbounded();
                let (_, from_host) = async_channel::unbounded();
                let (state_tx, state) = async_channel::unbounded();
                let _ = state_tx.try_send(tcode_client::ConnectionState::Offline {
                    reason: tcode_client::ConnectionFailure::Unreachable(Some(error.to_string())),
                });
                Transport {
                    originals: None,
                    to_host: to_host.into(),
                    from_host,
                    state,
                    current_host: None,
                }
            }
        }
    }

    fn supports_qr(&self) -> bool {
        self.qr_scanner.is_some()
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<Option<String>, DeviceFailure>> {
        self.qr_scanner.as_ref().map_or_else(
            || Box::pin(async { Err(DeviceFailure::Unsupported) }) as HostFuture<'_, _>,
            |scanner| scanner(),
        )
    }

    fn supports_image_picker(&self) -> bool {
        self.image_picker.is_some()
    }

    fn pick_images(&self, limit: usize) -> HostFuture<'_, Result<Vec<PickedFile>, DeviceFailure>> {
        self.image_picker.as_ref().map_or_else(
            || Box::pin(async { Err(DeviceFailure::Unsupported) }) as HostFuture<'_, _>,
            |picker| picker(limit),
        )
    }

    fn supports_file_picker(&self) -> bool {
        self.file_picker.is_some()
    }

    fn pick_files(&self) -> HostFuture<'_, Result<Vec<PickedFile>, DeviceFailure>> {
        self.file_picker.as_ref().map_or_else(
            || Box::pin(async { Err(DeviceFailure::Unsupported) }) as HostFuture<'_, _>,
            |picker| picker(),
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

#[cfg(target_os = "linux")]
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

/// Replace `path` in `data_dir` whole, as the other files of the profile are.
fn write_private(data_dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;
    crate::identity::write_private(path, bytes)
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
        write_private(&self.data_dir, &self.data_dir.join(&self.name), &bytes)
            .map_err(tcode_client::outbox::storage_error)
    }
}

#[cfg(test)]
mod tests {

    use tcode_core::settings::{SidebarLayout, ThemeMode};

    use super::*;

    const MACHINE_ID: &str = "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf";

    #[test]
    fn connection_stamp_preserves_an_address_update_already_in_progress() {
        let dir = TestDir::new();
        let client = NativeClientHost::new(dir.0.clone(), "phone");
        client
            .remember_host(PairedHost {
                space_id: None,
                space_name: None,
                host_id: MACHINE_ID.into(),
                name: "Machine".into(),
                traverse: Vec::new(),
                relay: None,
                addrs: vec!["192.168.31.5:47420".into()],
                last_connected_unix: None,
                group_id: None,
            })
            .unwrap();
        let (locked, received) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let transport_dir = dir.0.clone();
        let transport = std::thread::spawn(move || {
            crate::hosts::update_hosts(&transport_dir, |hosts| {
                hosts[0].addrs.insert(0, "192.168.1.161:47420".into());
                locked.send(()).unwrap();
                released.recv().unwrap();
            })
            .unwrap();
        });
        received.recv().unwrap();
        let (started, starting) = std::sync::mpsc::channel();
        let stamp_dir = dir.0.clone();
        let stamp = std::thread::spawn(move || {
            let client = NativeClientHost::new(stamp_dir, "phone");
            started.send(()).unwrap();
            client.stamp_connected(MACHINE_ID, 1234);
        });
        starting.recv().unwrap();
        release.send(()).unwrap();
        transport.join().unwrap();
        stamp.join().unwrap();
        let saved = client.load_hosts().unwrap().remove(0);
        assert_eq!(
            saved.addrs,
            vec!["192.168.1.161:47420", "192.168.31.5:47420"]
        );
        assert_eq!(saved.last_connected_unix, Some(1234));
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "tcode-native-client-host-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    fn unreadable_preferences_are_not_replaced_by_a_save() {
        let dir = TestDir::new();
        let damaged = br#"{"appearance": "dark","#;
        fs::write(dir.0.join("mobile.json"), damaged).unwrap();
        let host = NativeClientHost::new(dir.0.clone(), "fallback");
        let unreadable = host.load_preferences().unwrap_err();
        assert_eq!(unreadable.file, "mobile.json");
        assert_eq!(
            host.save_preferences(&ClientPreferences {
                appearance: Some(ThemeMode::Light),
                ..Default::default()
            }),
            Err(unreadable)
        );
        host.set_last_host_id(Some(MACHINE_ID));
        assert_eq!(fs::read(dir.0.join("mobile.json")).unwrap(), damaged);
    }

    /// Every appearance and sidebar layout a release wrote to mobile.json is
    /// read: v0.1.51 wrote "light", "dark" or null, v0.4.0 also "system"
    /// and the layouts "flat" and "grouped". A value this build does not
    /// know, or one of the wrong kind, makes the record unreadable, and a
    /// save neither replaces nor nulls it.
    #[test]
    fn preferences_releases_wrote_are_read_and_others_are_kept_unread() {
        for (written, appearance, layout) in [
            (r#"{"appearance": null, "language": null}"#, None, None),
            (r#"{"appearance": "light"}"#, Some(ThemeMode::Light), None),
            (r#"{"appearance": "dark"}"#, Some(ThemeMode::Dark), None),
            (
                r#"{"appearance": "system", "sidebar_layout": "flat"}"#,
                Some(ThemeMode::System),
                Some(SidebarLayout::Flat),
            ),
            (
                r#"{"sidebar_layout": "grouped"}"#,
                None,
                Some(SidebarLayout::Grouped),
            ),
        ] {
            let dir = TestDir::new();
            fs::write(dir.0.join("mobile.json"), written).unwrap();
            let read = NativeClientHost::new(dir.0.clone(), "phone")
                .load_preferences()
                .unwrap();
            assert_eq!((read.appearance, read.sidebar_layout), (appearance, layout));
        }
        for unknown in [
            r#"{"appearance": "sepia", "language": "ja"}"#,
            r#"{"sidebar_layout": "tree"}"#,
            r#"{"right_panel_width": "wide"}"#,
        ] {
            let dir = TestDir::new();
            fs::write(dir.0.join("mobile.json"), unknown).unwrap();
            let host = NativeClientHost::new(dir.0.clone(), "phone");
            assert_eq!(host.load_preferences().unwrap_err().file, "mobile.json");
            assert!(
                host.save_preferences(&ClientPreferences::default())
                    .is_err()
            );
            assert_eq!(
                fs::read_to_string(dir.0.join("mobile.json")).unwrap(),
                unknown
            );
        }
    }

    /// A hosts.json this build cannot read is reported, not shown as no
    /// machines, and neither pairing nor forgetting writes over it.
    #[test]
    fn unreadable_saved_machines_are_reported_and_kept() {
        let dir = TestDir::new();
        let damaged = br#"[{"host_id": 7}]"#;
        fs::write(dir.0.join("hosts.json"), damaged).unwrap();
        let host = NativeClientHost::new(dir.0.clone(), "phone");
        let unreadable = host.load_hosts().unwrap_err();
        assert_eq!(unreadable.file, "hosts.json");
        let machine = PairedHost {
            space_id: None,
            space_name: None,
            host_id: MACHINE_ID.into(),
            name: "Machine".into(),
            traverse: Vec::new(),
            relay: None,
            addrs: Vec::new(),
            last_connected_unix: None,
            group_id: None,
        };
        assert_eq!(host.remember_host(machine).unwrap_err().file, "hosts.json");
        assert_eq!(host.remove_host(MACHINE_ID).unwrap_err().file, "hosts.json");
        host.stamp_connected(MACHINE_ID, 1);
        assert_eq!(fs::read(dir.0.join("hosts.json")).unwrap(), damaged);
    }

    #[test]
    fn mobile_preferences_preserve_device_identity_last_host_and_unknown_fields() {
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
        let host = NativeClientHost::new(dir.0.clone(), "fallback").with_platform("Android 15");

        assert_eq!(
            host.load_preferences().unwrap(),
            ClientPreferences {
                appearance: Some(ThemeMode::Dark),
                language: Some("zh-CN".into()),
                device_name: Some("My phone".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            host.last_host_id().as_deref(),
            Some("host-before-client-seam")
        );
        let device_id = host.device_id();
        assert!(tcode_client::host::valid_device_id(&device_id));
        assert_eq!(host.device_id(), device_id);
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("device.json")).unwrap()).unwrap();
        assert_eq!(identity["v"], 1);
        assert_eq!(identity["name"], "My phone");
        assert_eq!(identity["platform"], "Android 15");
        host.save_preferences(&ClientPreferences {
            appearance: Some(ThemeMode::Light),
            language: None,
            device_name: Some("Renamed".into()),
            navigation: Some(serde_json::json!({"history": ["hosts", "threads"]})),
            remote_attachment_limit_mib: Some(4),
            right_panel_width: Some(480),
            sidebar_collapsed: Some(true),
            sidebar_layout: Some(SidebarLayout::Grouped),
            workspace: Some(tcode_client::host::WorkspacePreference {
                hosts: vec![
                    tcode_client::host::WorkspaceHost {
                        host_id: "local".into(),
                        collapsed: false,
                    },
                    tcode_client::host::WorkspaceHost {
                        host_id: "studio".into(),
                        collapsed: true,
                    },
                ],
                left: vec!["attic".into()],
            }),
        })
        .unwrap();

        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("mobile.json")).unwrap()).unwrap();
        assert_eq!(saved["last_host_id"], "host-before-client-seam");
        assert_eq!(saved["future_field"]["preserve"], true);
        assert_eq!(saved["appearance"], "light");
        assert_eq!(
            host.load_preferences().unwrap().navigation.unwrap()["history"],
            serde_json::json!(["hosts", "threads"])
        );
        assert!(saved["language"].is_null());
        assert_eq!(saved["remote_attachment_limit_mib"], 4);
        assert_eq!(
            host.load_preferences().unwrap().remote_attachment_limit_mib,
            Some(4)
        );
        assert_eq!(saved["right_panel_width"], 480);
        assert_eq!(
            host.load_preferences().unwrap().right_panel_width,
            Some(480)
        );
        assert_eq!(saved["sidebar_collapsed"], true);
        assert_eq!(saved["sidebar_layout"], "grouped");
        assert_eq!(
            host.load_preferences().unwrap().sidebar_collapsed,
            Some(true)
        );
        assert_eq!(
            host.load_preferences().unwrap().sidebar_layout,
            Some(SidebarLayout::Grouped)
        );
        assert_eq!(
            saved["workspace"],
            serde_json::json!({
                "hosts": [{"host_id": "local"}, {"host_id": "studio", "collapsed": true}],
                "left": ["attic"]
            })
        );
        assert_eq!(
            host.load_preferences()
                .unwrap()
                .workspace
                .map(|workspace| workspace.hosts.len()),
            Some(2)
        );

        host.set_last_host_id(Some("next-host"));
        assert_eq!(host.last_host_id().as_deref(), Some("next-host"));
        assert_eq!(host.device_name(), "Renamed");
        assert_eq!(host.device_id(), device_id);
        // Check the write before reopening can refresh the identity's details.
        let identity: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.0.join("device.json")).unwrap()).unwrap();
        assert_eq!(identity["v"], 1);
        assert_eq!(identity["name"], "Renamed");
        assert_eq!(identity["platform"], "Android 15");
        let loaded = DeviceIdentity::load_or_create(&dir.0).unwrap();
        assert_eq!(loaded.endpoint_id().to_string(), device_id);
        assert_eq!(loaded.claim().name, "Renamed");
        assert_eq!(loaded.claim().platform.as_deref(), Some("Android 15"));
        let reopened = NativeClientHost::new(dir.0.clone(), "fallback").with_platform("Android 15");
        assert_eq!(reopened.device_id(), device_id);
        assert_eq!(reopened.last_host_id().as_deref(), Some("next-host"));
        assert_eq!(reopened.device_name(), "Renamed");
    }
}
