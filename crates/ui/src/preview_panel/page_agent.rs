//! Each engine's half of the page agent: installing its scripts at document
//! start in a world of Tcode's own where the engine has one, the channel it
//! posts through, and the native sources of a page's [`Event`]s —
//! navigation, failed loads, new-window requests, and the console where the
//! engine reports it itself.
//!
//! Gap: gpui-webview and lb-wry expose neither script worlds, nor failed
//! navigations, nor console events, so this reaches each engine
//! directly; their initialization scripts and IPC handler run in the page's
//! own world, where the page could see and spoof them.

use preview_mcp::page::{self, Transport};

use super::events::Event;

pub(super) type Events = async_channel::Sender<Event>;

/// A secret the agent puts in every message: the page cannot read it from
/// the agent's closure, so it cannot pass its own messages off as the
/// agent's.
pub(super) fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(not(target_os = "android"))]
pub(super) use imp::install;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "linux"))]
pub(super) use imp::observe;
#[cfg(target_os = "android")]
pub(super) use imp::scripts;
#[cfg(target_os = "windows")]
pub(super) use imp::{devtools, prepare};
pub(super) use imp::{forget, run, stop};

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, ProtocolObject, Sel};
    use objc2::{
        DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, ffi, msg_send, sel,
    };
    use objc2_foundation::{
        NSError, NSObject, NSObjectProtocol, NSString, NSURL, NSURLErrorFailingURLErrorKey,
    };
    use objc2_web_kit::{
        WKContentWorld, WKScriptMessage, WKScriptMessageHandler, WKUserContentController,
        WKUserScript, WKUserScriptInjectionTime,
    };
    #[cfg(target_os = "ios")]
    use wry::WebViewExtIOS as _;
    #[cfg(target_os = "macos")]
    use wry::WebViewExtMacOS as _;

    use super::{Event, Events, Transport, page};
    use crate::preview_panel::events::LoadError;

    const WORLD: &str = "tcode";
    const HANDLER: &str = "tcode";
    const POST: &str = "((handler) => (message) => handler.postMessage(message))(window.webkit.messageHandlers.tcode)";

    thread_local! {
        /// The pages whose navigation delegate callbacks, plain IMPs on
        /// lb-wry's class, report here; keyed by the WKWebView.
        static PAGES: RefCell<HashMap<usize, Events>> = RefCell::new(HashMap::new());
    }

    fn key(raw: &wry::WebView) -> usize {
        Retained::as_ptr(&raw.webview()) as usize
    }

    fn send(webview: usize, event: Event) {
        PAGES.with_borrow(|pages| {
            if let Some(events) = pages.get(&webview) {
                let _ = events.try_send(event);
            }
        });
    }

    /// Tcode's world, where the page's scripts cannot reach the agent.
    fn world(mtm: MainThreadMarker) -> Retained<WKContentWorld> {
        unsafe { WKContentWorld::worldWithName(&NSString::from_str(WORLD), mtm) }
    }

    pub(crate) struct HandlerState {
        events: Events,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "TcodePageAgentHandler"]
        #[thread_kind = MainThreadOnly]
        #[ivars = HandlerState]
        struct Handler;

        unsafe impl NSObjectProtocol for Handler {}

        unsafe impl WKScriptMessageHandler for Handler {
            #[unsafe(method(userContentController:didReceiveScriptMessage:))]
            fn received(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
                let body = unsafe { message.body() };
                if let Ok(text) = body.downcast::<NSString>() {
                    let _ = self
                        .ivars()
                        .events
                        .try_send(Event::Message(text.to_string()));
                }
            }
        }
    );

    impl Handler {
        fn new(events: Events, mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(HandlerState { events });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// lb-wry's callbacks, built into the page before it exists: the end of
    /// a navigation, which no delegate method Tcode can add to lb-wry's class
    /// reports, and a request for a new window, which loads in this page.
    /// iOS: lb-wry has no UI delegate there, so such a request does nothing.
    pub(crate) fn observe<'a>(
        builder: wry::WebViewBuilder<'a>,
        events: &Events,
    ) -> wry::WebViewBuilder<'a> {
        builder
            .with_on_page_load_handler({
                let events = events.clone();
                move |event, _url| {
                    if matches!(event, wry::PageLoadEvent::Finished) {
                        let _ = events.try_send(Event::Finished);
                    }
                }
            })
            .with_new_window_req_handler({
                let events = events.clone();
                move |url, _| {
                    let _ = events.try_send(Event::NewWindow(url));
                    wry::NewWindowResponse::Deny
                }
            })
    }

    pub(crate) fn install(raw: &wry::WebView, events: Events, nonce: &str) {
        observe_navigation(raw);
        PAGES.with_borrow_mut(|pages| pages.insert(key(raw), events.clone()));
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let world = world(mtm);
        let scripts = page::scripts(
            &Transport {
                post: POST,
                page_world: false,
            },
            nonce,
            true,
        );
        let manager = raw.manager();
        let script = |source: &str, world: &WKContentWorld| unsafe {
            WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(
                WKUserScript::alloc(mtm),
                &NSString::from_str(source),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
                world,
            )
        };
        unsafe {
            let handler = Handler::new(events, mtm);
            manager.addScriptMessageHandler_contentWorld_name(
                ProtocolObject::from_ref(&*handler),
                &world,
                &NSString::from_str(HANDLER),
            );
            manager.addUserScript(&script(&scripts.agent, &world));
            if let Some(hooks) = &scripts.hooks {
                manager.addUserScript(&script(hooks, &WKContentWorld::pageWorld(mtm)));
            }
        }
    }

    /// `stopLoading`: wry has no Stop of its own.
    pub(crate) fn stop(raw: &wry::WebView) {
        let webview = raw.webview();
        // objc2-web-kit declares WKWebView for macOS only.
        unsafe {
            let () = msg_send![&*webview, stopLoading];
        }
    }

    pub(crate) fn run(raw: &wry::WebView, script: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let world = world(mtm);
        let webview = raw.webview();
        // The answer comes from the agent; only a failure is of interest.
        let done = block2::RcBlock::new(|_: *mut AnyObject, error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                log::debug!("preview: page agent call failed: {error:?}");
            }
        });
        // objc2-web-kit declares WKWebView for macOS only.
        unsafe {
            let () = msg_send![
                &*webview,
                evaluateJavaScript: &*NSString::from_str(script),
                inFrame: std::ptr::null::<AnyObject>(),
                inContentWorld: &*world,
                completionHandler: &*done
            ];
        }
    }

    pub(crate) fn forget(raw: &wry::WebView) {
        PAGES.with_borrow_mut(|pages| pages.remove(&key(raw)));
    }

    /// lb-wry's `WryNavigationDelegate` implements navigation policy,
    /// `didCommit` and `didFinish` only, so nothing observes a navigation
    /// starting or failing. Add the missing callbacks to its class — once for
    /// the process, and only where a future lb-wry does not define them.
    fn observe_navigation(raw: &wry::WebView) {
        static PATCHED: std::sync::Once = std::sync::Once::new();

        let webview = raw.webview();
        let Some(delegate) = (unsafe { webview.navigationDelegate() }) else {
            log::debug!("preview: webview has no navigation delegate to observe");
            return;
        };
        // SAFETY: any Objective-C object may be viewed as an `AnyObject`.
        let class = unsafe { &*Retained::as_ptr(&delegate).cast::<AnyObject>() }.class();
        PATCHED.call_once(|| {
            let start: unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut AnyObject,
            ) = did_start;
            let fail: unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut AnyObject,
                *mut NSError,
            ) = did_fail;
            // SAFETY: both functions use the Objective-C calling convention and
            // match the encodings below.
            unsafe {
                let start: Imp = std::mem::transmute(start);
                let fail: Imp = std::mem::transmute(fail);
                add_method(
                    class,
                    sel!(webView:didStartProvisionalNavigation:),
                    c"v@:@@",
                    start,
                );
                add_method(
                    class,
                    sel!(webView:didFailProvisionalNavigation:withError:),
                    c"v@:@@@",
                    fail,
                );
                add_method(
                    class,
                    sel!(webView:didFailNavigation:withError:),
                    c"v@:@@@",
                    fail,
                );
            }
        });
        // WKWebView caches which delegate methods exist when the delegate is
        // assigned, so re-assign it: without this the very first webview (whose
        // delegate was installed before the patch above) never calls them.
        unsafe {
            webview.setNavigationDelegate(None);
            webview.setNavigationDelegate(Some(&delegate));
        }
    }

    /// # Safety
    ///
    /// `imp` must be an `extern "C"` function whose signature matches `types`.
    unsafe fn add_method(class: &AnyClass, selector: Sel, types: &std::ffi::CStr, imp: Imp) {
        if class.responds_to(selector) {
            return;
        }
        let class = std::ptr::from_ref(class).cast_mut();
        let added = unsafe { ffi::class_addMethod(class, selector, imp, types.as_ptr()) };
        if !added.as_bool() {
            log::debug!("preview: failed to observe {selector} on the navigation delegate");
        }
    }

    unsafe extern "C-unwind" fn did_start(
        _this: *mut AnyObject,
        _cmd: Sel,
        webview: *mut AnyObject,
        _navigation: *mut AnyObject,
    ) {
        send(webview as usize, Event::Started);
    }

    unsafe extern "C-unwind" fn did_fail(
        _this: *mut AnyObject,
        _cmd: Sel,
        webview: *mut AnyObject,
        _navigation: *mut AnyObject,
        error: *mut NSError,
    ) {
        // SAFETY: WebKit hands us a live error for the failing web view.
        let Some(error) = (unsafe { error.as_ref() }) else {
            return;
        };
        // A navigation another replaced: NSURLErrorCancelled.
        if error.code() == -999 && error.domain().to_string() == "NSURLErrorDomain" {
            return;
        }
        let failure = LoadError {
            url: failing_url(error)
                .or_else(|| current_url(webview))
                .unwrap_or_default(),
            code: format!("{} {}", error.domain(), error.code()),
            message: error.localizedDescription().to_string(),
        };
        log::info!("preview: {}", failure.describe());
        send(webview as usize, Event::Failed(failure));
    }

    /// The URL WebKit was loading, which is *not* `WKWebView.URL` for a
    /// provisional failure — that still points at the page left on screen.
    fn failing_url(error: &NSError) -> Option<String> {
        let info = error.userInfo();
        let failing = unsafe { info.objectForKey(NSURLErrorFailingURLErrorKey) }?;
        let url = failing.downcast::<NSURL>().ok()?;
        url.absoluteString().map(|url| url.to_string())
    }

    fn current_url(webview: *mut AnyObject) -> Option<String> {
        // SAFETY: WebKit passes the live web view that failed. objc2-web-kit
        // declares WKWebView for macOS only, so `URL` is sent untyped.
        let webview = unsafe { webview.as_ref() }?;
        let url: Option<Retained<NSURL>> = unsafe { msg_send![webview, URL] };
        url?.absoluteString().map(|url| url.to_string())
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    use serde_json::Value;
    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::{
        CallDevToolsProtocolMethodCompletedHandler, DevToolsProtocolEventReceivedEventHandler,
        NavigationCompletedEventHandler, NavigationStartingEventHandler,
        NewWindowRequestedEventHandler, take_pwstr,
    };
    use windows_core::{HSTRING, Interface as _, PWSTR};

    use super::{Event, Events, Transport, page};
    use crate::preview_panel::events::LoadError;
    use crate::preview_panel::webview2::RawWebView;
    use preview_mcp::page::ConsoleEntry;

    /// The isolated world the agent runs in, and the binding it posts
    /// through, which exists only there.
    const WORLD: &str = "tcode";
    const BINDING: &str = "__tcodePost";
    const POST: &str = "((binding) => (message) => binding(message))(globalThis.__tcodePost)";

    thread_local! {
        /// Each page's agent context: Runtime.evaluate needs its id, which
        /// the agent's `ready` arrives with.
        static CONTEXTS: RefCell<HashMap<usize, Rc<Cell<Option<i64>>>>> = RefCell::new(HashMap::new());
    }

    fn key(raw: &RawWebView) -> usize {
        raw.webview().as_raw() as usize
    }

    /// Call a DevTools method; `done` gets its result.
    fn cdp_call(
        webview: &ICoreWebView2,
        method: &str,
        params: Value,
        done: impl FnOnce(Result<String, String>) + 'static,
    ) {
        let name = method.to_owned();
        let done = Rc::new(RefCell::new(Some(done)));
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new({
            let done = done.clone();
            let name = name.clone();
            move |result: windows_core::Result<()>, json: String| {
                if let Some(done) = done.borrow_mut().take() {
                    done(
                        result
                            .map(|()| json)
                            .map_err(|error| format!("{name}: {error}")),
                    );
                }
                Ok(())
            }
        }));
        let started = unsafe {
            webview.CallDevToolsProtocolMethod(
                &HSTRING::from(method),
                &HSTRING::from(params.to_string()),
                &handler,
            )
        };
        if let Err(error) = started
            && let Some(done) = done.borrow_mut().take()
        {
            done(Err(format!("{name}: {error}")));
        }
    }

    /// Call a DevTools method and await its result.
    pub(crate) async fn devtools(
        webview: &ICoreWebView2,
        method: &str,
        params: Value,
    ) -> Result<String, String> {
        let (tx, rx) = async_channel::bounded(1);
        cdp_call(webview, method, params, move |result| {
            let _ = tx.try_send(result);
        });
        rx.recv()
            .await
            .map_err(|_| format!("{method} was cancelled"))?
    }

    /// Before the page's first navigation: the agent in its own world, the
    /// binding it posts through, and the engine's console, which makes the
    /// agent's own capture unnecessary.
    pub(crate) async fn prepare(webview: ICoreWebView2, nonce: String) -> Result<(), String> {
        let scripts = page::scripts(
            &Transport {
                post: POST,
                page_world: false,
            },
            &nonce,
            false,
        );
        devtools(&webview, "Page.enable", serde_json::json!({})).await?;
        devtools(
            &webview,
            "Runtime.addBinding",
            serde_json::json!({ "name": BINDING, "executionContextName": WORLD }),
        )
        .await?;
        devtools(
            &webview,
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": scripts.agent, "worldName": WORLD }),
        )
        .await?;
        devtools(&webview, "Runtime.enable", serde_json::json!({})).await?;
        Ok(())
    }

    fn receive(webview: &ICoreWebView2, event: &str, mut handle: impl FnMut(Value) + 'static) {
        let receiver =
            match unsafe { webview.GetDevToolsProtocolEventReceiver(&HSTRING::from(event)) } {
                Ok(receiver) => receiver,
                Err(error) => {
                    log::debug!("preview: cannot receive {event}: {error}");
                    return;
                }
            };
        let handler =
            DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut json = PWSTR::null();
                    unsafe { args.ParameterObjectAsJson(&mut json)? };
                    if let Ok(params) = serde_json::from_str(&take_pwstr(json)) {
                        handle(params);
                    }
                }
                Ok(())
            }));
        let mut token = 0i64;
        if let Err(error) =
            unsafe { receiver.add_DevToolsProtocolEventReceived(&handler, &mut token) }
        {
            log::debug!("preview: cannot receive {event}: {error}");
        }
    }

    pub(crate) fn install(raw: &RawWebView, events: Events, _nonce: &str) {
        let webview = raw.webview();
        let context = Rc::new(Cell::new(None));
        CONTEXTS.with_borrow_mut(|contexts| contexts.insert(key(raw), context.clone()));
        let mut token = 0i64;

        let started = NavigationStartingEventHandler::create(Box::new({
            let events = events.clone();
            move |_, _| {
                let _ = events.try_send(Event::Started);
                Ok(())
            }
        }));
        if let Err(error) = unsafe { webview.add_NavigationStarting(&started, &mut token) } {
            log::debug!("preview: failed to observe navigation start: {error}");
        }
        // WebView2 reports a failed navigation only here; the page itself
        // keeps showing the previous document.
        let completed = NavigationCompletedEventHandler::create(Box::new({
            let events = events.clone();
            move |webview, args| {
                let (Some(webview), Some(args)) = (webview, args) else {
                    return Ok(());
                };
                let mut succeeded = windows_core::BOOL::default();
                unsafe { args.IsSuccess(&mut succeeded)? };
                if succeeded.as_bool() {
                    let _ = events.try_send(Event::Finished);
                    return Ok(());
                }
                let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                unsafe { args.WebErrorStatus(&mut status)? };
                // A navigation another replaced.
                if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED {
                    return Ok(());
                }
                let failure = LoadError {
                    url: source_url(&webview),
                    code: status_name(status).to_string(),
                    message: format!("WebView2 error status {}", status.0),
                };
                log::info!("preview: {}", failure.describe());
                let _ = events.try_send(Event::Failed(failure));
                Ok(())
            }
        }));
        if let Err(error) = unsafe { webview.add_NavigationCompleted(&completed, &mut token) } {
            log::debug!("preview: failed to observe navigation completion: {error}");
        }
        // A page asking for a window gets none: what it opens loads here.
        let opened = NewWindowRequestedEventHandler::create(Box::new({
            let events = events.clone();
            move |_, args| {
                if let Some(args) = args {
                    let mut uri = windows_core::PWSTR::null();
                    unsafe {
                        args.SetHandled(true)?;
                        args.Uri(&mut uri)?;
                    }
                    let _ = events.try_send(Event::NewWindow(take_pwstr(uri)));
                }
                Ok(())
            }
        }));
        if let Err(error) = unsafe { webview.add_NewWindowRequested(&opened, &mut token) } {
            log::debug!("preview: failed to observe new-window requests: {error}");
        }

        receive(webview, "Runtime.bindingCalled", {
            let events = events.clone();
            let context = context.clone();
            move |params| {
                if params["name"] != BINDING {
                    return;
                }
                let Some(payload) = params["payload"].as_str() else {
                    return;
                };
                if serde_json::from_str::<Value>(payload)
                    .is_ok_and(|message| message["t"] == "ready")
                {
                    context.set(params["executionContextId"].as_i64());
                }
                let _ = events.try_send(Event::Message(payload.to_owned()));
            }
        });
        receive(webview, "Runtime.consoleAPICalled", {
            let events = events.clone();
            let context = context.clone();
            move |params| {
                if params["executionContextId"].as_i64() == context.get() {
                    return;
                }
                let text = params["args"]
                    .as_array()
                    .map(|args| args.iter().map(remote_text).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let level = match params["type"].as_str().unwrap_or("log") {
                    "warning" => "warn",
                    "error" | "assert" => "error",
                    "info" => "info",
                    "debug" => "debug",
                    _ => "log",
                };
                let _ = events.try_send(Event::Console(ConsoleEntry {
                    level: level.into(),
                    text,
                    location: location(&params["stackTrace"]["callFrames"][0]),
                }));
            }
        });
        receive(webview, "Runtime.exceptionThrown", {
            let events = events.clone();
            move |params| {
                let details = &params["exceptionDetails"];
                let text = details["exception"]["description"]
                    .as_str()
                    .or_else(|| details["text"].as_str())
                    .unwrap_or("Uncaught exception");
                let _ = events.try_send(Event::Console(ConsoleEntry {
                    level: "error".into(),
                    text: text.into(),
                    location: location(details),
                }));
            }
        });
    }

    /// A console argument as DevTools prints it.
    fn remote_text(argument: &Value) -> String {
        match &argument["value"] {
            Value::String(text) => text.clone(),
            Value::Null => argument["description"]
                .as_str()
                .or_else(|| argument["unserializableValue"].as_str())
                .or_else(|| argument["type"].as_str())
                .unwrap_or_default()
                .to_owned(),
            value => value.to_string(),
        }
    }

    fn location(frame: &Value) -> Option<String> {
        let url = frame["url"].as_str().filter(|url| !url.is_empty())?;
        Some(match frame["lineNumber"].as_u64() {
            Some(line) => format!("{url}:{}", line + 1),
            None => url.to_owned(),
        })
    }

    pub(crate) fn stop(raw: &RawWebView) {
        if let Err(error) = unsafe { raw.webview().Stop() } {
            log::debug!("preview: failed to stop loading: {error}");
        }
    }

    pub(crate) fn run(raw: &RawWebView, script: &str) {
        let Some(context) = CONTEXTS.with_borrow(|contexts| contexts.get(&key(raw))?.get()) else {
            log::debug!("preview: no page agent context to run in");
            return;
        };
        // The answer comes from the agent; only a refusal is of interest.
        cdp_call(
            raw.webview(),
            "Runtime.evaluate",
            serde_json::json!({ "expression": script, "contextId": context }),
            |result| {
                if let Err(error) = result {
                    log::debug!("preview: page agent call failed: {error}");
                }
            },
        );
    }

    pub(crate) fn forget(raw: &RawWebView) {
        CONTEXTS.with_borrow_mut(|contexts| contexts.remove(&key(raw)));
    }

    fn source_url(webview: &ICoreWebView2) -> String {
        let mut source = windows_core::PWSTR::null();
        match unsafe { webview.Source(&mut source) } {
            Ok(()) => take_pwstr(source),
            Err(error) => {
                log::debug!("preview: failed to read the failing URL: {error}");
                String::new()
            }
        }
    }

    /// WebView2 statuses are a plain integer newtype; report the SDK name so the
    /// agent (and the user) can recognise e.g. a self-signed certificate.
    fn status_name(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> &'static str {
        match status {
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT => {
                "CertificateCommonNameIsIncorrect"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED => "CertificateExpired",
            COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS => {
                "ClientCertificateContainsErrors"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED => "CertificateRevoked",
            COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID => "CertificateIsInvalid",
            COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE => "ServerUnreachable",
            COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => "Timeout",
            COREWEBVIEW2_WEB_ERROR_STATUS_ERROR_HTTP_INVALID_SERVER_RESPONSE => {
                "ErrorHttpInvalidServerResponse"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED => "ConnectionAborted",
            COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET => "ConnectionReset",
            COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED => "Disconnected",
            COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT => "CannotConnect",
            COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED => "HostNameNotResolved",
            COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => "OperationCanceled",
            COREWEBVIEW2_WEB_ERROR_STATUS_REDIRECT_FAILED => "RedirectFailed",
            COREWEBVIEW2_WEB_ERROR_STATUS_UNEXPECTED_ERROR => "UnexpectedError",
            COREWEBVIEW2_WEB_ERROR_STATUS_VALID_AUTHENTICATION_CREDENTIALS_REQUIRED => {
                "ValidAuthenticationCredentialsRequired"
            }
            COREWEBVIEW2_WEB_ERROR_STATUS_VALID_PROXY_AUTHENTICATION_REQUIRED => {
                "ValidProxyAuthenticationRequired"
            }
            _ => "Unknown",
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {

    use webkit2gtk::glib::{self, translate::ToGlibPtr as _};
    use webkit2gtk::{
        LoadEvent, NetworkError, PolicyError, UserContentInjectedFrames,
        UserContentManagerExt as _, UserScript, UserScriptInjectionTime, WebViewExt as _, gio,
    };
    use wry::WebViewExtUnix as _;

    use super::{Event, Events, Transport, page};
    use crate::preview_panel::events::LoadError;

    const WORLD: &str = "tcode";
    const HANDLER: &str = "tcode";
    const POST: &str = "((handler) => (message) => handler.postMessage(message))(window.webkit.messageHandlers.tcode)";

    // Gap: webkit2gtk 2.0 takes a script message as a JavaScriptCore value
    // from javascriptcore-rs, which it does not re-export.
    unsafe extern "C" {
        fn webkit_javascript_result_get_js_value(
            result: *mut webkit2gtk::ffi::WebKitJavascriptResult,
        ) -> *mut std::ffi::c_void;
        fn jsc_value_to_string(value: *mut std::ffi::c_void) -> *mut std::ffi::c_char;
    }

    /// lb-wry's new-window callback, built into the page before it exists:
    /// what a page opens loads in this page. `load-changed` reports
    /// navigations directly.
    pub(crate) fn observe<'a>(
        builder: wry::WebViewBuilder<'a>,
        events: &Events,
    ) -> wry::WebViewBuilder<'a> {
        let events = events.clone();
        builder.with_new_window_req_handler(move |url, _| {
            let _ = events.try_send(Event::NewWindow(url));
            wry::NewWindowResponse::Deny
        })
    }

    pub(crate) fn install(raw: &wry::WebView, events: Events, nonce: &str) {
        let webview = raw.webview();
        // WebKitGTK reports a failed navigation through `load-failed`, and an
        // untrusted certificate first through `load-failed-with-tls-errors`.
        // `load-failed` is handled so WebKit does not load its stock error
        // page: the other engines keep the previous document on screen too.
        webview.connect_load_changed({
            let events = events.clone();
            move |_, event| match event {
                LoadEvent::Started => {
                    let _ = events.try_send(Event::Started);
                }
                LoadEvent::Finished => {
                    let _ = events.try_send(Event::Finished);
                }
                _ => {}
            }
        });
        webview.connect_load_failed({
            let events = events.clone();
            move |_, _, uri, error| {
                // A certificate failure was already recorded with its flags.
                if error.kind::<gio::TlsError>().is_none()
                    && error.kind::<NetworkError>() != Some(NetworkError::Cancelled)
                {
                    let _ = events.try_send(Event::Failed(LoadError {
                        url: uri.to_string(),
                        code: error_code(error),
                        message: error.message().to_string(),
                    }));
                }
                true
            }
        });
        webview.connect_load_failed_with_tls_errors({
            let events = events.clone();
            move |_, uri, _, flags| {
                let _ = events.try_send(Event::Failed(LoadError {
                    url: uri.to_string(),
                    code: format!("WebKitTlsCertificate {flags:?}"),
                    message: "the server's TLS certificate is not trusted".into(),
                }));
                false
            }
        });

        let Some(manager) = webview.user_content_manager() else {
            log::warn!("preview: the webview has no user content manager; the page agent is off");
            return;
        };
        let scripts = page::scripts(
            &Transport {
                post: POST,
                page_world: false,
            },
            nonce,
            true,
        );
        manager.register_script_message_handler_in_world(HANDLER, WORLD);
        manager.connect_script_message_received(Some(HANDLER), move |_, result| {
            // SAFETY: the result is live for the signal; the string is ours.
            let text = unsafe {
                let value = webkit_javascript_result_get_js_value(result.to_glib_none().0);
                let raw = jsc_value_to_string(value);
                if raw.is_null() {
                    return;
                }
                let text = std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned();
                glib::ffi::g_free(raw.cast());
                text
            };
            let _ = events.try_send(Event::Message(text));
        });
        manager.add_script(&UserScript::for_world(
            &scripts.agent,
            UserContentInjectedFrames::TopFrame,
            UserScriptInjectionTime::Start,
            WORLD,
            &[],
            &[],
        ));
        if let Some(hooks) = &scripts.hooks {
            manager.add_script(&UserScript::new(
                hooks,
                UserContentInjectedFrames::TopFrame,
                UserScriptInjectionTime::Start,
                &[],
                &[],
            ));
        }
    }

    // `run_javascript_in_world` is deprecated from WebKitGTK 2.40, whose
    // replacement lb-wry's webkit2gtk features leave out.
    #[allow(deprecated)]
    pub(crate) fn stop(raw: &wry::WebView) {
        raw.webview().stop_loading();
    }

    pub(crate) fn run(raw: &wry::WebView, script: &str) {
        raw.webview()
            .run_javascript_in_world(script, WORLD, None::<&gio::Cancellable>, |result| {
                if let Err(error) = result {
                    log::debug!("preview: page agent call failed: {error}");
                }
            });
    }

    pub(crate) fn forget(_raw: &wry::WebView) {}

    /// GLib errors carry a domain and an enum value; name both so a refused
    /// connection reads differently from a policy cancellation.
    fn error_code(error: &glib::Error) -> String {
        fn named<T: glib::error::ErrorDomain + std::fmt::Debug>(
            error: &glib::Error,
        ) -> Option<String> {
            error.kind::<T>().map(|kind| format!("{kind:?}"))
        }
        let kind = named::<NetworkError>(error)
            .or_else(|| named::<PolicyError>(error))
            .or_else(|| named::<gio::IOErrorEnum>(error))
            .or_else(|| named::<gio::ResolverError>(error));
        let domain = error.domain();
        match kind {
            Some(kind) => format!("{} {kind}", domain.as_str()),
            None => domain.as_str().to_string(),
        }
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::{Transport, page};
    use crate::preview_panel::android::RawWebView;

    /// `PreviewHost` installs the agent with
    /// `WebViewCompat.addDocumentStartJavaScript` and listens with
    /// `addWebMessageListener`, both in the page's world: Android WebView has
    /// no other. The listener's object is captured before any page script
    /// runs, and the nonce stays in the agent's closure.
    const POST: &str =
        "((listener) => (message) => listener.postMessage(message))(globalThis.tcodeAgent)";

    /// The agent's script for `PreviewHost`; Android reports the console
    /// itself (`onConsoleMessage`), and with one window a page's new-window
    /// request loads in the page by default.
    pub(crate) fn scripts(nonce: &str) -> String {
        page::scripts(
            &Transport {
                post: POST,
                page_world: true,
            },
            nonce,
            false,
        )
        .agent
    }

    pub(crate) fn run(raw: &RawWebView, script: &str) {
        raw.run(script);
    }

    pub(crate) fn stop(raw: &RawWebView) {
        raw.stop();
    }

    pub(crate) fn forget(_raw: &RawWebView) {}
}
