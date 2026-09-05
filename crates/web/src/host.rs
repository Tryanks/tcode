use gpui::App;
use tcode_mobile::host::{MobileHost, PairDone, PairRequest, PairedHost, Transport};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

#[derive(Default)]
pub struct WebHost;

impl WebHost {
    pub fn new() -> Self {
        Self
    }
}

pub(crate) fn window() -> web_sys::Window {
    web_sys::window().expect("tcode-web requires a browser window")
}

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

impl MobileHost for WebHost {
    fn device_name(&self) -> String {
        let ua = window().navigator().user_agent().unwrap_or_default();
        let family = if ua.contains("Edg/") {
            "Edge"
        } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
            "Firefox"
        } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
            "Chrome"
        } else if ua.contains("Safari/") {
            "Safari"
        } else {
            "WebKit"
        };
        format!("Browser ({family})")
    }

    fn load_hosts(&self) -> Vec<PairedHost> {
        storage()
            .and_then(|storage| storage.get_item("tcode.hosts").ok().flatten())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    fn save_hosts(&self, hosts: &[PairedHost]) {
        if let (Some(storage), Ok(json)) = (storage(), serde_json::to_string(hosts)) {
            let _ = storage.set_item("tcode.hosts", &json);
        }
    }

    fn last_host_id(&self) -> Option<String> {
        storage().and_then(|storage| storage.get_item("tcode.last_host").ok().flatten())
    }

    fn set_last_host_id(&self, host_id: Option<&str>) {
        if let Some(storage) = storage() {
            let _ = match host_id {
                Some(id) => storage.set_item("tcode.last_host", id),
                None => storage.remove_item("tcode.last_host"),
            };
        }
    }

    fn fixed_pairing_endpoint(&self) -> Option<(String, u16)> {
        let location = window().location();
        Some((
            location.hostname().unwrap_or_default(),
            location
                .port()
                .ok()
                .and_then(|port| port.parse().ok())
                .unwrap_or(if location.protocol().ok().as_deref() == Some("https:") {
                    443
                } else {
                    80
                }),
        ))
    }

    fn pair(&self, request: PairRequest, cx: &mut App, done: PairDone) {
        let device_name = self.device_name();
        // Fetch is a browser future; the callback must re-enter GPUI through
        // its foreground executor, never from a WebSocket/Promise callback.
        cx.spawn(async move |cx| {
            let result = pair(&request.code, &device_name).await;
            cx.update(|cx| done(result, cx));
        })
        .detach();
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        crate::transport::connect(host.token.clone(), self.device_name())
    }
}

async fn pair(code: &str, device_name: &str) -> Result<PairedHost, String> {
    async fn fetch(code: &str, device_name: &str) -> Result<PairedHost, JsValue> {
        let options = web_sys::RequestInit::new();
        options.set_method("POST");
        options.set_body(&JsValue::from_str(
            &serde_json::json!({"code": code, "device_name": device_name}).to_string(),
        ));
        let request = web_sys::Request::new_with_str_and_init("/pair", &options)?;
        request.headers().set("Content-Type", "application/json")?;
        let response: web_sys::Response = JsFuture::from(window().fetch_with_request(&request))
            .await?
            .dyn_into()?;
        if !response.ok() {
            return Err(JsValue::from_str(&format!(
                "Pairing failed (HTTP {})",
                response.status()
            )));
        }
        let text = JsFuture::from(response.text()?)
            .await?
            .as_string()
            .unwrap_or_default();
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| JsValue::from_str(&error.to_string()))?;
        let field = |key: &str| {
            value[key]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| JsValue::from_str(&format!("Pairing response missing {key}")))
        };
        let (addr, port) = WebHost.fixed_pairing_endpoint().unwrap();
        Ok(PairedHost {
            host_id: field("host_id")?,
            name: field("host_name")?,
            token: field("token")?,
            fingerprint: field("fp")?,
            addrs: vec![addr],
            port,
            last_connected_unix: None,
        })
    }
    fetch(code, device_name)
        .await
        .map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))
}
