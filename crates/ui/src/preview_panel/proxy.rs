//! Desktop proxy configuration absent from wry's endpoint type. The bridge
//! asks for no credentials: the paired connection is the authority.
use tcode_traverse::preview::ProxyEntry;

pub(super) fn builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    host: Option<&ProxyEntry>,
) -> Result<wry::WebViewBuilder<'a>, String> {
    let Some(_host) = host else {
        return Ok(builder);
    };
    #[cfg(target_os = "macos")]
    {
        // Every attached browser gets a separate nonpersistent WK store.
        // Cookies are scoped by hostname, not by our allocated viewing ports.
        Ok(builder.with_incognito(true))
    }
    #[cfg(target_os = "windows")]
    {
        use wry::WebViewBuilderExtWindows as _;
        let host = _host;
        let origin = url::Url::parse(&host.origin).map_err(|e| e.to_string())?;
        let builder = builder
            .with_incognito(true)
            .with_proxy_config(wry::ProxyConfig::Http(wry::ProxyEndpoint {
                host: origin.host_str().ok_or("missing proxy host")?.into(),
                port: origin
                    .port_or_known_default()
                    .ok_or("missing proxy port")?
                    .to_string(),
            }));
        // wry's additional arguments replace its generated proxy arguments.
        Ok(builder.with_additional_browser_args(format!(
            "--proxy-server={} --proxy-bypass-list=<-loopback>",
            host.origin
        )))
    }
}
