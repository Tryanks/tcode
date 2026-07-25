use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc};

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{
    Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel,
    RequestFrameOptions, Scene, Size, TextInputStateChange, TouchEvent, TouchId, TouchPhase,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowControls,
    WindowInsets,
};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};

use crate::{
    AndroidDisplayMetrics, AndroidHost, AndroidNativeWindow, AndroidSurface,
    display::AndroidDisplay,
};

type InputCallback = Box<dyn FnMut(PlatformInput) -> DispatchEventResult>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;
type HitTestCallback = Box<dyn FnMut() -> Option<WindowControlArea>>;

#[derive(Default)]
struct AndroidWindowCallbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<InputCallback>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<ResizeCallback>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    hit_test_window_control: Option<HitTestCallback>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
    insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
    back: Option<Box<dyn FnMut()>>,
}

struct AndroidWindowState {
    // Drop the renderer before either retained ANativeWindow lease.
    renderer: WgpuRenderer,
    native_window: Option<AndroidNativeWindow>,
    retired_native_window: Option<AndroidNativeWindow>,
    bounds: Bounds<Pixels>,
    scale_factor: f32,
    appearance: WindowAppearance,
    background_appearance: WindowBackgroundAppearance,
    title: String,
    input_handler: Option<PlatformInputHandler>,
    active: bool,
    hovered: bool,
    mouse_position: Point<Pixels>,
    modifiers: Modifiers,
    capslock: Capslock,
    pressed_button: Option<MouseButton>,
    primary_touch: Option<TouchId>,
    insets: WindowInsets,
    back_enabled: bool,
}

pub(crate) struct AndroidWindowInner {
    host: Arc<dyn AndroidHost>,
    gpu_context: GpuContext,
    state: RefCell<AndroidWindowState>,
    callbacks: RefCell<AndroidWindowCallbacks>,
}

pub(crate) struct AndroidWindow {
    pub(crate) inner: Rc<AndroidWindowInner>,
    display: Rc<AndroidDisplay>,
}

impl AndroidWindow {
    pub(crate) fn new(
        host: Arc<dyn AndroidHost>,
        gpu_context: GpuContext,
        display: Rc<AndroidDisplay>,
        surface: AndroidSurface,
        appearance: WindowAppearance,
    ) -> Result<Self> {
        let renderer = WgpuRenderer::new(
            gpu_context.clone(),
            &surface.window,
            surface_config(surface.metrics, false),
            None,
        )
        .context("failed to create Android wgpu renderer")?;

        let state = AndroidWindowState {
            renderer,
            native_window: Some(surface.window),
            retired_native_window: None,
            bounds: surface.metrics.logical_bounds(),
            scale_factor: surface.metrics.scale_factor,
            appearance,
            background_appearance: WindowBackgroundAppearance::Opaque,
            title: String::new(),
            input_handler: None,
            active: true,
            hovered: false,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
            pressed_button: None,
            primary_touch: None,
            insets: WindowInsets::default(),
            back_enabled: false,
        };

        Ok(Self {
            inner: Rc::new(AndroidWindowInner {
                host,
                gpu_context,
                state: RefCell::new(state),
                callbacks: RefCell::new(AndroidWindowCallbacks::default()),
            }),
            display,
        })
    }
}

impl AndroidWindowInner {
    pub(crate) fn replace_surface(&self, surface: AndroidSurface) -> Result<()> {
        let instance = self
            .gpu_context
            .borrow()
            .as_ref()
            .context("wgpu context missing while replacing Android surface")?
            .instance
            .clone();

        {
            let mut state = self.state.borrow_mut();
            let transparent = state.background_appearance != WindowBackgroundAppearance::Opaque;
            state.renderer.replace_surface(
                &surface.window,
                surface_config(surface.metrics, transparent),
                &instance,
            )?;
            state.native_window = Some(surface.window);
            state.retired_native_window = None;
            state.bounds = surface.metrics.logical_bounds();
            state.scale_factor = surface.metrics.scale_factor;
        }

        self.notify_resize(surface.metrics);
        Ok(())
    }

    pub(crate) fn detach_surface(&self) {
        let mut state = self.state.borrow_mut();
        if state.native_window.is_some() {
            state.renderer.unconfigure_surface();
            state.retired_native_window = state.native_window.take();
        }
    }

    pub(crate) fn resize_from_native(&self, metrics: AndroidDisplayMetrics) {
        {
            let mut state = self.state.borrow_mut();
            state.bounds = metrics.logical_bounds();
            state.scale_factor = metrics.scale_factor;
            if state.native_window.is_some() {
                state.renderer.update_drawable_size(Size {
                    width: DevicePixels(metrics.width_px as i32),
                    height: DevicePixels(metrics.height_px as i32),
                });
            }
        }
        self.notify_resize(metrics);
    }

    fn notify_resize(&self, metrics: AndroidDisplayMetrics) {
        if let Some(callback) = self.callbacks.borrow_mut().resize.as_mut() {
            callback(metrics.logical_size(), metrics.scale_factor);
        }
    }

    pub(crate) fn frame(&self) {
        if let Some(callback) = self.callbacks.borrow_mut().request_frame.as_mut() {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render: false,
            });
        }
    }

    pub(crate) fn should_schedule_frames(&self) -> bool {
        let state = self.state.borrow();
        state.active && state.native_window.is_some()
    }

    pub(crate) fn dispatch_input(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        {
            let mut state = self.state.borrow_mut();
            match &input {
                PlatformInput::MouseDown(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                    state.pressed_button = Some(event.button);
                }
                PlatformInput::MouseUp(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                    state.pressed_button = None;
                }
                PlatformInput::MouseMove(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                    state.pressed_button = event.pressed_button;
                }
                PlatformInput::ModifiersChanged(event) => {
                    state.modifiers = event.modifiers;
                    state.capslock = event.capslock;
                }
                PlatformInput::ScrollWheel(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                }
                PlatformInput::Pinch(event) => {
                    state.mouse_position = event.position;
                    state.modifiers = event.modifiers;
                }
                PlatformInput::KeyDown(_)
                | PlatformInput::KeyUp(_)
                | PlatformInput::MousePressure(_)
                | PlatformInput::MouseExited(_)
                | PlatformInput::FileDrop(_)
                | PlatformInput::Touch(_) => {}
            }
        }

        self.callbacks
            .borrow_mut()
            .input
            .as_mut()
            .map(|callback| callback(input))
    }

    pub(crate) fn dispatch_touch(&self, touch: TouchEvent) -> Option<DispatchEventResult> {
        let input = {
            let mut state = self.state.borrow_mut();
            match touch.phase {
                TouchPhase::Started if state.primary_touch.is_none() => {
                    state.primary_touch = Some(touch.id);
                    Some(PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: touch.position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                        first_mouse: false,
                    }))
                }
                TouchPhase::Moved if state.primary_touch == Some(touch.id) => {
                    Some(PlatformInput::MouseMove(MouseMoveEvent {
                        position: touch.position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    }))
                }
                TouchPhase::Ended | TouchPhase::Cancelled
                    if state.primary_touch == Some(touch.id) =>
                {
                    state.primary_touch = None;
                    // GPUI has no pointer-cancel mouse event yet; MouseUp is the
                    // only portable way to release captures for a cancelled touch.
                    Some(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: touch.position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    }))
                }
                TouchPhase::Started
                | TouchPhase::Moved
                | TouchPhase::Ended
                | TouchPhase::Cancelled => None,
            }
        };
        input.and_then(|input| self.dispatch_input(input))
    }

    pub(crate) fn scale_factor(&self) -> f32 {
        self.state.borrow().scale_factor
    }

    pub(crate) fn set_active(&self, active: bool) -> bool {
        let changed = {
            let mut state = self.state.borrow_mut();
            if state.active == active {
                false
            } else {
                state.active = active;
                true
            }
        };
        if changed && let Some(callback) = self.callbacks.borrow_mut().active_status_change.as_mut()
        {
            callback(active);
        }
        changed
    }

    pub(crate) fn set_hovered(&self, hovered: bool) {
        let changed = {
            let mut state = self.state.borrow_mut();
            if state.hovered == hovered {
                false
            } else {
                state.hovered = hovered;
                true
            }
        };
        if changed && let Some(callback) = self.callbacks.borrow_mut().hover_status_change.as_mut()
        {
            callback(hovered);
        }
    }

    pub(crate) fn moved(&self) {
        if let Some(callback) = self.callbacks.borrow_mut().moved.as_mut() {
            callback();
        }
    }

    pub(crate) fn hit_test_window_control(&self) -> Option<WindowControlArea> {
        self.callbacks
            .borrow_mut()
            .hit_test_window_control
            .as_mut()
            .and_then(|callback| callback())
    }

    pub(crate) fn close_requested(&self) -> bool {
        let should_close = self
            .callbacks
            .borrow_mut()
            .should_close
            .as_mut()
            .is_none_or(|callback| callback());
        if !should_close {
            return false;
        }

        if let Some(callback) = self.callbacks.borrow_mut().close.take() {
            callback();
        }
        true
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        let changed = self.state.borrow().appearance != appearance;
        self.state.borrow_mut().appearance = appearance;
        if changed && let Some(callback) = self.callbacks.borrow_mut().appearance_changed.as_mut() {
            callback();
        }
    }

    pub(crate) fn set_insets(&self, insets: WindowInsets) {
        let changed = self.state.borrow().insets != insets;
        self.state.borrow_mut().insets = insets.clone();
        if changed && let Some(callback) = self.callbacks.borrow_mut().insets_changed.as_mut() {
            callback(insets);
        }
    }

    pub(crate) fn back_pressed(&self) -> bool {
        if !self.state.borrow().back_enabled {
            return false;
        }
        if let Some(callback) = self.callbacks.borrow_mut().back.as_mut() {
            callback();
            true
        } else {
            false
        }
    }

    pub(crate) fn ime_replace_text(&self, range: Option<Range<usize>>, text: &str) -> bool {
        self.with_input_handler(|handler| handler.replace_text_in_range(range, text))
            .is_some()
    }

    pub(crate) fn ime_set_composing_text(
        &self,
        range: Option<Range<usize>>,
        text: &str,
        selected_range: Option<Range<usize>>,
    ) -> bool {
        self.with_input_handler(|handler| {
            handler.replace_and_mark_text_in_range(range, text, selected_range);
        })
        .is_some()
    }

    pub(crate) fn ime_finish_composing(&self) -> bool {
        self.with_input_handler(PlatformInputHandler::unmark_text)
            .is_some()
    }

    fn with_input_handler<T>(
        &self,
        callback: impl FnOnce(&mut PlatformInputHandler) -> T,
    ) -> Option<T> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = callback(&mut handler);
        self.state.borrow_mut().input_handler = Some(handler);
        Some(result)
    }
}

impl HasWindowHandle for AndroidWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = {
            let state = self.inner.state.borrow();
            let native_window = state
                .native_window
                .as_ref()
                .ok_or(HandleError::Unavailable)?;
            native_window.window_handle()?.as_raw()
        };

        // SAFETY: AndroidWindowInner retains the corresponding NativeWindow
        // lease, and all lifecycle mutation is confined to the UI thread.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for AndroidWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::android())
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.inner.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // Android owns activity surface dimensions; native resize callbacks update GPUI.
    }

    fn scale_factor(&self) -> f32 {
        self.inner.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.inner.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.inner.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.inner.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.inner.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.inner.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.inner.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // Android native prompts need an Activity result contract, absent from GPUI's sync API.
        None
    }

    fn activate(&self) {
        // Android, not an application window, owns foreground activation.
    }

    fn is_active(&self) -> bool {
        self.inner.state.borrow().active
    }

    fn is_hovered(&self) -> bool {
        self.inner.state.borrow().hovered
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.inner.state.borrow().background_appearance
    }

    fn set_title(&mut self, title: &str) {
        // Android task labels come from the manifest; retain the title for GPUI queries.
        self.inner.state.borrow_mut().title = title.to_owned();
    }

    fn set_background_appearance(&self, appearance: WindowBackgroundAppearance) {
        let mut state = self.inner.state.borrow_mut();
        state.background_appearance = appearance;
        if state.native_window.is_some() {
            // Reconfiguring the retired surface after surfaceDestroyed can hang Android drivers.
            state
                .renderer
                .update_transparency(appearance != WindowBackgroundAppearance::Opaque);
        }
    }

    fn minimize(&self) {
        // Android activity visibility is lifecycle-owned, not window-minimized.
    }

    fn zoom(&self) {
        // Android has no desktop maximize/zoom window operation.
    }

    fn toggle_fullscreen(&self) {
        // The activity surface is the backend's single full-screen GPUI window.
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.inner.callbacks.borrow_mut().request_frame = Some(callback);
        self.inner.host.request_frame();
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.inner.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.inner.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.inner.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.inner.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.inner.state.borrow_mut().renderer.draw(scene);
    }

    fn completed_frame(&self) {
        // WgpuRenderer presents the Android surface as part of draw.
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        // Android display rotation makes RGB stripe order unstable; use grayscale glyphs.
        false
    }

    fn get_title(&self) -> String {
        self.inner.state.borrow().title.clone()
    }

    fn window_controls(&self) -> WindowControls {
        WindowControls {
            fullscreen: false,
            maximize: false,
            minimize: false,
            window_menu: false,
        }
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.inner.state.borrow().renderer.gpu_specs())
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        self.inner.host.update_ime_position(bounds);
    }

    fn insets(&self) -> WindowInsets {
        self.inner.state.borrow().insets.clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.inner.callbacks.borrow_mut().insets_changed = Some(callback);
    }

    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().back = Some(callback);
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.inner.state.borrow_mut().back_enabled = enabled;
        self.inner.host.set_back_enabled(enabled);
    }

    fn show_soft_keyboard(&self) {
        self.inner.host.show_soft_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        self.inner.host.hide_soft_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        self.inner.host.text_input_state_changed(change);
    }
}

fn surface_config(metrics: AndroidDisplayMetrics, transparent: bool) -> WgpuSurfaceConfig {
    WgpuSurfaceConfig {
        size: Size {
            width: DevicePixels(metrics.width_px as i32),
            height: DevicePixels(metrics.height_px as i32),
        },
        transparent,
        // FIFO is universally available and remains valid across native-window replacement.
        preferred_present_mode: None,
    }
}
