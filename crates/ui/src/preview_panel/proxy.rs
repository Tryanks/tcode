//! Desktop proxy configuration and platform authentication absent from wry's endpoint type.
use tcode_client::pairing::PairedHost;

pub(super) fn builder<'a>(
    builder: wry::WebViewBuilder<'a>,
    host: Option<&PairedHost>,
) -> Result<wry::WebViewBuilder<'a>, String> {
    let Some(host) = host else {
        return Ok(builder);
    };
    let origin = url::Url::parse(&host.origin).map_err(|e| e.to_string())?;
    if origin.scheme() != "http" {
        return Err("remote desktop preview requires a direct HTTP machine origin".into());
    }
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSOperatingSystemVersion, NSProcessInfo};
        if !NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
            majorVersion: 14,
            minorVersion: 0,
            patchVersion: 0,
        }) {
            return Err("remote preview requires macOS 14 or later".into());
        }
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
    #[cfg(target_os = "windows")]
    let builder = {
        use wry::WebViewBuilderExtWindows as _;
        // wry's additional arguments replace its generated proxy arguments.
        builder.with_additional_browser_args(&format!(
            "--proxy-server={} --proxy-bypass-list=<-loopback>",
            host.origin
        ))
    };
    Ok(builder)
}

#[cfg(target_os = "macos")]
pub(super) fn authenticate(raw: &wry::WebView, host: Option<&PairedHost>) -> Result<(), String> {
    use objc2::{msg_send, rc::Retained};
    use objc2_foundation::{NSArray, NSObject, ns_string};
    use std::ffi::{CString, c_char};
    use wry::WebViewExtMacOS as _;
    let Some(host) = host else {
        return Ok(());
    };
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_proxy_config_set_username_and_password(
            config: *const NSObject,
            username: *const c_char,
            password: *const c_char,
        );
    }
    let token = CString::new(host.token.as_str()).map_err(|e| e.to_string())?;
    // SAFETY: wry installed an NWProxyConfig on this private data store. Access
    // and mutate it only on the UI thread, before the first network navigation.
    unsafe {
        let config: Retained<NSObject> = msg_send![&*raw.webview(), configuration];
        let store: Retained<NSObject> = msg_send![&*config, websiteDataStore];
        let proxies: Option<Retained<NSArray<NSObject>>> =
            msg_send![&*store, valueForKey: ns_string!("proxyConfigurations")];
        let proxies = proxies.ok_or("WebKit did not install the preview proxy")?;
        if proxies.count() != 1 {
            return Err("WebKit did not install one preview proxy".into());
        }
        let proxy = proxies.objectAtIndex(0);
        nw_proxy_config_set_username_and_password(&*proxy, c"tcode".as_ptr(), token.as_ptr());
        let _: () =
            msg_send![&*store, setValue: &*proxies, forKey: ns_string!("proxyConfigurations")];
    }
    Ok(())
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
