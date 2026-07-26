use std::{
    borrow::Cow,
    cell::RefCell,
    ops::Range,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::oneshot;
use gpui::{
    Action, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, ClipboardItem, CursorStyle,
    DispatchEventResult, DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem,
    OwnedMenu, PathPromptOptions, Platform, PlatformDispatcher, PlatformDisplay,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow,
    ScreenCaptureSource, Task, ThermalState, TouchEvent, TouchId, TouchPhase, WindowAppearance,
    WindowInsets, WindowKind, WindowParams, point, popup::PopupNotSupportedError, px,
};
use gpui_wgpu::{CosmicTextSystem, GpuContext};

use crate::{
    AndroidDisplayMetrics, AndroidHost, AndroidSurface,
    dispatcher::AndroidDispatcher,
    display::AndroidDisplay,
    keyboard::AndroidKeyboardLayout,
    window::{AndroidWindow, AndroidWindowInner},
};

static BUNDLED_FONTS: &[&[u8]] = &[
    include_bytes!("../../../assets/fonts/DMSans[wght].ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-Italic.ttf"),
    include_bytes!("../../../assets/fonts/lilex/Lilex-BoldItalic.ttf"),
];

#[derive(Default)]
struct AndroidPlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    system_wake: Option<Box<dyn FnMut()>>,
    lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
    memory_warning: Option<Box<dyn FnMut()>>,
    thermal_state_change: Option<Box<dyn FnMut()>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
}

struct AndroidPlatformState {
    surface: Option<AndroidSurface>,
    active_window: Option<AnyWindowHandle>,
    window: Option<Weak<AndroidWindowInner>>,
    appearance: WindowAppearance,
    thermal_state: ThermalState,
    callbacks: AndroidPlatformCallbacks,
}

/// GPUI's Android platform implementation.
pub struct AndroidPlatform {
    host: Arc<dyn AndroidHost>,
    dispatcher: Arc<AndroidDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    display: Rc<AndroidDisplay>,
    state: Rc<RefCell<AndroidPlatformState>>,
    gpu_context: GpuContext,
}

impl AndroidPlatform {
    /// Creates the backend on Android's UI thread with the current surface generation.
    pub fn new(host: Arc<dyn AndroidHost>, surface: AndroidSurface) -> Result<Self> {
        surface.metrics.validate()?;

        let dispatcher = Arc::new(AndroidDispatcher::new(host.clone())?);
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());

        let text_system = Arc::new(CosmicTextSystem::new_without_system_fonts("DM Sans"));
        text_system.add_fonts(
            BUNDLED_FONTS
                .iter()
                .map(|bytes| Cow::Borrowed(*bytes))
                .collect(),
        )?;
        let text_system: Arc<dyn PlatformTextSystem> = text_system;

        let display = Rc::new(AndroidDisplay::new(surface.metrics));
        let appearance = host.window_appearance();
        let thermal_state = host.thermal_state();

        Ok(Self {
            host,
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            display,
            state: Rc::new(RefCell::new(AndroidPlatformState {
                surface: Some(surface),
                active_window: None,
                window: None,
                appearance,
                thermal_state,
                callbacks: AndroidPlatformCallbacks::default(),
            })),
            gpu_context: Rc::new(RefCell::new(None)),
        })
    }

    /// Returns the inbound JNI/NDK event boundary for the owning activity.
    pub fn event_sink(&self) -> AndroidEventSink {
        AndroidEventSink {
            host: self.host.clone(),
            dispatcher: self.dispatcher.clone(),
            display: self.display.clone(),
            state: self.state.clone(),
        }
    }
}

/// Main-thread entry points called by the Android activity and Choreographer.
///
/// This type is deliberately `!Send`: Android UI, lifecycle, input, and surface
/// callbacks must enter GPUI on the same thread that constructed the platform.
#[derive(Clone)]
pub struct AndroidEventSink {
    host: Arc<dyn AndroidHost>,
    dispatcher: Arc<AndroidDispatcher>,
    display: Rc<AndroidDisplay>,
    state: Rc<RefCell<AndroidPlatformState>>,
}

impl AndroidEventSink {
    /// Drains foreground tasks after `AndroidHost::wake_main_thread`.
    pub fn drain_main_thread(&self) {
        self.dispatcher.drain_main_thread();
    }

    /// Replaces a surface after Android creates a new `ANativeWindow`.
    pub fn surface_created(&self, surface: AndroidSurface) -> Result<()> {
        self.assert_main_thread();
        surface.metrics.validate()?;

        if let Some(window) = self.window() {
            window.replace_surface(surface.clone())?;
        }
        self.display.update(surface.metrics);
        self.state.borrow_mut().surface = Some(surface);

        if self
            .window()
            .is_some_and(|window| window.should_schedule_frames())
        {
            self.host.request_frame();
        }
        Ok(())
    }

    /// Unconfigures wgpu before Android releases the current native window.
    pub fn surface_destroyed(&self) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            window.detach_surface();
        }
        self.state.borrow_mut().surface = None;
    }

    /// Applies a native surface resize or density change.
    pub fn resized(&self, metrics: AndroidDisplayMetrics) -> Result<()> {
        self.assert_main_thread();
        metrics.validate()?;
        if let Some(surface) = self.state.borrow_mut().surface.as_mut() {
            surface.metrics = metrics;
        }
        self.display.update(metrics);
        if let Some(window) = self.window() {
            window.resize_from_native(metrics);
        }
        Ok(())
    }

    /// Delivers one Choreographer frame and schedules the next while visible.
    pub fn frame(&self) {
        self.assert_main_thread();
        let Some(window) = self.window() else {
            return;
        };
        window.frame();
        if window.should_schedule_frames() {
            self.host.request_frame();
        }
    }

    /// Delivers an already translated GPUI input event.
    pub fn dispatch_input(&self, input: gpui::PlatformInput) -> Option<DispatchEventResult> {
        self.assert_main_thread();
        self.window()
            .and_then(|window| window.dispatch_input(input))
    }

    /// Converts a physical Android touch into logical coordinates and mouse input.
    ///
    /// GPUI core does not yet dispatch `TouchEvent` to elements, so the primary
    /// contact is synthesized as left-button mouse input for the mobile MVP.
    pub fn touch(
        &self,
        id: u64,
        phase: TouchPhase,
        x_px: f32,
        y_px: f32,
        force: Option<f32>,
    ) -> Option<DispatchEventResult> {
        self.assert_main_thread();
        let window = self.window()?;
        let scale = window.scale_factor();
        window.dispatch_touch(TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(x_px / scale), px(y_px / scale)),
            force,
        })
    }

    /// Updates focus from Android lifecycle or pointer-device callbacks.
    pub fn active_changed(&self, active: bool) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            let changed = window.set_active(active);
            if changed && active && window.should_schedule_frames() {
                // A surface can survive pause/resume, so restart the frame chain explicitly.
                self.host.request_frame();
            }
        }
    }

    /// Updates hover state for attached mouse or stylus devices.
    pub fn hovered_changed(&self, hovered: bool) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            window.set_hovered(hovered);
        }
    }

    /// Notifies GPUI that Android moved the activity to another display.
    pub fn moved(&self) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            window.moved();
        }
    }

    /// Runs GPUI's registered window-control hit test, if any.
    pub fn hit_test_window_control(&self) -> Option<gpui::WindowControlArea> {
        self.assert_main_thread();
        self.window()
            .and_then(|window| window.hit_test_window_control())
    }

    /// Delivers Android's application lifecycle vocabulary.
    pub fn lifecycle_changed(&self, phase: AppLifecyclePhase) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            let active = matches!(phase, AppLifecyclePhase::Active);
            let changed = window.set_active(active);
            if changed && active && window.should_schedule_frames() {
                // Android may resume without recreating the surface.
                self.host.request_frame();
            }
        }
        if let Some(callback) = self.state.borrow_mut().callbacks.lifecycle.as_mut() {
            callback(phase);
        }
    }

    /// Delivers `onTrimMemory`/memory-pressure notification.
    pub fn memory_warning(&self) {
        self.assert_main_thread();
        if let Some(callback) = self.state.borrow_mut().callbacks.memory_warning.as_mut() {
            callback();
        }
    }

    /// Delivers animated safe-area and IME inset updates in logical pixels.
    pub fn insets_changed(&self, insets: WindowInsets) {
        self.assert_main_thread();
        if let Some(window) = self.window() {
            window.set_insets(insets);
        }
    }

    /// Runs GPUI's back callback. Returns false when Android should use its default.
    pub fn back_pressed(&self) -> bool {
        self.assert_main_thread();
        self.window().is_some_and(|window| window.back_pressed())
    }

    /// Consults GPUI's close veto, delivers close, and finishes the activity.
    pub fn close_requested(&self) -> bool {
        self.assert_main_thread();
        if let Some(window) = self.window()
            && !window.close_requested()
        {
            return false;
        }

        {
            let mut state = self.state.borrow_mut();
            state.active_window = None;
            state.window = None;
        }
        self.host.finish_activity();
        true
    }

    /// Delivers Android UI-mode changes.
    pub fn appearance_changed(&self, appearance: WindowAppearance) {
        self.assert_main_thread();
        self.state.borrow_mut().appearance = appearance;
        if let Some(window) = self.window() {
            window.set_appearance(appearance);
        }
    }

    /// Delivers Android thermal-status changes.
    pub fn thermal_state_changed(&self, thermal_state: ThermalState) {
        self.assert_main_thread();
        let changed = self.state.borrow().thermal_state != thermal_state;
        self.state.borrow_mut().thermal_state = thermal_state;
        if changed
            && let Some(callback) = self
                .state
                .borrow_mut()
                .callbacks
                .thermal_state_change
                .as_mut()
        {
            callback();
        }
    }

    /// Delivers Android keyboard-layout changes.
    pub fn keyboard_layout_changed(&self) {
        self.assert_main_thread();
        if let Some(callback) = self
            .state
            .borrow_mut()
            .callbacks
            .keyboard_layout_change
            .as_mut()
        {
            callback();
        }
    }

    /// Delivers VIEW intents registered by the Android manifest.
    pub fn open_urls(&self, urls: Vec<String>) {
        self.assert_main_thread();
        if let Some(callback) = self.state.borrow_mut().callbacks.open_urls.as_mut() {
            callback(urls);
        }
    }

    /// Delivers a host-driven reopen request.
    pub fn reopen(&self) {
        self.assert_main_thread();
        if let Some(callback) = self.state.borrow_mut().callbacks.reopen.as_mut() {
            callback();
        }
    }

    /// Delivers a system-wake notification.
    pub fn system_wake(&self) {
        self.assert_main_thread();
        if let Some(callback) = self.state.borrow_mut().callbacks.system_wake.as_mut() {
            callback();
        }
    }

    /// Commits text from Android's InputConnection.
    pub fn ime_replace_text(&self, range: Option<Range<usize>>, text: &str) -> bool {
        self.assert_main_thread();
        self.window()
            .is_some_and(|window| window.ime_replace_text(range, text))
    }

    /// Updates composing text from Android's InputConnection.
    pub fn ime_set_composing_text(
        &self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
    ) -> bool {
        self.assert_main_thread();
        self.window()
            .is_some_and(|window| window.ime_set_composing_text(range, text, selected_range))
    }

    /// Finishes the current IME composition.
    pub fn ime_finish_composing(&self) -> bool {
        self.assert_main_thread();
        self.window()
            .is_some_and(|window| window.ime_finish_composing())
    }

    fn window(&self) -> Option<Rc<AndroidWindowInner>> {
        self.state.borrow().window.as_ref().and_then(Weak::upgrade)
    }

    fn assert_main_thread(&self) {
        assert!(
            self.dispatcher.is_main_thread(),
            "Android event delivered outside the main thread"
        );
    }
}

impl Platform for AndroidPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        assert!(
            self.dispatcher.is_main_thread(),
            "AndroidPlatform::run must be called on the activity UI thread"
        );
        // Android's Looper already owns the event loop; GPUI launches inside it.
        on_finish_launching();
    }

    fn quit(&self) {
        if let Some(callback) = self.state.borrow_mut().callbacks.quit.as_mut() {
            callback();
        }
        self.host.finish_activity();
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        // Android packages cannot restart by executing a replacement binary path.
        log::warn!("Android does not support Platform::restart");
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        // Android owns activity activation through its lifecycle.
    }

    fn hide(&self) {
        // Android activities are backgrounded by the OS, not hidden as windows.
    }

    fn hide_other_apps(&self) {
        // Android apps cannot hide other applications.
    }

    fn unhide_other_apps(&self) {
        // Android apps cannot change other applications' visibility.
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.state.borrow().active_window
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        self.state.borrow().active_window.map(|window| vec![window])
    }

    fn is_screen_capture_supported(&self) -> bool {
        false
    }

    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<Result<Vec<Rc<dyn ScreenCaptureSource>>>> {
        // Android MediaProjection is consent/activity-result based and not this synchronous API.
        unsupported_receiver("screen capture is not supported by gpui-android")
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        if matches!(&params.kind, WindowKind::AnchoredPopup(_)) {
            return Err(PopupNotSupportedError.into());
        }
        if !matches!(&params.kind, WindowKind::Normal) {
            bail!("Android currently supports only its single normal activity window");
        }
        if self
            .state
            .borrow()
            .window
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some()
        {
            bail!("Android currently supports only one GPUI platform window");
        }

        let surface = self
            .state
            .borrow()
            .surface
            .clone()
            .context("Android native surface is not currently available")?;
        let appearance = self.state.borrow().appearance;
        let window = AndroidWindow::new(
            self.host.clone(),
            self.gpu_context.clone(),
            self.display.clone(),
            surface,
            appearance,
        )?;

        {
            let mut state = self.state.borrow_mut();
            state.active_window = Some(handle);
            state.window = Some(Rc::downgrade(&window.inner));
        }
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.state.borrow().appearance
    }

    fn open_url(&self, url: &str) {
        if let Err(error) = self.host.open_uri(url) {
            log::warn!("failed to open Android URI {url:?}: {error:#}");
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.state.borrow_mut().callbacks.open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        // Android URL schemes are declared statically as manifest intent filters.
        Task::ready(Err(anyhow!(
            "Android URL schemes must be registered in AndroidManifest.xml"
        )))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        // Android's Storage Access Framework requires an Activity result contract.
        unsupported_receiver("file selection is not supported by gpui-android")
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        // Android's Storage Access Framework returns content URIs, not writable Paths.
        unsupported_receiver("new-path prompts are not supported by gpui-android")
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {
        // Android has no system file-manager reveal operation for arbitrary Paths.
    }

    fn open_with_system(&self, path: &Path) {
        if let Err(error) = self.host.open_path(path) {
            log::warn!("failed to open Android path {path:?}: {error:#}");
        }
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.reopen = Some(callback);
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.system_wake = Some(callback);
    }

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.state.borrow_mut().callbacks.lifecycle = Some(callback);
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.memory_warning = Some(callback);
    }

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {
        // Android has no process-wide desktop menu bar.
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        // Android has no process-wide desktop menu bar to query.
        None
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // Android has no desktop dock menu.
    }

    fn perform_dock_menu_action(&self, _action: usize) {
        // Android has no desktop dock menu action source.
    }

    fn add_recent_document(&self, _path: &Path) {
        // Android's recents screen tracks activities, not application documents.
    }

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {
        // Android has no GPUI app-menu action source.
    }

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {
        // Android has no process-wide application menu.
    }

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        // Android has no process-wide application-menu commands to validate.
    }

    fn thermal_state(&self) -> ThermalState {
        self.state.borrow().thermal_state
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.thermal_state_change = Some(callback);
    }

    fn compositor_name(&self) -> &'static str {
        "Android SurfaceFlinger"
    }

    fn app_path(&self) -> Result<PathBuf> {
        // Android applications are APKs, not executable filesystem bundles.
        bail!("application paths are not available on Android")
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        // Android packages cannot ship independently executed auxiliary binaries.
        bail!("auxiliary executable paths are not available on Android")
    }

    fn set_cursor_style(&self, _style: CursorStyle) {
        // The mobile MVP has touch input and no application-controlled cursor.
    }

    fn hide_cursor_until_mouse_moves(&self) {
        // The mobile MVP has no visible mouse cursor to hide.
    }

    fn is_cursor_visible(&self) -> bool {
        false
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        match self.host.read_clipboard_text() {
            Ok(Some(text)) => Some(ClipboardItem::new_string(text)),
            Ok(None) => None,
            Err(error) => {
                log::warn!("failed to read Android clipboard: {error:#}");
                None
            }
        }
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        let Some(text) = item.text() else {
            // Android host seam currently supports GPUI's plain-text clipboard entries only.
            log::warn!("gpui-android cannot write a non-text clipboard item");
            return;
        };
        if let Err(error) = self.host.write_clipboard_text(&text) {
            log::warn!("failed to write Android clipboard: {error:#}");
        }
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        Task::ready(self.host.write_credentials(url, username, password))
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(self.host.read_credentials(url))
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        Task::ready(self.host.delete_credentials(url))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().callbacks.keyboard_layout_change = Some(callback);
    }
}

fn unsupported_receiver<T>(message: &'static str) -> oneshot::Receiver<Result<T>> {
    let (sender, receiver) = oneshot::channel();
    sender.send(Err(anyhow!(message))).ok();
    receiver
}
