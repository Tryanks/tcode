//! Attached-browser configuration absent from wry's endpoint type. The bridge
//! asks for no credentials: the paired connection is the authority.
use tcode_traverse::preview::ProxyEntry;

#[cfg(not(target_os = "windows"))]
pub(super) fn builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    host: Option<&ProxyEntry>,
) -> Result<wry::WebViewBuilder<'a>, String> {
    let Some(_host) = host else {
        return Ok(builder);
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // Every attached browser gets a separate nonpersistent WK store.
        // Cookies are scoped by hostname, not by our allocated viewing ports.
        Ok(builder.with_incognito(true))
    }
    // lb-wry gives WebKitGTK's ephemeral context a custom proxy with no
    // ignored hosts, so loopback goes through the bridge too.
    #[cfg(target_os = "linux")]
    {
        let origin = url::Url::parse(&_host.origin).map_err(|e| e.to_string())?;
        Ok(builder
            .with_incognito(true)
            .with_proxy_config(wry::ProxyConfig::Http(wry::ProxyEndpoint {
                host: origin.host_str().ok_or("missing proxy host")?.into(),
                port: origin
                    .port_or_known_default()
                    .ok_or("missing proxy port")?
                    .to_string(),
            })))
    }
}

/// WebView2's browser arguments: wry's defaults (no mini menu, no PDF or
/// SmartScreen out-of-process UI), or for an attached browser, which is also
/// in-private, the bridge as its proxy, loopback included.
#[cfg(target_os = "windows")]
pub(super) fn browser_arguments(host: Option<&ProxyEntry>) -> String {
    match host {
        Some(host) => format!(
            "--proxy-server={} --proxy-bypass-list=<-loopback>",
            host.origin
        ),
        None => "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection".into(),
    }
}
