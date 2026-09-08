//! Desktop proxy configuration and platform authentication absent from wry's endpoint type.
use tcode_client::pairing::PairedHost;

pub(super) fn builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    host: Option<&PairedHost>,
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
        if origin.scheme() != "http" {
            return Err("remote desktop preview requires a direct HTTP machine origin".into());
        }
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

#[cfg(target_os = "windows")]
pub(super) fn authenticate(raw: &wry::WebView, host: Option<&PairedHost>) -> Result<(), String> {
    use webview2_com::{
        BasicAuthenticationRequestedEventHandler,
        Microsoft::Web::WebView2::Win32::ICoreWebView2_10, take_pwstr,
    };
    use windows_core::{HSTRING, Interface as _, PWSTR};
    use wry::WebViewExtWindows as _;
    let Some(host) = host.cloned() else {
        return Ok(());
    };
    let webview = raw
        .webview()
        .cast::<ICoreWebView2_10>()
        .map_err(|e| e.to_string())?;
    let handler = BasicAuthenticationRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        // SAFETY: WebView2 owns the event arguments throughout this callback.
        unsafe {
            let mut uri = PWSTR::null();
            args.Uri(&mut uri)?;
            let uri = take_pwstr(uri);
            let mut challenge = PWSTR::null();
            args.Challenge(&mut challenge)?;
            let challenge = take_pwstr(challenge);
            if url::Url::parse(&uri)
                .ok()
                .is_some_and(|url| url.origin().ascii_serialization() == host.origin)
                && challenge.contains("tcode-preview")
            {
                let response = args.Response()?;
                response.SetUserName(&HSTRING::from("tcode"))?;
                response.SetPassword(&HSTRING::from(&host.token))?;
            } else {
                args.SetCancel(true)?;
            }
        }
        Ok(())
    }));
    let mut token = 0;
    // SAFETY: the webview retains the callback until its owning view is destroyed.
    unsafe { webview.add_BasicAuthenticationRequested(&handler, &mut token) }
        .map_err(|e| e.to_string())
}
