use tcode_client::host::{ClientHost, HostFuture, PairRequest, Transport};
use tcode_client::pairing::PairedHost;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

pub struct WebHost;

pub(crate) fn window() -> web_sys::Window {
    web_sys::window().expect("tcode-web requires a browser window")
}

pub(crate) fn take_pairing_code() -> Result<Option<String>, String> {
    let window = window();
    let location = window.location();
    let hash = location.hash().map_err(js_error)?;
    let params =
        web_sys::UrlSearchParams::new_with_str(hash.trim_start_matches('#')).map_err(js_error)?;
    let Some(code) = params.get("code") else {
        return Ok(None);
    };
    let clean_url = format!(
        "{}{}",
        location.pathname().map_err(js_error)?,
        location.search().map_err(js_error)?
    );
    window
        .history()
        .and_then(|history| history.replace_state_with_url(&JsValue::NULL, "", Some(&clean_url)))
        .map_err(js_error)?;
    Ok(Some(code))
}

fn js_error(error: JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

impl ClientHost for WebHost {
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

    fn outbox_storage(
        &self,
        host_id: &str,
    ) -> Option<std::sync::Arc<dyn tcode_client::outbox::Storage>> {
        Some(std::sync::Arc::new(WebOutbox(format!(
            "tcode.outbox.{host_id}"
        ))))
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

    fn fixed_pairing_endpoint(&self) -> Option<String> {
        window().location().origin().ok()
    }

    fn pair(&self, request: PairRequest) -> HostFuture<'_, Result<PairedHost, String>> {
        let device_name = self.device_name();
        Box::pin(async move { pair(&request.code, &device_name).await })
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        crate::transport::connect(host.token.clone(), self.device_name())
    }

    fn supports_artifact_delivery(&self) -> bool {
        true
    }

    fn deliver_artifact(&self, name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
        download(name, mime, bytes)
            .map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))
    }
}

/// A browser has no filesystem: the only way to give the user a file is a Blob
/// behind a synthetic download link, revoked as soon as the click is dispatched.
fn download(name: &str, mime: &str, bytes: &[u8]) -> Result<(), JsValue> {
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(bytes).into());
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let document = window()
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let anchor: web_sys::HtmlAnchorElement = document.create_element("a")?.dyn_into()?;
    anchor.set_href(&url);
    anchor.set_download(name);
    anchor.click();
    web_sys::Url::revoke_object_url(&url)?;
    Ok(())
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
        let origin = WebHost.fixed_pairing_endpoint().unwrap();
        Ok(PairedHost {
            host_id: field("host_id")?,
            name: field("host_name")?,
            token: field("token")?,
            origin,
            last_connected_unix: None,
        })
    }
    fetch(code, device_name)
        .await
        .map_err(|error| error.as_string().unwrap_or_else(|| format!("{error:?}")))
}

struct WebOutbox(String);
impl tcode_client::outbox::Storage for WebOutbox {
    fn load(&self) -> Result<Vec<tcode_client::outbox::Entry>, tcode_protocol::ProtocolError> {
        let storage = storage()
            .ok_or_else(|| tcode_client::outbox::storage_error("localStorage unavailable"))?;
        storage
            .get_item(&self.0)
            .map_err(|error| tcode_client::outbox::storage_error(js_error(error)))?
            .map_or(Ok(Vec::new()), |json| {
                serde_json::from_str(&json).map_err(tcode_client::outbox::storage_error)
            })
    }
    fn save(
        &self,
        entries: &[tcode_client::outbox::Entry],
    ) -> Result<(), tcode_protocol::ProtocolError> {
        let storage = storage()
            .ok_or_else(|| tcode_client::outbox::storage_error("localStorage unavailable"))?;
        let json = serde_json::to_string(entries).map_err(tcode_client::outbox::storage_error)?;
        storage
            .set_item(&self.0, &json)
            .map_err(|error| tcode_client::outbox::storage_error(js_error(error)))
    }
}
