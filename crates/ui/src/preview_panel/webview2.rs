//! The Windows page: a WebView2 composition controller whose content is a
//! native surface of the window's composition, between GPUI's base content
//! and its overlays, as gpui-webview places a page on macOS and Linux.
//!
//! Gap: gpui-webview hosts WebView2 through lb-wry, which creates only the
//! windowed controller (`CreateCoreWebView2Controller`, a child HWND beneath
//! GPUI's topmost DirectComposition target), and lists Windows composition as
//! not yet done. Tcode drives WebView2 itself here; this module goes once
//! gpui-webview composes the page on Windows.
//!
//! A composition controller receives no window input. GPUI routes the
//! pointer over the page's surface to [`Page::input`], which forwards it with
//! `SendMouseInput`; WebView2's own focus window takes the keyboard and IME
//! once `MoveFocus` gives it focus.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, Bounds, Context, CursorStyle, DevicePixels, DispatchEventResult, DispatchPhase, Element,
    ElementId, Entity, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Modifiers,
    MouseButton, MouseDownEvent, NavigationDirection, Pixels, PlatformInput, Point, Render,
    ScrollDelta, Size, Style, Window, WindowCompositionSurface,
};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, CursorChangedEventHandler,
    ExecuteScriptCompletedHandler, FocusChangedEventHandler, NewWindowRequestedEventHandler,
};
use windows::Win32::Foundation::{E_POINTER, HWND, POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
    SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW,
};
use windows_core::{HSTRING, Interface as _, PCWSTR};

/// One WebView2 environment for every page of a lifecycle: the user-data
/// folder and browser arguments wry was given, created on first use.
pub(super) struct Environment {
    user_data: PathBuf,
    arguments: String,
    /// Removed with the environment: a proxied attachment's own profile.
    _temporary: Option<tempfile::TempDir>,
    created: async_lock::Mutex<Option<ICoreWebView2Environment>>,
}

impl Environment {
    pub(super) fn new(proxy: Option<&tcode_traverse::preview::ProxyEntry>) -> Result<Self, String> {
        // Proxy arguments include an attachment-specific loopback port.
        // WebView2 cannot share a user-data directory between environments
        // with different arguments, even while an old creation is finishing.
        let local_profile = crate::client_data_dir()
            .ok_or_else(|| "client data directory was not set at startup".to_string())?
            .join("WebView2");
        let temporary = if proxy.is_some() {
            Some(
                tempfile::Builder::new()
                    .prefix("tcode-preview-")
                    .tempdir()
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let user_data = temporary
            .as_ref()
            .map_or(local_profile, |directory| directory.path().to_path_buf());
        std::fs::create_dir_all(&user_data).map_err(|error| {
            format!(
                "failed to create WebView2 user-data directory {}: {error}",
                user_data.display()
            )
        })?;
        log::debug!(
            "preview: using WebView2 user-data directory {}",
            user_data.display()
        );
        Ok(Self {
            user_data,
            arguments: super::proxy::browser_arguments(proxy),
            _temporary: temporary,
            created: async_lock::Mutex::new(None),
        })
    }

    async fn get(&self) -> Result<ICoreWebView2Environment, String> {
        let mut created = self.created.lock().await;
        if let Some(environment) = created.as_ref() {
            return Ok(environment.clone());
        }
        let (tx, rx) = async_channel::bounded(1);
        let options = CoreWebView2EnvironmentOptions::default();
        unsafe {
            options.set_additional_browser_arguments(self.arguments.clone());
            options.set_language(user_language());
        }
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |result, environment| {
                let _ =
                    tx.try_send(result.and_then(|()| environment.ok_or_else(|| E_POINTER.into())));
                Ok(())
            },
        ));
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                &HSTRING::from(self.user_data.as_path()),
                &ICoreWebView2EnvironmentOptions::from(options),
                &handler,
            )
        }
        .map_err(|error| error.to_string())?;
        let environment = rx
            .recv()
            .await
            .map_err(|_| "WebView2 environment creation was cancelled".to_string())?
            .map_err(|error| error.to_string())?;
        *created = Some(environment.clone());
        Ok(environment)
    }
}

/// The user's UI language, which wry gives WebView2 for its own UI (context
/// menu, error pages).
fn user_language() -> String {
    use windows::Win32::Globalization::{
        GetUserDefaultUILanguage, LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME,
    };
    let mut name = [0u16; MAX_LOCALE_NAME as usize];
    let length = unsafe {
        LCIDToLocaleName(
            u32::from(GetUserDefaultUILanguage()),
            Some(&mut name),
            LOCALE_ALLOW_NEUTRAL_NAMES,
        )
    };
    let length = usize::try_from(length).unwrap_or(0).saturating_sub(1);
    String::from_utf16_lossy(&name[..length.min(name.len())])
}

/// Create a page's composition controller in `parent`, the GPUI window.
pub(super) async fn create(
    environment: Rc<Environment>,
    parent: HWND,
    private: bool,
) -> Result<ICoreWebView2CompositionController, String> {
    let environment = environment.get().await?;
    let (tx, rx) = async_channel::bounded(1);
    let handler = CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
        move |result, controller| {
            let _ = tx.try_send(result.and_then(|()| controller.ok_or_else(|| E_POINTER.into())));
            Ok(())
        },
    ));
    let started = (|| unsafe {
        match environment.cast::<ICoreWebView2Environment10>() {
            Ok(environment) => {
                let options = environment.CreateCoreWebView2ControllerOptions()?;
                options.SetIsInPrivateModeEnabled(private)?;
                environment
                    .CreateCoreWebView2CompositionControllerWithOptions(parent, &options, &handler)
            }
            Err(_) => environment
                .cast::<ICoreWebView2Environment3>()?
                .CreateCoreWebView2CompositionController(parent, &handler),
        }
    })();
    started.map_err(|error| error.to_string())?;
    rx.recv()
        .await
        .map_err(|_| "WebView2 controller creation was cancelled".to_string())?
        .map_err(|error| error.to_string())
}

/// Close a controller whose page was never installed.
pub(super) fn discard(controller: &ICoreWebView2CompositionController) {
    if let Ok(controller) = controller.cast::<ICoreWebView2Controller>() {
        let _ = unsafe { controller.Close() };
    }
}

/// The WebView2 objects of one page: what navigation, scripts and load
/// errors drive.
pub struct RawWebView {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}

impl RawWebView {
    fn new(controller: ICoreWebView2Controller) -> windows_core::Result<Self> {
        let webview = unsafe { controller.CoreWebView2()? };
        unsafe {
            let settings = webview.Settings()?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetAreDefaultContextMenusEnabled(true)?;
            settings.SetIsZoomControlEnabled(false)?;
            settings.SetAreDevToolsEnabled(true)?;
            if let Ok(settings) = settings.cast::<ICoreWebView2Settings5>() {
                settings.SetIsPinchZoomEnabled(false)?;
            }
            if let Ok(settings) = settings.cast::<ICoreWebView2Settings6>() {
                settings.SetIsSwipeNavigationEnabled(false)?;
            }
            // A page asking for a window gets none, as under wry.
            let mut token = 0i64;
            webview.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                    if let Some(args) = args {
                        args.SetHandled(true)?;
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            controller.SetIsVisible(false)?;
            // As wry's builder was given: a first navigation, so a script
            // run before the panel's own has a document.
            webview.Navigate(&HSTRING::from("about:blank"))?;
        }
        Ok(Self {
            controller,
            webview,
        })
    }

    pub(super) fn webview(&self) -> &ICoreWebView2 {
        &self.webview
    }

    pub fn load_url(&self, url: &str) -> Result<(), String> {
        unsafe { self.webview.Navigate(&HSTRING::from(url)) }.map_err(|error| error.to_string())
    }

    pub fn evaluate_script(&self, script: &str) -> Result<(), String> {
        self.evaluate_script_with_callback(script, |_| {})
    }

    /// Run `script`; `callback` gets its JSON-encoded result.
    pub fn evaluate_script_with_callback(
        &self,
        script: &str,
        callback: impl FnOnce(String) + 'static,
    ) -> Result<(), String> {
        let handler = ExecuteScriptCompletedHandler::create(Box::new(move |_, result| {
            callback(result);
            Ok(())
        }));
        unsafe { self.webview.ExecuteScript(&HSTRING::from(script), &handler) }
            .map_err(|error| error.to_string())
    }
}

impl Drop for RawWebView {
    fn drop(&mut self) {
        let _ = unsafe { self.controller.Close() };
    }
}

/// What a page was last placed at, in the window's device pixels.
#[derive(Clone, Copy, PartialEq)]
struct Placed {
    document: Bounds<DevicePixels>,
    /// The size WebView2 renders the page at: the document's, or larger.
    rendered: Size<DevicePixels>,
    portal: Bounds<DevicePixels>,
    shown: Bounds<DevicePixels>,
    scale: f32,
}

/// One page and its native surface, shared with the input and WebView2
/// callbacks that run outside GPUI's updates.
struct Page {
    raw: RawWebView,
    composition: ICoreWebView2CompositionController,
    surface: WindowCompositionSurface,
    parent: HWND,
    /// The page's bounds in the window, in logical pixels, as laid out.
    bounds: Cell<Bounds<Pixels>>,
    scale: Cell<f32>,
    placed: Cell<Option<Placed>>,
    /// How much larger the page is about to be while its size glides.
    ahead: Cell<Size<Pixels>>,
    /// Whether the surface is drawn: the page wanted and some of it unclipped.
    portal_shown: Cell<bool>,
    /// Whether GPUI last routed the pointer to the page.
    hovered: Cell<bool>,
    cursor: Cell<CursorStyle>,
    focused: Cell<bool>,
    /// Whether the press being dispatched reached the page.
    pressed: Cell<bool>,
    /// What a pixel scroll left below one wheel unit, per axis.
    wheel: Cell<Point<f32>>,
    /// Redraw the page's entity: a hover or cursor change.
    changed: RefCell<Option<Box<dyn Fn()>>>,
}

/// The GPUI owner of one page, which the shared lifecycle holds.
pub struct WebView {
    page: Rc<Page>,
    visible: bool,
    window: gpui::AnyWindowHandle,
    _moved: gpui::Subscription,
}

/// A page put on its surface, not yet owned by an entity.
pub(super) struct Prepared(Rc<Page>);

impl Prepared {
    pub(super) fn raw(&self) -> &RawWebView {
        &self.0.raw
    }
}

impl WebView {
    /// Put `controller`'s page on a new native surface of `window`.
    pub(super) fn prepare(
        controller: ICoreWebView2CompositionController,
        window: &mut Window,
    ) -> Result<Prepared, String> {
        let parent = parent_window(window)?;
        let raw = controller
            .cast::<ICoreWebView2Controller>()
            .and_then(RawWebView::new)
            .map_err(|error| error.to_string())?;
        let composition = window
            .enable_window_composition()
            .map_err(|error| format!("{error:#}"))?;
        let surface = match parked::take(window.window_handle()) {
            Some(surface) => surface,
            None => composition
                .create_native_surface()
                .map_err(|error| format!("{error:#}"))?,
        };
        let page = Rc::new(Page {
            raw,
            composition: controller,
            surface,
            parent,
            bounds: Cell::default(),
            scale: Cell::new(window.scale_factor()),
            placed: Cell::new(None),
            ahead: Cell::default(),
            portal_shown: Cell::new(false),
            hovered: Cell::new(false),
            cursor: Cell::new(CursorStyle::Arrow),
            focused: Cell::new(false),
            pressed: Cell::new(false),
            wheel: Cell::default(),
            changed: RefCell::new(None),
        });
        if let Err(error) = page.attach(&composition) {
            parked::park(window.window_handle(), page.surface.clone());
            return Err(error);
        }
        Ok(Prepared(page))
    }

    pub(super) fn new(prepared: Prepared, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Prepared(page) = prepared;
        let entity = cx.entity().downgrade();
        let async_cx = cx.to_async();
        *page.changed.borrow_mut() = Some(Box::new(move || {
            let entity = entity.clone();
            async_cx
                .spawn(async move |cx| {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                })
                .detach();
        }));
        // WebView2 places its popups (menus, pickers, IME candidates) from
        // the parent window's position, which moves without a layout.
        let moved = cx.observe_window_bounds(window, |view, _, _| {
            let _ = unsafe { view.page.raw.controller.NotifyParentWindowPositionChanged() };
        });
        Self {
            page,
            visible: false,
            window: window.window_handle(),
            _moved: moved,
        }
    }

    pub fn raw(&self) -> &RawWebView {
        &self.page.raw
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Where the page was last laid out, in window coordinates.
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.page.bounds.get()
    }

    /// Show or hide the page, keeping its document. A shown page is placed
    /// when its element is next prepainted.
    pub fn set_visible(&mut self, visible: bool) -> Result<(), String> {
        if self.visible == visible {
            return Ok(());
        }
        self.visible = visible;
        let page = &self.page;
        if !visible {
            page.hide_portal();
            page.focus_parent();
        }
        unsafe { page.raw.controller.SetIsVisible(visible) }.map_err(|error| error.to_string())
    }

    /// Give a hidden page the viewport it would have on screen, so a page
    /// nobody is looking at still lays out at a real size.
    pub(super) fn stage(&self, size: Size<Pixels>) {
        let page = &self.page;
        let size = size.to_device_pixels(page.scale.get());
        page.placed.set(None);
        let bounds = RECT {
            left: 0,
            top: 0,
            right: size.width.0,
            bottom: size.height.0,
        };
        if let Err(error) = unsafe { page.raw.controller.SetBounds(bounds) } {
            log::debug!("preview: failed to size a hidden page: {error}");
        }
    }

    /// While the page's size glides larger, render it at the size it is
    /// heading for: WebView2 renders a new size a frame or more after it is
    /// set, and the surface's clip, which follows at once, then reveals
    /// rendered page instead of what lies beneath it.
    pub(super) fn lead(&self, ahead: Size<Pixels>) {
        self.page.ahead.set(ahead);
    }

    pub fn back(&mut self) -> Result<(), String> {
        self.page.raw.evaluate_script("history.back();")
    }

    pub fn forward(&mut self) -> Result<(), String> {
        self.page.raw.evaluate_script("history.forward();")
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.page.hide_portal();
        self.page.focus_parent();
        let _ = unsafe { self.page.raw.controller.Close() };
        parked::park(self.window, self.page.surface.clone());
    }
}

/// Surfaces of released pages, kept hidden for the next page of the same
/// window.
///
/// Gap: gpui-fast-windows cannot remove a native surface. Its
/// `remove_surface` hides the surface first through `set_visible`, which
/// casts the surface's visual to `IDCompositionVisual3`; the visual comes from
/// a `DCompositionCreateDevice` device, whose visuals have no such interface
/// (E_NOINTERFACE), so the removal fails and is rolled back. A released
/// page's surface is therefore parked here, at no size, instead. Its rebind
/// after a lost device fails the same way, and no surface can leave the
/// composition: a window that has shown a page cannot recover from a lost
/// device, and gpui-fast-windows aborts.
mod parked {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use gpui::{AnyWindowHandle, WindowCompositionSurface};

    thread_local! {
        static PARKED: RefCell<HashMap<AnyWindowHandle, Vec<WindowCompositionSurface>>> =
            RefCell::new(HashMap::new());
    }

    pub(super) fn park(window: AnyWindowHandle, surface: WindowCompositionSurface) {
        PARKED.with_borrow_mut(|parked| parked.entry(window).or_default().push(surface));
    }

    pub(super) fn take(window: AnyWindowHandle) -> Option<WindowCompositionSurface> {
        PARKED.with_borrow_mut(|parked| parked.get_mut(&window)?.pop())
    }
}

pub(super) fn parent_window(window: &Window) -> Result<HWND, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window)
        .map_err(|error| error.to_string())?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as *mut _)),
        _ => Err("the preview window is not a Win32 window".into()),
    }
}

/// webview2-com and gpui-fast-windows bind different `windows` releases;
/// the portal's visual crosses between them as the COM pointer it is.
fn visual_from(handle: Box<dyn Any>) -> windows_core::Result<IDCompositionVisual> {
    let unknown = handle
        .downcast::<gpui_windows_core::IUnknown>()
        .map_err(|_| windows_core::Error::from(E_POINTER))?;
    let raw = gpui_windows_core::Interface::as_raw(&*unknown);
    // SAFETY: `raw` is a live IUnknown owned by `unknown` for this call; the
    // borrowed wrapper is cloned (AddRef) before `unknown` is dropped.
    let unknown = unsafe { windows_core::IUnknown::from_raw_borrowed(&raw) }
        .cloned()
        .ok_or_else(|| windows_core::Error::from(E_POINTER))?;
    unknown.cast()
}

impl Page {
    /// Make the surface's visual the page's root, and route input and the
    /// page's own events back here.
    fn attach(self: &Rc<Self>, composition: &gpui::WindowComposition) -> Result<(), String> {
        let describe = |error: windows_core::Error| error.to_string();
        let surface = self
            .surface
            .platform_surface()
            .map_err(|error| format!("{error:#}"))?;
        let visual = surface
            .platform_handle()
            .map_err(|error| format!("{error:#}"))
            .and_then(|handle| visual_from(handle).map_err(describe))?;
        unsafe {
            self.composition
                .SetRootVisualTarget(&visual)
                .map_err(describe)?;
            let controller = self
                .raw
                .controller
                .cast::<ICoreWebView2Controller3>()
                .map_err(describe)?;
            controller
                .SetShouldDetectMonitorScaleChanges(false)
                .map_err(describe)?;
            controller
                .SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS)
                .map_err(describe)?;
            controller
                .SetRasterizationScale(f64::from(self.scale.get()))
                .map_err(describe)?;
        }
        // Device loss gives the surface a new visual: the page moves to it.
        let page = Rc::downgrade(self);
        surface
            .set_compositor_recreated_callback(Rc::new(move |handle| {
                if let Some(page) = page.upgrade() {
                    page.recreated(handle)?;
                }
                Ok(())
            }))
            .map_err(|error| format!("{error:#}"))?;
        let page = Rc::downgrade(self);
        composition
            .on_input(self.surface.id(), move |event| match page.upgrade() {
                Some(page) => page.input(event),
                None => DispatchEventResult {
                    propagate: true,
                    default_prevented: false,
                },
            })
            .map_err(|error| format!("{error:#}"))?;
        self.observe().map_err(describe)
    }

    fn observe(self: &Rc<Self>) -> windows_core::Result<()> {
        let mut token = 0i64;
        let page = Rc::downgrade(self);
        let handler = CursorChangedEventHandler::create(Box::new(move |composition, _| {
            if let (Some(page), Some(composition)) = (page.upgrade(), composition) {
                let mut id = 0u32;
                unsafe { composition.SystemCursorId(&mut id)? };
                if page.cursor.replace(cursor_style(id)) != cursor_style(id) {
                    page.changed();
                }
            }
            Ok(())
        }));
        unsafe { self.composition.add_CursorChanged(&handler, &mut token)? };
        for focused in [true, false] {
            let page = Rc::downgrade(self);
            let handler = FocusChangedEventHandler::create(Box::new(move |_, _| {
                if let Some(page) = page.upgrade() {
                    page.focused.set(focused);
                }
                Ok(())
            }));
            unsafe {
                if focused {
                    self.raw.controller.add_GotFocus(&handler, &mut token)?;
                } else {
                    self.raw.controller.add_LostFocus(&handler, &mut token)?;
                }
            }
        }
        Ok(())
    }

    fn changed(&self) {
        if let Some(changed) = self.changed.borrow().as_ref() {
            changed();
        }
    }

    fn recreated(&self, handle: Box<dyn Any>) -> windows_core::Result<()> {
        let visual = visual_from(handle)?;
        unsafe { self.composition.SetRootVisualTarget(&visual)? };
        // The new visual is placed as the surface was; the clip this page
        // set on the old one goes again.
        self.clip(
            &visual,
            self.placed.get().filter(|_| self.portal_shown.get()),
        );
        self.commit();
        Ok(())
    }

    fn visual(&self) -> Option<IDCompositionVisual> {
        let handle = self
            .surface
            .platform_surface()
            .ok()?
            .platform_handle()
            .ok()?;
        visual_from(handle).ok()
    }

    /// Commit what this page set on the surface's visual, through the
    /// surface: it commits its device whenever it places the visual, and a
    /// root surface's parent is the window's origin.
    fn commit(&self) {
        if let Ok(surface) = self.surface.platform_surface()
            && let Err(error) = surface.set_parent_origin(Point::default())
        {
            log::debug!("preview: failed to commit a page's surface: {error:#}");
        }
    }

    /// Place the page at `document`, of which `shown` is unclipped, both in
    /// window coordinates. Called while the page's element is prepainted,
    /// so the surface stacks with it.
    fn place(&self, document: Bounds<Pixels>, shown: Bounds<Pixels>, window: &Window) {
        let scale = window.scale_factor();
        if shown.is_empty() {
            self.hide_portal();
            return;
        }
        if self.scale.replace(scale) != scale
            && let Ok(controller) = self.raw.controller.cast::<ICoreWebView2Controller3>()
            && let Err(error) = unsafe { controller.SetRasterizationScale(f64::from(scale)) }
        {
            log::debug!("preview: failed to rescale a page: {error}");
        }
        let rendered = (document.size + self.ahead.get()).to_device_pixels(scale);
        let document = document.to_device_pixels(scale);
        let shown = shown.to_device_pixels(scale);
        // Gap: gpui-fast-windows' surface clips at its own bounds and cannot
        // offset what it holds, and WebView2 draws a page at its root
        // visual's origin. A page clipped at its top or left is therefore
        // placed at its whole bounds, and the surface's visual clipped to
        // the part shown.
        let portal = if document.origin == shown.origin {
            shown
        } else {
            document
        };
        let placed = Placed {
            document,
            rendered,
            portal,
            shown,
            scale,
        };
        let moved = self.placed.replace(Some(placed)) != Some(placed);
        let was_shown = self.portal_shown.replace(true);
        if moved {
            let bounds = RECT {
                left: document.origin.x.0,
                top: document.origin.y.0,
                right: document.origin.x.0 + rendered.width.0,
                bottom: document.origin.y.0 + rendered.height.0,
            };
            if let Err(error) = unsafe { self.raw.controller.SetBounds(bounds) } {
                log::debug!("preview: failed to size a page: {error}");
            }
        }
        if moved || !was_shown {
            self.apply(portal, Some(placed));
        } else if let Ok(surface) = self.surface.platform_surface() {
            // Unchanged, but placed while this frame prepaints the page, so
            // the surface keeps stacking with it.
            let _ = surface.set_bounds(portal);
        }
    }

    /// Clip the surface's visual to `placed`'s part shown, or to nothing,
    /// and move the surface to `portal`, committing both together.
    fn apply(&self, portal: Bounds<DevicePixels>, placed: Option<Placed>) {
        let Ok(surface) = self.surface.platform_surface() else {
            return;
        };
        if let Some(visual) = self.visual() {
            self.clip(&visual, placed);
        }
        let moved = surface.bounds() != portal;
        if let Err(error) = surface.set_bounds(portal) {
            log::debug!("preview: failed to place a page's surface: {error:#}");
        }
        // The surface commits when it moves; otherwise the clip is
        // committed on its own.
        if !moved {
            self.commit();
        }
    }

    fn clip(&self, visual: &IDCompositionVisual, placed: Option<Placed>) {
        let rect = placed.map_or_else(D2D_RECT_F::default, |placed| {
            let left = (placed.shown.origin.x - placed.portal.origin.x).0 as f32;
            let top = (placed.shown.origin.y - placed.portal.origin.y).0 as f32;
            D2D_RECT_F {
                left,
                top,
                right: left + placed.shown.size.width.0 as f32,
                bottom: top + placed.shown.size.height.0 as f32,
            }
        });
        if let Err(error) = unsafe { visual.SetClip2(&rect) } {
            log::debug!("preview: failed to clip a page: {error}");
        }
    }

    /// Gap: gpui-fast-windows hides a surface through
    /// `IDCompositionVisual3::SetOpacity2`, which its `DCompositionCreateDevice`
    /// visuals do not implement (E_NOINTERFACE). A hidden page's surface is
    /// clipped to nothing and has no size instead, which also keeps GPUI from
    /// routing the pointer to it.
    fn hide_portal(&self) {
        if self.portal_shown.replace(false) {
            self.apply(Bounds::default(), None);
        }
    }

    /// Give the keyboard back to GPUI's window, as wry's `focus_parent`.
    fn focus_parent(&self) {
        if self.focused.replace(false) {
            let _ = unsafe { SetFocus(Some(self.parent)) };
        }
    }

    /// A point in the window, in the page's client pixels.
    fn client_point(&self, position: Point<Pixels>) -> POINT {
        let origin = self.bounds.get().origin;
        let scale = self.scale.get();
        POINT {
            x: (f32::from(position.x - origin.x) * scale).round() as i32,
            y: (f32::from(position.y - origin.y) * scale).round() as i32,
        }
    }

    fn send(
        &self,
        kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
        keys: COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
        data: i32,
        point: POINT,
    ) {
        if let Err(error) = unsafe {
            self.composition
                .SendMouseInput(kind, keys, data as u32, point)
        } {
            log::debug!("preview: failed to forward pointer input: {error}");
        }
    }

    /// The pointer GPUI routes to the page's surface: over it, or captured
    /// by a press that began on it.
    fn input(&self, event: &PlatformInput) -> DispatchEventResult {
        let handled = DispatchEventResult {
            propagate: false,
            default_prevented: false,
        };
        match event {
            PlatformInput::MouseMove(event) => {
                self.send(
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                    virtual_keys(&event.modifiers, event.pressed_button),
                    0,
                    self.client_point(event.position),
                );
                if !self.hovered.replace(true) {
                    self.changed();
                }
            }
            PlatformInput::MouseDown(event) => {
                let Some((kind, data)) = button_kind(event.button, Press::Down(event.click_count))
                else {
                    return handled;
                };
                self.pressed.set(true);
                if !self.focused.get()
                    && let Err(error) = unsafe {
                        self.raw
                            .controller
                            .MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)
                    }
                {
                    log::debug!("preview: failed to focus a page: {error}");
                }
                self.send(
                    kind,
                    virtual_keys(&event.modifiers, Some(event.button)),
                    data,
                    self.client_point(event.position),
                );
            }
            PlatformInput::MouseUp(event) => {
                let Some((kind, data)) = button_kind(event.button, Press::Up) else {
                    return handled;
                };
                self.send(
                    kind,
                    virtual_keys(&event.modifiers, None),
                    data,
                    self.client_point(event.position),
                );
            }
            PlatformInput::MouseExited(_) => {
                self.send(
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
                    0,
                    POINT::default(),
                );
                if self.hovered.replace(false) {
                    self.changed();
                }
            }
            PlatformInput::Scroll(event) => {
                let point = self.client_point(event.position);
                let keys = virtual_keys(&event.modifiers, None);
                for (kind, data) in self.wheel(&event.delta, event.modifiers.shift) {
                    self.send(kind, keys, data, point);
                }
            }
            _ => {
                return DispatchEventResult {
                    propagate: true,
                    default_prevented: false,
                };
            }
        }
        handled
    }

    /// GPUI's scroll back in wheel units: lines through the system's lines
    /// (or, sideways, characters) per notch, as GPUI derived them; pixels at
    /// Chromium's 100 pixels per notch.
    fn wheel(&self, delta: &ScrollDelta, shift: bool) -> Vec<(COREWEBVIEW2_MOUSE_EVENT_KIND, i32)> {
        const NOTCH: f32 = 120.;
        let (x, y) = match delta {
            ScrollDelta::Lines(lines) => (
                lines.x / system_wheel(SPI_GETWHEELSCROLLCHARS) * NOTCH,
                lines.y / system_wheel(SPI_GETWHEELSCROLLLINES) * NOTCH,
            ),
            ScrollDelta::Pixels(pixels) => {
                let carried = self.wheel.get();
                (
                    carried.x + f32::from(pixels.x) * NOTCH / 100.,
                    carried.y + f32::from(pixels.y) * NOTCH / 100.,
                )
            }
        };
        let (x_units, y_units) = (x.trunc(), y.trunc());
        self.wheel.set(Point::new(x - x_units, y - y_units));
        let mut events = Vec::new();
        if y_units != 0. {
            events.push((COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, y_units as i32));
        }
        if x_units != 0. {
            // Shift turns a vertical wheel sideways in GPUI; the page turns
            // it again from the same notch and modifier.
            events.push(if shift {
                (COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, x_units as i32)
            } else {
                (
                    COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
                    -x_units as i32,
                )
            });
        }
        events
    }
}

/// The system's wheel setting GPUI scales a notch by; a page-at-a-time
/// setting counts as the default three.
fn system_wheel(
    setting: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION,
) -> f32 {
    let mut value = 3u32;
    let read = unsafe {
        SystemParametersInfoW(
            setting,
            0,
            Some(std::ptr::from_mut(&mut value).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if read.is_err() || value == 0 || value == u32::MAX {
        3.
    } else {
        value as f32
    }
}

enum Press {
    Down(usize),
    Up,
}

/// The WebView2 event for a button going down or up, and its mouse data.
/// A second click is a double click, as Win32 reports it to a window.
fn button_kind(button: MouseButton, press: Press) -> Option<(COREWEBVIEW2_MOUSE_EVENT_KIND, i32)> {
    let double = matches!(press, Press::Down(2));
    let down = matches!(press, Press::Down(_));
    let kinds = match button {
        MouseButton::Left => [
            COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
            COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK,
            COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
        ],
        MouseButton::Right => [
            COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
            COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK,
            COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP,
        ],
        MouseButton::Middle => [
            COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
            COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK,
            COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
        ],
        MouseButton::Navigate(_) => [
            COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN,
            COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOUBLE_CLICK,
            COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
        ],
    };
    let kind = match (down, double) {
        (true, false) => kinds[0],
        (true, true) => kinds[1],
        (false, _) => kinds[2],
    };
    let data = match button {
        MouseButton::Navigate(NavigationDirection::Back) => 1,
        MouseButton::Navigate(NavigationDirection::Forward) => 2,
        _ => 0,
    };
    Some((kind, data))
}

/// The keys and buttons Win32 would report held with a mouse event.
fn virtual_keys(
    modifiers: &Modifiers,
    button: Option<MouseButton>,
) -> COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS {
    let mut keys = COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE;
    if modifiers.shift {
        keys |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_SHIFT;
    }
    if modifiers.control {
        keys |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_CONTROL;
    }
    keys |= match button {
        Some(MouseButton::Left) => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_LEFT_BUTTON,
        Some(MouseButton::Right) => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_RIGHT_BUTTON,
        Some(MouseButton::Middle) => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_MIDDLE_BUTTON,
        Some(MouseButton::Navigate(NavigationDirection::Back)) => {
            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON1
        }
        Some(MouseButton::Navigate(NavigationDirection::Forward)) => {
            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON2
        }
        None => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE,
    };
    keys
}

/// The page's system cursor as GPUI's, the inverse of the cursors
/// gpui-fast-windows loads for each style.
fn cursor_style(id: u32) -> CursorStyle {
    let is = |cursor: PCWSTR| cursor.0 as usize == id as usize;
    if is(IDC_IBEAM) {
        CursorStyle::IBeam
    } else if is(IDC_CROSS) {
        CursorStyle::Crosshair
    } else if is(IDC_HAND) {
        CursorStyle::PointingHand
    } else if is(IDC_SIZEWE) {
        CursorStyle::ResizeLeftRight
    } else if is(IDC_SIZENS) {
        CursorStyle::ResizeUpDown
    } else if is(IDC_SIZENWSE) {
        CursorStyle::ResizeUpLeftDownRight
    } else if is(IDC_SIZENESW) {
        CursorStyle::ResizeUpRightDownLeft
    } else if is(IDC_NO) {
        CursorStyle::OperationNotAllowed
    } else {
        CursorStyle::Arrow
    }
}

impl Render for WebView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        PageElement { view: cx.entity() }
    }
}

/// Where the page is laid out: it places the page's surface as it is
/// prepainted, and while the page is shown takes its cursor and gives the
/// keyboard back to GPUI on a press elsewhere.
struct PageElement {
    view: Entity<WebView>,
}

impl IntoElement for PageElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PageElement {
    type RequestLayoutState = ();
    type PrepaintState = bool;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: Size::full(),
            flex_shrink: 1.,
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let view = self.view.read(cx);
        let page = &view.page;
        page.bounds.set(bounds);
        if !view.visible {
            return false;
        }
        let shown = bounds.intersect(&window.content_mask().bounds);
        page.place(bounds, shown, window);
        !shown.is_empty()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        shown: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !*shown {
            return;
        }
        let page = self.view.read(cx).page.clone();
        if page.hovered.get() {
            window.set_window_cursor_style(page.cursor.get());
        }
        window.on_mouse_event(move |_: &MouseDownEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            page.pressed.set(false);
            if !page.focused.get() {
                return;
            }
            // Whether the press reached the page is known once it has
            // been dispatched, after GPUI's own listeners.
            let page = page.clone();
            cx.defer(move |_| {
                if !page.pressed.get() {
                    page.focus_parent();
                }
            });
        });
    }
}
