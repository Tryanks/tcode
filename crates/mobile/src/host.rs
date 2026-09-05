//! The platform seam of the phone/browser client.
//!
//! Everything the screens need from the outside world goes through
//! [`MobileHost`]: persisted paired hosts, pairing, the transport to a host,
//! the camera, and safe-area insets. `tcode-mobile` itself never touches a
//! socket or the filesystem, so the same screens run on iOS, Android, the
//! desktop preview window, and (with a wasm implementation) in a browser.

use std::rc::Rc;

use gpui::{App, Edges, Pixels};
pub use tcode_client::ConnectionState;
pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, pair_url, parse_pair_url,
};

/// A live link to a host: NDJSON lines in both directions plus the
/// connection state stream. Wrap `to_host`/`from_host` in a
/// `tcode_client::HostLink` and feed `state` into `set_connection_state`.
pub struct Transport {
    pub to_host: async_channel::Sender<String>,
    pub from_host: async_channel::Receiver<String>,
    pub state: async_channel::Receiver<ConnectionState>,
}

/// What the pairing form submits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub addr: String,
    pub port: u16,
    pub code: String,
}

/// Delivered on the main thread once pairing finished.
pub type PairDone = Box<dyn FnOnce(Result<PairedHost, String>, &mut App) + 'static>;
/// Delivered on the main thread with the decoded QR text.
pub type ScanDone = Box<dyn FnOnce(Result<String, String>, &mut App) + 'static>;

/// Phone-only preferences; never forwarded to the execution host.
#[derive(Debug, Clone, Default)]
pub struct MobilePreferences {
    pub appearance: Option<String>,
    pub language: Option<String>,
    pub device_name: Option<String>,
}

pub trait MobileHost: 'static {
    /// Name this device presents to hosts while pairing and connecting.
    fn device_name(&self) -> String;

    fn preferences(&self) -> MobilePreferences {
        MobilePreferences::default()
    }
    fn save_preferences(&self, _preferences: &MobilePreferences) {}

    fn load_hosts(&self) -> Vec<PairedHost>;
    fn save_hosts(&self, hosts: &[PairedHost]);

    /// `Some(id)` of the host to reconnect to on launch.
    fn last_host_id(&self) -> Option<String>;
    fn set_last_host_id(&self, host_id: Option<&str>);

    /// When `Some((addr, port))`, this client can only pair with that one
    /// endpoint (a browser page can only reach its own origin): the pairing
    /// form hides the address and port fields and asks for the code alone.
    fn fixed_pairing_endpoint(&self) -> Option<(String, u16)> {
        None
    }

    /// Pair with a host; `done` runs on the main thread.
    fn pair(&self, request: PairRequest, cx: &mut App, done: PairDone);

    /// Open a reconnecting link to a paired host. Dropping the returned
    /// channels ends the link.
    fn connect(&self, host: &PairedHost) -> Transport;

    fn supports_qr(&self) -> bool {
        false
    }

    /// Start the platform QR scanner; the default has no camera.
    fn scan_qr(&self, done: ScanDone, cx: &mut App) {
        done(Err("unsupported".into()), cx);
    }

    /// Status bar / home indicator / display cutout insets in logical pixels.
    fn safe_area(&self) -> Edges<Pixels> {
        Edges::default()
    }
}

pub type SharedHost = Rc<dyn MobileHost>;

#[cfg(feature = "native")]
pub use native::NativeHost;

#[cfg(feature = "native")]
mod native {
    use std::path::PathBuf;

    use gpui::{App, Edges, Pixels};

    use super::{MobileHost, MobilePreferences, PairDone, PairRequest, PairedHost, Transport};

    /// iOS, Android, and the desktop preview: `tcode-remote`'s WebSocket
    /// client, `hosts.json` and `mobile.json` under the platform data dir.
    pub struct NativeHost {
        data_dir: PathBuf,
        device_name: String,
    }

    impl NativeHost {
        pub fn new(data_dir: PathBuf, device_name: String) -> Self {
            Self {
                data_dir,
                device_name,
            }
        }

        /// `TCODE_DATA_DIR`, else the platform data dir; hostname as device name.
        pub fn from_env() -> Self {
            let data_dir = match std::env::var_os("TCODE_DATA_DIR") {
                Some(dir) => PathBuf::from(dir),
                None => dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("tcode"),
            };
            let device_name = ["HOSTNAME", "HOST", "COMPUTERNAME"]
                .iter()
                .filter_map(|key| std::env::var(key).ok())
                .map(|name| name.trim().to_owned())
                .find(|name| !name.is_empty())
                .unwrap_or_else(|| "tcode phone".into());
            Self::new(data_dir, device_name)
        }

        fn prefs_path(&self) -> PathBuf {
            self.data_dir.join("mobile.json")
        }

        fn prefs(&self) -> serde_json::Value {
            std::fs::read(self.prefs_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_else(|| serde_json::json!({}))
        }

        fn write_prefs(&self, prefs: &serde_json::Value) {
            let _ = std::fs::create_dir_all(&self.data_dir);
            if let Ok(bytes) = serde_json::to_vec_pretty(prefs) {
                let _ = std::fs::write(self.prefs_path(), bytes);
            }
        }
    }

    impl MobileHost for NativeHost {
        fn device_name(&self) -> String {
            self.preferences()
                .device_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| self.device_name.clone())
        }

        fn preferences(&self) -> MobilePreferences {
            let prefs = self.prefs();
            let value = |key: &str| prefs.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            MobilePreferences {
                appearance: value("appearance"),
                language: value("language"),
                device_name: value("device_name"),
            }
        }

        fn save_preferences(&self, preferences: &MobilePreferences) {
            let mut prefs = self.prefs();
            prefs["appearance"] = serde_json::json!(preferences.appearance);
            prefs["language"] = serde_json::json!(preferences.language);
            prefs["device_name"] = serde_json::json!(preferences.device_name);
            self.write_prefs(&prefs);
        }

        fn load_hosts(&self) -> Vec<PairedHost> {
            tcode_remote::client::load_hosts(&self.data_dir).unwrap_or_else(|error| {
                log::warn!("could not read hosts.json: {error}");
                Vec::new()
            })
        }

        fn save_hosts(&self, hosts: &[PairedHost]) {
            if let Err(error) = tcode_remote::client::save_hosts(&self.data_dir, hosts) {
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

        fn pair(&self, request: PairRequest, cx: &mut App, done: PairDone) {
            let device_name = self.device_name();
            let task = cx.background_executor().spawn(async move {
                tcode_remote::client::pair(&request.addr, request.port, &request.code, &device_name)
            });
            cx.spawn(async move |cx| {
                let result = task.await;
                cx.update(|cx| done(result, cx));
            })
            .detach();
        }

        fn connect(&self, host: &PairedHost) -> Transport {
            let client = tcode_remote::client::connect(host.clone(), self.device_name());
            Transport {
                to_host: client.to_host,
                from_host: client.from_host,
                state: client.state,
            }
        }

        fn safe_area(&self) -> Edges<Pixels> {
            #[cfg(target_os = "ios")]
            {
                gpui_ios::safe_area()
            }
            #[cfg(target_os = "android")]
            {
                gpui_android::safe_area()
            }
            #[cfg(not(any(target_os = "ios", target_os = "android")))]
            {
                Edges::default()
            }
        }
    }
}
