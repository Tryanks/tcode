//! `gpui::Platform` for iOS.
//!
//! The division of labour mirrors `gpui-android`: [`IosPlatform`] is what GPUI
//! talks to, [`IosEventSink`] is what the UIKit shell calls, and the sink is
//! `!Send` so the compiler enforces that UIKit callbacks stay on the main
//! thread rather than leaving it to convention.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{
    AppLifecyclePhase, BackgroundExecutor, DevicePixels, DispatchEventResult, ForegroundExecutor,
    Pixels, PlatformDisplay, PlatformTextSystem, Point, Size, TouchEvent, TouchId, TouchPhase,
    WindowAppearance, point, px, size,
};
use gpui_wgpu::{CosmicTextSystem, GpuContext, WgpuRenderer, WgpuSurfaceConfig};

use crate::native::{IosDisplayMetrics, IosHost, IosSurface, metal_context};

/// The single display iOS exposes to an app.
///
/// iOS has no multi-monitor model an app can enumerate the way desktops do —
/// external displays arrive as separate scenes — so one display is the honest
/// answer rather than a simplification.
#[derive(Debug)]
pub struct IosDisplay {
    id: gpui::DisplayId,
    uuid: uuid::Uuid,
    bounds: gpui::Bounds<Pixels>,
}

impl IosDisplay {
    fn new(metrics: IosDisplayMetrics) -> Self {
        let scale = metrics.scale_factor();
        Self {
            id: gpui::DisplayId::new(0),
            // Stable for the process: GPUI uses it to correlate windows with
            // displays, and iOS gives no persistent identifier of its own.
            uuid: uuid::Uuid::new_v4(),
            bounds: gpui::Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(
                    px(metrics.width_px() as f32 / scale),
                    px(metrics.height_px() as f32 / scale),
                ),
            },
        }
    }
}

impl PlatformDisplay for IosDisplay {
    fn id(&self) -> gpui::DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> gpui::Bounds<Pixels> {
        self.bounds
    }
}

/// State shared between the platform, its window, and the event sink.
struct Shared {
    host: Arc<dyn IosHost>,
    surface: Option<IosSurface>,
    renderer: Option<WgpuRenderer>,
    gpu: GpuContext,
    metrics: IosDisplayMetrics,
    active: bool,
}

impl Shared {
    /// Build (or rebuild) the renderer against the current surface.
    ///
    /// Separated from construction because UIKit destroys and recreates the
    /// drawable across backgrounding and rotation, and the surface-replacement
    /// path must not rebuild the wgpu device — losing the device would discard
    /// every uploaded glyph and texture for no reason.
    fn configure_renderer(&mut self) -> Result<()> {
        let surface = self
            .surface
            .context("cannot configure a renderer without a drawable surface")?;
        let config = WgpuSurfaceConfig {
            size: Size {
                width: DevicePixels(surface.metrics().width_px() as i32),
                height: DevicePixels(surface.metrics().height_px() as i32),
            },
            transparent: false,
            // FIFO, not Mailbox. The field's own docs suggest mobile may prefer
            // triple buffering to avoid blocking during lifecycle transitions,
            // but `None` is the portable default and iOS suspends the display
            // link across those transitions anyway — so the block the hint
            // avoids does not arise. Revisit with a measurement, not a guess.
            preferred_present_mode: None,
        };
        let renderer = WgpuRenderer::new(self.gpu.clone(), &surface.layer(), config, None)
            .context("creating the Metal renderer")?;
        self.renderer = Some(renderer);
        Ok(())
    }
}

/// What the UIKit shell calls.
///
/// Deliberately `!Send` (it holds an `Rc`): UIKit delivers these on the main
/// thread, and the surface lifecycle depends on them being serialized against
/// each other. Making that a type error rather than a rule is the point.
pub struct IosEventSink {
    shared: Rc<RefCell<Shared>>,
}

impl IosEventSink {
    /// A new drawable is available — first launch, rotation, or return from
    /// background.
    ///
    /// The renderer is rebuilt against it while the wgpu device is preserved.
    pub fn surface_created(&self, surface: IosSurface) -> Result<()> {
        let mut shared = self.shared.borrow_mut();
        shared.metrics = surface.metrics();
        shared.surface = Some(surface);
        shared.configure_renderer()
    }

    /// UIKit is about to release the drawable.
    ///
    /// Must be called *before* the layer goes away: the renderer is dropped here
    /// so wgpu cannot present into a surface iOS has already reclaimed.
    pub fn surface_destroyed(&self) {
        let mut shared = self.shared.borrow_mut();
        shared.renderer = None;
        shared.surface = None;
    }

    /// The layer changed size — rotation, split view, or a keyboard inset.
    pub fn resized(&self, metrics: IosDisplayMetrics) -> Result<()> {
        let mut shared = self.shared.borrow_mut();
        shared.metrics = metrics;
        if let Some(surface) = shared.surface.as_mut() {
            *surface = IosSurface::new(surface.layer(), metrics);
        }
        if shared.surface.is_some() {
            shared.configure_renderer()?;
        }
        Ok(())
    }

    /// A `CADisplayLink` tick.
    pub fn frame(&self) {
        let active = self.shared.borrow().active;
        if !active {
            // Backgrounded apps must not draw: iOS terminates a process that
            // touches the GPU while suspended.
            return;
        }
        self.shared.borrow().host.request_frame();
    }

    /// Drain work GPUI queued from another thread.
    pub fn drain_main_thread(&self) {
        // The dispatcher owns the queue; this exists so the shell has one
        // symmetrical entry point per wake, matching gpui-android.
    }

    pub fn lifecycle_changed(&self, phase: AppLifecyclePhase) {
        let mut shared = self.shared.borrow_mut();
        shared.active = matches!(phase, AppLifecyclePhase::Active);
    }

    /// A touch, in physical pixels.
    ///
    /// Converted to logical units here because every UIKit coordinate arrives in
    /// points scaled by `contentScaleFactor`, and doing it once at the boundary
    /// keeps the conversion out of every call site.
    pub fn touch(&self, id: u64, phase: TouchPhase, x_px: f32, y_px: f32, force: Option<f32>) {
        let scale = self.shared.borrow().metrics.scale_factor();
        let _event = TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(x_px / scale), px(y_px / scale)),
            force,
        };
        // GPUI's core touch dispatch is still marked "implementation pending"
        // upstream, so a usable app has to synthesize mouse events the way
        // gpui_web does. Wiring that is the next milestone, not this one.
        let _ = DispatchEventResult::default();
    }
}

/// GPUI's view of iOS.
pub struct IosPlatform {
    shared: Rc<RefCell<Shared>>,
    text_system: Arc<CosmicTextSystem>,
    display: Rc<IosDisplay>,
    background: BackgroundExecutor,
    foreground: ForegroundExecutor,
}

impl IosPlatform {
    pub fn new(
        host: Arc<dyn IosHost>,
        surface: IosSurface,
        background: BackgroundExecutor,
        foreground: ForegroundExecutor,
    ) -> Result<Self> {
        let metrics = surface.metrics();
        // Metal is selected here, once, by pre-filling the slot the renderer
        // would otherwise populate with gpui_wgpu's VULKAN|GL default.
        let gpu = metal_context(&surface).context("building the Metal GPU context")?;

        let mut shared = Shared {
            host,
            surface: Some(surface),
            renderer: None,
            gpu,
            metrics,
            active: true,
        };
        shared.configure_renderer()?;

        Ok(Self {
            shared: Rc::new(RefCell::new(shared)),
            // Without system fonts the app must register its own, which tcode
            // already does — it bundles DM Sans and Lilex rather than relying on
            // whatever a platform happens to ship.
            // No system font database: iOS ships fonts, but GPUI's cosmic-text
            // path cannot enumerate them, and tcode bundles DM Sans and Lilex
            // regardless. The fallback name is what unresolved glyphs map to.
            text_system: Arc::new(CosmicTextSystem::new_without_system_fonts("DM Sans")),
            display: Rc::new(IosDisplay::new(metrics)),
            background,
            foreground,
        })
    }

    /// The half the UIKit shell retains.
    pub fn event_sink(&self) -> IosEventSink {
        IosEventSink {
            shared: self.shared.clone(),
        }
    }

    pub fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    pub fn primary_display(&self) -> Rc<dyn PlatformDisplay> {
        self.display.clone()
    }

    pub fn background_executor(&self) -> BackgroundExecutor {
        self.background.clone()
    }

    pub fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground.clone()
    }

    pub fn window_appearance(&self) -> WindowAppearance {
        self.shared.borrow().host.window_appearance()
    }

    /// Scale factor of the current drawable.
    pub fn scale_factor(&self) -> f32 {
        self.shared.borrow().metrics.scale_factor()
    }

    /// Logical size of the current drawable.
    pub fn content_size(&self) -> Size<Pixels> {
        let metrics = self.shared.borrow().metrics;
        let scale = metrics.scale_factor();
        size(
            px(metrics.width_px() as f32 / scale),
            px(metrics.height_px() as f32 / scale),
        )
    }

    /// Physical origin of the drawable. Always zero: an iOS app owns its whole
    /// scene and has no window position to report.
    pub fn origin(&self) -> Point<Pixels> {
        point(px(0.0), px(0.0))
    }
}
