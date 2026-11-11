//! GPUI owner for the activity's WebView. The shared lifecycle owns these
//! entities; Java owns the actual views and only sends owned events/results.
use super::{
    ReplyTx,
    events::{self, Event},
};
use futures::StreamExt as _;
use gpui::{
    Context, IntoElement, ParentElement as _, Render, Styled as _, Window, canvas, div, point,
};
use gpui_android::webview::{Browser, Event as JniEvent, Reply};
use preview_mcp::page::ConsoleEntry;
use std::{cell::RefCell, rc::Rc};

/// `PreviewHost`'s event numbers past the page events `events::android_event`
/// reads: a console message, and a page agent message.
const CONSOLE: i32 = 8;
const AGENT: i32 = 9;

pub struct RawWebView {
    browser: Browser,
    url: RefCell<String>,
    title: RefCell<String>,
}
impl RawWebView {
    /// A page whose documents get `agent` at document start.
    pub(super) fn new(
        initial_url: &str,
        agent: &str,
        proxy: Option<&tcode_traverse::preview::ProxyEntry>,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<JniEvent>), String> {
        let (browser, events) = Browser::new(
            &serde_json::json!({
                "url": initial_url,
                "agent": agent,
                "proxy": proxy.map(|host| serde_json::json!({"origin": host.origin})),
            })
            .to_string(),
        )?;
        Ok((
            Self {
                browser,
                url: RefCell::new(initial_url.to_owned()),
                title: RefCell::new(String::new()),
            },
            events,
        ))
    }
    pub fn load_url(&self, url: &str) -> Result<(), String> {
        *self.url.borrow_mut() = url.to_owned();
        self.browser.command("navigate", url, [0; 4]);
        Ok(())
    }
    pub fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.browser
            .command(if visible { "show" } else { "hide" }, "", [0; 4]);
        Ok(())
    }
    pub fn focus_parent(&self) -> Result<(), String> {
        self.browser.command("blur", "", [0; 4]);
        Ok(())
    }
    /// Run a page agent call; its answer comes back as an agent message.
    pub(super) fn run(&self, script: &str) {
        self.browser.command("run", script, [0; 4]);
    }
    pub fn reload(&self) -> Result<(), String> {
        self.browser.command("reload", "", [0; 4]);
        Ok(())
    }
    pub(super) fn stop(&self) {
        self.browser.command("stop", "", [0; 4]);
    }
    pub fn forward(&self) -> Result<(), String> {
        self.browser.command("forward", "", [0; 4]);
        Ok(())
    }
    pub(super) fn evaluate_json<T: 'static>(&self, script: &str, reply: ReplyTx, cx: &Context<T>) {
        let request = self.browser.request("evaluate", script);
        cx.spawn(async move |_, _| {
            // The page's answer is waited for until the caller stops waiting.
            let result = futures::future::select(Box::pin(request), Box::pin(reply.closed())).await;
            let result = match result {
                futures::future::Either::Left((Ok(Reply::Json(raw)), _)) => Ok(
                    tcode_protocol::PreviewResponse::Json(preview_mcp::page::parse_result(&raw)),
                ),
                futures::future::Either::Left((Err(error), _)) => Err(error),
                futures::future::Either::Left((Ok(Reply::Png(_)), _)) => {
                    Err("preview returned an image for JavaScript".into())
                }
                futures::future::Either::Right(_) => return,
            };
            let _ = reply.try_send(result);
        })
        .detach();
    }
    /// What is on screen, or the part `crop` names: `x,y,width,height` in
    /// device pixels.
    pub(super) fn screenshot(
        &self,
        crop: &str,
    ) -> impl Future<Output = Result<Reply, String>> + use<> {
        self.browser.request("screenshot", crop)
    }
}

pub struct WebView {
    raw: Rc<RawWebView>,
    visible: bool,
}
impl WebView {
    pub(super) fn new(
        raw: RawWebView,
        mut events: futures::channel::mpsc::UnboundedReceiver<JniEvent>,
        page: super::page_agent::Events,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |view, cx| {
                        let forwarded = match event.kind {
                            CONSOLE => Some(Event::Console(ConsoleEntry {
                                level: event.title,
                                text: event.message,
                                location: (!event.url.is_empty())
                                    .then(|| format!("{}:{}", event.url, event.code)),
                            })),
                            AGENT => Some(Event::Message(event.message)),
                            _ => {
                                if !event.url.is_empty() {
                                    *view.raw.url.borrow_mut() = event.url.clone();
                                }
                                *view.raw.title.borrow_mut() = event.title;
                                events::android_event(
                                    event.kind,
                                    event.url,
                                    event.code,
                                    event.message,
                                )
                            }
                        };
                        if let Some(forwarded) = forwarded {
                            let _ = page.try_send(forwarded);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            raw: Rc::new(raw),
            visible: false,
        }
    }
    pub fn raw(&self) -> &RawWebView {
        &self.raw
    }
    pub fn show(&mut self) {
        self.visible = true;
    }
    pub fn visible(&self) -> bool {
        self.visible
    }
    pub fn hide(&mut self) {
        self.visible = false;
        let _ = self.raw.set_visible(false);
    }
    pub fn back(&mut self) -> Result<(), String> {
        self.raw.browser.command("back", "", [0; 4]);
        Ok(())
    }
    pub(super) fn metadata(&self) -> (String, String) {
        (self.url(), self.raw.title.borrow().clone())
    }
    pub(super) fn url(&self) -> String {
        self.raw.url.borrow().clone()
    }
}
impl Drop for WebView {
    fn drop(&mut self) {
        self.hide();
    }
}
impl Render for WebView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let raw = self.raw.clone();
        let visible = self.visible;
        div().size_full().child(
            canvas(
                move |bounds, window, _| {
                    let safe = crate::window_seam::content_insets(window);
                    let physical = super::android_geometry::physical_bounds(
                        gpui::Bounds {
                            origin: bounds.origin - point(safe.left, safe.top),
                            size: bounds.size,
                        },
                        point(safe.left, safe.top),
                        window.scale_factor(),
                    );
                    raw.browser.command("bounds", "", physical);
                    let _ = raw.set_visible(visible && physical[2] > 0 && physical[3] > 0);
                },
                |_, _, _, _| {},
            )
            .size_full(),
        )
    }
}
