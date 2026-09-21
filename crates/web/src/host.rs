use tcode_client::host::{ClientHost, DeviceIdentity, HostFuture, Transport, persistent_device_id};
use tcode_client::pairing::{PairInvite, PairedHost};
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

/// `tcode.hosts` as the login page writes it: one record per machine with
/// the bearer token the browser presents in hello. The shared
/// [`PairedHost`] carries no token, so the raw records are kept here and
/// merged back on every save.
fn raw_hosts() -> Vec<serde_json::Value> {
    storage()
        .and_then(|storage| storage.get_item("tcode.hosts").ok().flatten())
        .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(&json).ok())
        .unwrap_or_default()
}

fn write_raw_hosts(hosts: &[serde_json::Value]) {
    if let (Some(storage), Ok(json)) = (storage(), serde_json::to_string(hosts)) {
        let _ = storage.set_item("tcode.hosts", &json);
    }
}

/// The token the browser holds for `host_id`, if it logged in there.
pub(crate) fn token_for(host_id: &str) -> Option<String> {
    raw_hosts()
        .iter()
        .find(|record| record["host_id"].as_str() == Some(host_id))
        .and_then(|record| record["token"].as_str())
        .map(str::to_owned)
}

fn user_agent() -> String {
    window().navigator().user_agent().unwrap_or_default()
}

/// The browser family, which is the whole device name: the platform carries
/// the operating system separately.
fn browser_family(user_agent: &str) -> &'static str {
    if user_agent.contains("Edg/") {
        "Edge"
    } else if user_agent.contains("Firefox/") || user_agent.contains("FxiOS/") {
        "Firefox"
    } else if user_agent.contains("Chrome/") || user_agent.contains("CriOS/") {
        "Chrome"
    } else if user_agent.contains("Safari/") {
        "Safari"
    } else {
        "WebKit"
    }
}

/// The operating system named by the user agent. Android must be checked
/// before Linux, and iPadOS Safari reports itself as a Mac.
fn browser_platform(user_agent: &str) -> Option<&'static str> {
    if user_agent.contains("iPhone") || user_agent.contains("iPad") {
        Some("iOS")
    } else if user_agent.contains("Android") {
        Some("Android")
    } else if user_agent.contains("Mac OS X") {
        Some("macOS")
    } else if user_agent.contains("Windows") {
        Some("Windows")
    } else if user_agent.contains("Linux") {
        Some("Linux")
    } else {
        None
    }
}

impl ClientHost for WebHost {
    fn device_name(&self) -> String {
        browser_family(&user_agent()).to_owned()
    }

    /// Shared with the password login page, which stores the same key so a
    /// browser that logs in again keeps its one record on the host.
    fn device_id(&self) -> String {
        let stored =
            storage().and_then(|storage| storage.get_item("tcode.device_id").ok().flatten());
        persistent_device_id(stored, |id| {
            if let Some(storage) = storage() {
                let _ = storage.set_item("tcode.device_id", id);
            }
        })
    }

    fn device_platform(&self) -> Option<String> {
        browser_platform(&user_agent()).map(str::to_owned)
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
        raw_hosts()
            .into_iter()
            .filter_map(|record| serde_json::from_value(record).ok())
            .collect()
    }

    fn save_hosts(&self, hosts: &[PairedHost]) {
        let existing = raw_hosts();
        let records: Vec<serde_json::Value> = hosts
            .iter()
            .filter_map(|host| {
                let mut record = serde_json::to_value(host).ok()?;
                if let Some(token) = existing
                    .iter()
                    .find(|record| record["host_id"].as_str() == Some(host.host_id.as_str()))
                    .and_then(|record| record.get("token"))
                {
                    record["token"] = token.clone();
                }
                Some(record)
            })
            .collect();
        write_raw_hosts(&records);
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

    fn pair(&self, invite: PairInvite) -> HostFuture<'_, Result<PairedHost, String>> {
        let device = self.device_identity();
        Box::pin(async move { pair(&invite.secret, &device).await })
    }

    fn connect(&self, host: &PairedHost) -> Transport {
        crate::transport::connect(
            token_for(&host.host_id).unwrap_or_default(),
            self.device_identity(),
        )
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

/// The `/pair` request body: the code plus the device fields the host reads.
fn pair_body(code: &str, device: &DeviceIdentity) -> String {
    let mut body = serde_json::to_value(device).expect("string fields serialize");
    body["code"] = code.into();
    body.to_string()
}

async fn pair(code: &str, device: &DeviceIdentity) -> Result<PairedHost, String> {
    async fn fetch(code: &str, device: &DeviceIdentity) -> Result<PairedHost, JsValue> {
        let options = web_sys::RequestInit::new();
        options.set_method("POST");
        options.set_body(&JsValue::from_str(&pair_body(code, device)));
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
        let paired = PairedHost {
            host_id: field("host_id")?,
            name: field("host_name")?,
            traverse: None,
            relay: None,
            addrs: Vec::new(),
            last_connected_unix: None,
        };
        // Keep the token where the login page keeps it, next to the record.
        let mut records = raw_hosts();
        records.retain(|record| record["host_id"].as_str() != Some(paired.host_id.as_str()));
        let mut record =
            serde_json::to_value(&paired).map_err(|error| JsValue::from_str(&error.to_string()))?;
        record["token"] = serde_json::Value::String(field("token")?);
        record["origin"] = serde_json::Value::String(WebHost.fixed_pairing_endpoint().unwrap());
        records.push(record);
        write_raw_hosts(&records);
        Ok(paired)
    }
    fetch(code, device)
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
