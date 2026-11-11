//! Pictures of a preview page — what is on screen, one element, or the whole
//! document — as each engine can take them. A picture an engine cannot take
//! is refused with the reason, never approximated.

use gpui::{App, Entity};
#[cfg(not(any(target_os = "android", target_os = "windows")))]
use gpui_webview::WebView;
use serde_json::Value;
use tcode_protocol::PreviewResponse;

#[cfg(target_os = "android")]
use super::android::WebView;
#[cfg(target_os = "windows")]
use super::webview2::WebView;

use super::ReplyTx;

/// The largest picture taken, in pixels.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const MAX_PIXELS: f64 = 16_000_000.;

pub(super) enum Region {
    Viewport,
    Element(ElementBox),
    FullPage,
}

/// An element's box as the page agent measured it, in CSS pixels of the
/// viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ElementBox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    scroll_x: f64,
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    scroll_y: f64,
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    dpr: f64,
    viewport_width: f64,
    viewport_height: f64,
}

impl ElementBox {
    /// The agent's `rect` answer.
    pub(super) fn from_agent(value: &Value) -> Option<Self> {
        let number = |key: &str| value[key].as_f64();
        Some(Self {
            x: number("x")?,
            y: number("y")?,
            width: number("width")?,
            height: number("height")?,
            scroll_x: number("scrollX").unwrap_or_default(),
            scroll_y: number("scrollY").unwrap_or_default(),
            dpr: number("dpr").unwrap_or(1.),
            viewport_width: number("viewportWidth")?,
            viewport_height: number("viewportHeight")?,
        })
    }

    /// The part of the box inside the viewport: `(x, y, width, height)`.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    fn visible(&self) -> Option<(f64, f64, f64, f64)> {
        let left = self.x.max(0.);
        let top = self.y.max(0.);
        let right = (self.x + self.width).min(self.viewport_width);
        let bottom = (self.y + self.height).min(self.viewport_height);
        (right > left && bottom > top).then_some((left, top, right - left, bottom - top))
    }
}

const NO_AREA: &str = "the element has no visible area to capture";

/// Answer `reply` with a picture of `region` of `view`'s page.
pub(super) fn capture(view: &Entity<WebView>, region: Region, reply: ReplyTx, cx: &mut App) {
    let picture = imp::capture(view, region, cx);
    cx.spawn(async move |_| {
        // The engine's picture is waited for until the caller stops waiting.
        let result = futures_lite::future::or(async { Some(picture.await) }, async {
            reply.closed().await;
            None
        })
        .await;
        if let Some(result) = result {
            let _ = reply.send(result.map(png)).await;
        }
    })
    .detach();
}

fn png(bytes: Vec<u8>) -> PreviewResponse {
    use base64::Engine as _;
    PreviewResponse::Image {
        mime: "image/png".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

/// A picture to come, as PNG bytes.
type Picture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, String>>>>;

fn ready(result: Result<Vec<u8>, String>) -> Picture {
    Box::pin(std::future::ready(result))
}

/// A picture an engine's callback delivers.
#[cfg(not(target_os = "android"))]
fn delivered() -> (async_channel::Sender<Result<Vec<u8>, String>>, Picture) {
    let (tx, rx) = async_channel::bounded(1);
    (
        tx,
        Box::pin(async move {
            rx.recv()
                .await
                .unwrap_or_else(|_| Err("the browser dropped the screenshot".into()))
        }),
    )
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use block2::RcBlock;
    #[cfg(target_os = "macos")]
    use objc2_app_kit::NSImage as Image;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::NSError;
    #[cfg(target_os = "ios")]
    use objc2_ui_kit::UIImage as Image;
    #[cfg(target_os = "ios")]
    use wry::WebViewExtIOS as _;
    #[cfg(target_os = "macos")]
    use wry::WebViewExtMacOS as _;

    use super::*;

    pub(super) fn capture(view: &Entity<WebView>, region: Region, cx: &App) -> Picture {
        let webview = view.read(cx).raw().webview();
        // The native frame, not the GPUI bounds: a hidden browser has never
        // been laid out by GPUI, only staged offscreen.
        let frame = webview.frame();
        if frame.size.width <= 0. || frame.size.height <= 0. {
            return ready(Err("preview browser has no visible area".into()));
        }
        let rect = match region {
            Region::Viewport => None,
            Region::Element(element) => match element.visible() {
                Some((x, y, width, height)) => {
                    Some(CGRect::new(CGPoint::new(x, y), CGSize::new(width, height)))
                }
                None => return ready(Err(NO_AREA.into())),
            },
            Region::FullPage => {
                return ready(Err(
                    "preview_screenshot fullPage is not supported here: WebKit snapshots only \
                     the visible area; take the viewport, or an element by its ref"
                        .into(),
                ));
            }
        };
        let (tx, picture) = delivered();
        let handler = RcBlock::new(move |image: *mut Image, error: *mut NSError| {
            let result = if let Some(image) = unsafe { image.as_ref() } {
                encode(image)
            } else if let Some(error) = unsafe { error.as_ref() } {
                Err(format!(
                    "WKWebView snapshot failed: {}",
                    error.localizedDescription()
                ))
            } else {
                Err("WKWebView snapshot returned no image".into())
            };
            let _ = tx.try_send(result);
        });
        // lb-wry's iOS WKWebView binding declares no snapshot method, and
        // objc2-web-kit's configuration is macOS only.
        unsafe {
            let configuration: Option<objc2::rc::Retained<objc2::runtime::AnyObject>> =
                rect.map(|rect| {
                    let configuration: objc2::rc::Retained<objc2::runtime::AnyObject> =
                        objc2::msg_send![objc2::class!(WKSnapshotConfiguration), new];
                    let () = objc2::msg_send![&*configuration, setRect: rect];
                    configuration
                });
            let () = objc2::msg_send![
                &*webview,
                takeSnapshotWithConfiguration: configuration
                    .as_deref()
                    .map_or(std::ptr::null(), std::ptr::from_ref),
                completionHandler: &*handler
            ];
        }
        picture
    }

    #[cfg(target_os = "macos")]
    fn encode(image: &Image) -> Result<Vec<u8>, String> {
        let cg_image =
            unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
                .ok_or_else(|| "failed to obtain CGImage from WKWebView snapshot".to_string())?;
        png_bytes(&cg_image)
    }

    #[cfg(target_os = "ios")]
    fn encode(image: &Image) -> Result<Vec<u8>, String> {
        let cg_image = unsafe { image.CGImage() }
            .ok_or_else(|| "failed to obtain CGImage from WKWebView snapshot".to_string())?;
        png_bytes(&cg_image)
    }

    fn png_bytes(cg_image: &objc2_core_graphics::CGImage) -> Result<Vec<u8>, String> {
        use objc2_core_foundation::{CFMutableData, CFString};
        use objc2_image_io::CGImageDestination;

        let data = CFMutableData::new(None, 0)
            .ok_or_else(|| "failed to allocate PNG destination data".to_string())?;
        let png_type = CFString::from_static_str("public.png");
        let destination = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) }
            .ok_or_else(|| "failed to create PNG image destination".to_string())?;
        unsafe {
            destination.add_image(cg_image, None);
            if !destination.finalize() {
                return Err("failed to finalize WKWebView snapshot PNG".into());
            }
        }
        Ok(data.to_vec())
    }
}

#[cfg(target_os = "android")]
mod imp {
    use super::*;

    pub(super) fn capture(view: &Entity<WebView>, region: Region, cx: &App) -> Picture {
        let crop = match region {
            Region::Viewport => None,
            Region::Element(element) => match element.visible() {
                Some((x, y, width, height)) => {
                    let device = |value: f64| (value * element.dpr).round() as i64;
                    Some(format!(
                        "{},{},{},{}",
                        device(x),
                        device(y),
                        device(width).max(1),
                        device(height).max(1)
                    ))
                }
                None => return ready(Err(NO_AREA.into())),
            },
            Region::FullPage => {
                return ready(Err(
                    "preview_screenshot fullPage is not supported on Android: the WebView \
                     draws only the visible area; take the viewport, or an element by its ref"
                        .into(),
                ));
            }
        };
        let request = view
            .read(cx)
            .raw()
            .screenshot(crop.as_deref().unwrap_or(""));
        Box::pin(async move {
            match request.await? {
                gpui_android::webview::Reply::Png(bytes) => Ok(bytes),
                _ => Err("preview screenshot returned no image".into()),
            }
        })
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, ICoreWebView2,
    };
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET};

    use super::*;
    use crate::preview_panel::page_agent;

    pub(super) fn capture(view: &Entity<WebView>, region: Region, cx: &App) -> Picture {
        let webview = view.read(cx).raw().webview().clone();
        match region {
            Region::Viewport => preview(&webview),
            Region::Element(element) => {
                if element.width <= 0. || element.height <= 0. {
                    return ready(Err(NO_AREA.into()));
                }
                devtools_shot(
                    webview,
                    Some((
                        element.x + element.scroll_x,
                        element.y + element.scroll_y,
                        element.width,
                        element.height,
                    )),
                )
            }
            Region::FullPage => devtools_shot(webview, None),
        }
    }

    /// What is on screen: `CapturePreview`.
    fn preview(webview: &ICoreWebView2) -> Picture {
        let stream = match unsafe { CreateStreamOnHGlobal(Default::default(), true) } {
            Ok(stream) => stream,
            Err(error) => return ready(Err(format!("cannot buffer the screenshot: {error}"))),
        };
        let (tx, picture) = delivered();
        let handler = CapturePreviewCompletedHandler::create(Box::new({
            let stream = stream.clone();
            move |result: windows_core::Result<()>| {
                let _ = tx.try_send(
                    result
                        .map_err(|error| format!("WebView2 CapturePreview failed: {error}"))
                        .and_then(|()| read_all(&stream)),
                );
                Ok(())
            }
        }));
        if let Err(error) = unsafe {
            webview.CapturePreview(
                COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                &stream,
                &handler,
            )
        } {
            return ready(Err(format!("WebView2 CapturePreview failed: {error}")));
        }
        picture
    }

    fn read_all(stream: &IStream) -> Result<Vec<u8>, String> {
        let failed = |error: windows_core::Error| format!("cannot read the screenshot: {error}");
        let mut stat = STATSTG::default();
        unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.map_err(failed)?;
        let mut bytes = vec![0u8; usize::try_from(stat.cbSize).unwrap_or_default()];
        unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }.map_err(failed)?;
        let mut read = 0u32;
        unsafe {
            stream
                .Read(
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as u32,
                    Some(&mut read),
                )
                .ok()
                .map_err(failed)?;
        }
        bytes.truncate(read as usize);
        Ok(bytes)
    }

    /// An element's box, or the whole document, from DevTools'
    /// `Page.captureScreenshot`, which renders beyond the viewport:
    /// `CapturePreview` takes only what is on screen.
    fn devtools_shot(webview: ICoreWebView2, clip: Option<(f64, f64, f64, f64)>) -> Picture {
        Box::pin(async move {
            let (x, y, width, height) = match clip {
                Some(clip) => clip,
                None => {
                    let metrics: Value = serde_json::from_str(
                        &page_agent::devtools(
                            &webview,
                            "Page.getLayoutMetrics",
                            serde_json::json!({}),
                        )
                        .await?,
                    )
                    .map_err(|error| error.to_string())?;
                    let size = &metrics["cssContentSize"];
                    (
                        0.,
                        0.,
                        size["width"].as_f64().unwrap_or_default(),
                        size["height"].as_f64().unwrap_or_default(),
                    )
                }
            };
            if width * height > MAX_PIXELS {
                return Err(format!(
                    "the picture would be {width:.0}×{height:.0} CSS pixels, past the \
                     {MAX_PIXELS:.0}-pixel limit; take an element by its ref"
                ));
            }
            let shot: Value = serde_json::from_str(
                &page_agent::devtools(
                    &webview,
                    "Page.captureScreenshot",
                    serde_json::json!({
                        "format": "png",
                        "captureBeyondViewport": true,
                        "clip": { "x": x, "y": y, "width": width, "height": height, "scale": 1 },
                    }),
                )
                .await?,
            )
            .map_err(|error| error.to_string())?;
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(shot["data"].as_str().unwrap_or_default())
                .map_err(|error| format!("DevTools returned an unreadable picture: {error}"))
        })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt as _, gio};
    use wry::WebViewExtUnix as _;

    use super::*;

    pub(super) fn capture(view: &Entity<WebView>, region: Region, cx: &App) -> Picture {
        let (snapshot, crop) = match region {
            Region::Viewport => (SnapshotRegion::Visible, None),
            Region::Element(element) => match element.visible() {
                Some(visible) => (SnapshotRegion::Visible, Some(visible)),
                None => return ready(Err(NO_AREA.into())),
            },
            Region::FullPage => (SnapshotRegion::FullDocument, None),
        };
        let (tx, picture) = delivered();
        view.read(cx).raw().webview().snapshot(
            snapshot,
            SnapshotOptions::NONE,
            None::<&gio::Cancellable>,
            move |result| {
                let _ = tx.try_send(
                    result
                        .map_err(|error| format!("WebKitGTK snapshot failed: {error}"))
                        .and_then(|surface| encode(&surface, crop)),
                );
            },
        );
        picture
    }

    /// The snapshot as PNG, cut to `crop` (CSS pixels, the snapshot's user
    /// space).
    fn encode(
        surface: &cairo::Surface,
        crop: Option<(f64, f64, f64, f64)>,
    ) -> Result<Vec<u8>, String> {
        let failed = |error: cairo::Error| format!("cannot encode the screenshot: {error}");
        let mut bytes = Vec::new();
        match crop {
            None => surface
                .write_to_png(&mut bytes)
                .map_err(|error| format!("cannot encode the screenshot: {error}"))?,
            Some((x, y, width, height)) => {
                let image = cairo::ImageSurface::create(
                    cairo::Format::ARgb32,
                    width.ceil() as i32,
                    height.ceil() as i32,
                )
                .map_err(failed)?;
                let context = cairo::Context::new(&image).map_err(failed)?;
                context
                    .set_source_surface(surface, -x, -y)
                    .map_err(failed)?;
                context.paint().map_err(failed)?;
                drop(context);
                image
                    .write_to_png(&mut bytes)
                    .map_err(|error| format!("cannot encode the screenshot: {error}"))?;
            }
        }
        if bytes.len() as f64 > MAX_PIXELS * 4. {
            return Err("the screenshot is too large to send".into());
        }
        Ok(bytes)
    }
}
