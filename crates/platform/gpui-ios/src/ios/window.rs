//! GPUI window backed by the Swift host's `CAMetalLayer` UIView.

use super::{IosDisplay, raw_handles::IosRawHandles};
use gpui::{
    Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, KeyDownEvent, KeyUpEvent,
    Keystroke, Modifiers, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, Size, TextInputConfiguration, TextInputStateChange, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowInsets, WindowParams, px,
    size,
};
use gpui_wgpu::{GpuContext, WgpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, UiKitDisplayHandle, UiKitWindowHandle, WindowHandle,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
};

type RequestFrameCallback = Box<dyn FnMut(RequestFrameOptions)>;
type InputCallback = Box<dyn FnMut(PlatformInput) -> DispatchEventResult>;
type BoolCallback = Box<dyn FnMut(bool)>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;
type UnitCallback = Box<dyn FnMut()>;
type ShouldCloseCallback = Box<dyn FnMut() -> bool>;
type HitTestCallback = Box<dyn FnMut() -> Option<WindowControlArea>>;
type InsetsCallback = Box<dyn FnMut(WindowInsets)>;

pub(crate) struct IosWindow {
    view: Cell<NonNull<c_void>>,
    raw_handles: RefCell<IosRawHandles>,
    gpu_context: GpuContext,
    surface_attached: Cell<bool>,
    bounds: Cell<Bounds<Pixels>>,
    scale_factor: Cell<f32>,
    appearance: Cell<WindowAppearance>,
    active: Cell<bool>,
    mouse_position: Cell<Point<Pixels>>,
    modifiers: Cell<Modifiers>,
    background: Cell<WindowBackgroundAppearance>,
    insets: RefCell<WindowInsets>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    renderer: RefCell<WgpuRenderer>,
    request_frame_callback: RefCell<Option<RequestFrameCallback>>,
    input_callback: RefCell<Option<InputCallback>>,
    active_callback: RefCell<Option<BoolCallback>>,
    hover_callback: RefCell<Option<BoolCallback>>,
    resize_callback: RefCell<Option<ResizeCallback>>,
    moved_callback: RefCell<Option<UnitCallback>>,
    should_close_callback: RefCell<Option<ShouldCloseCallback>>,
    hit_test_callback: RefCell<Option<HitTestCallback>>,
    close_callback: RefCell<Option<Box<dyn FnOnce()>>>,
    appearance_callback: RefCell<Option<UnitCallback>>,
    insets_callback: RefCell<Option<InsetsCallback>>,
    dispatching_input: Cell<bool>,
    pending_input: RefCell<VecDeque<PlatformInput>>,
    requesting_frame: Cell<bool>,
}

impl std::fmt::Debug for IosWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IosWindow")
            .field("view", &self.view.get())
            .field("bounds", &self.bounds.get())
            .field("scale_factor", &self.scale_factor.get())
            .finish_non_exhaustive()
    }
}

impl IosWindow {
    pub(crate) fn new(
        _handle: gpui::AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Self> {
        super::assert_main_thread();
        let metrics = super::ffi::host_metrics();
        let view = super::ffi::host_view()?;
        let logical_size = if metrics.width > 0.0 && metrics.height > 0.0 {
            size(px(metrics.width), px(metrics.height))
        } else {
            params.bounds.size
        };
        let scale_factor = metrics.scale.max(1.0);
        let raw_handles = Self::raw_handles(view);

        let descriptor = wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: Some(Box::new(raw_handles.clone())),
        };
        let instance = wgpu::Instance::new(descriptor);
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: None,
            raw_window_handle: raw_handles.window,
        };
        let bootstrap_surface = unsafe { instance.create_surface_unsafe(target) }?;
        let context = WgpuContext::new(instance, &bootstrap_surface, None)?;
        let surface_capabilities = bootstrap_surface.get_capabilities(&context.adapter);
        let surface_format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|format| surface_capabilities.formats.contains(format))
        .or_else(|| {
            surface_capabilities
                .formats
                .iter()
                .find(|format| !format.is_srgb())
                .copied()
        })
        .or_else(|| surface_capabilities.formats.first().copied())
        .ok_or_else(|| anyhow::anyhow!("iOS Metal surface reports no supported formats"))?;
        let format_features = context.adapter.get_texture_format_features(surface_format);
        let glass_usages = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST;
        let glass_enabled = format_features.allowed_usages.contains(glass_usages)
            && format_features
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE);
        drop(bootstrap_surface);
        let gpu_context = Rc::new(RefCell::new(Some(context)));
        let drawable_size = size(
            DevicePixels((f32::from(logical_size.width) * scale_factor).round() as i32),
            DevicePixels((f32::from(logical_size.height) * scale_factor).round() as i32),
        );
        let renderer = WgpuRenderer::new(
            gpu_context.clone(),
            &raw_handles,
            WgpuSurfaceConfig {
                size: drawable_size,
                transparent: false,
                preferred_present_mode: Some(wgpu::PresentMode::Fifo),
            },
            None,
        )?;
        log::info!(
            "GPUI iOS Metal surface format selected: {surface_format:?}; scale factor: \
             {scale_factor:.2}; glass enabled: {glass_enabled}"
        );

        Ok(Self {
            view: Cell::new(view),
            raw_handles: RefCell::new(raw_handles),
            gpu_context,
            surface_attached: Cell::new(true),
            bounds: Cell::new(Bounds::new(Default::default(), logical_size)),
            scale_factor: Cell::new(scale_factor),
            appearance: Cell::new(metrics.appearance),
            active: Cell::new(metrics.active),
            mouse_position: Cell::new(Point::default()),
            modifiers: Cell::new(Modifiers::default()),
            background: Cell::new(WindowBackgroundAppearance::Opaque),
            insets: RefCell::new(metrics.insets()),
            input_handler: RefCell::new(None),
            renderer: RefCell::new(renderer),
            request_frame_callback: RefCell::new(None),
            input_callback: RefCell::new(None),
            active_callback: RefCell::new(None),
            hover_callback: RefCell::new(None),
            resize_callback: RefCell::new(None),
            moved_callback: RefCell::new(None),
            should_close_callback: RefCell::new(None),
            hit_test_callback: RefCell::new(None),
            close_callback: RefCell::new(None),
            appearance_callback: RefCell::new(None),
            insets_callback: RefCell::new(None),
            dispatching_input: Cell::new(false),
            pending_input: RefCell::new(VecDeque::new()),
            requesting_frame: Cell::new(false),
        })
    }

    pub(crate) fn register(&self) {
        super::ffi::register_window(self);
    }

    fn raw_handles(view: NonNull<c_void>) -> IosRawHandles {
        IosRawHandles {
            window: RawWindowHandle::UiKit(UiKitWindowHandle::new(view)),
            display: RawDisplayHandle::UiKit(UiKitDisplayHandle::new()),
        }
    }

    pub(crate) fn needs_surface_replacement(&self, view: NonNull<c_void>) -> bool {
        !self.surface_attached.get() || self.view.get() != view
    }

    pub(crate) fn replace_surface_from_host(
        &self,
        view: NonNull<c_void>,
        width: f32,
        height: f32,
        scale_factor: f32,
    ) -> anyhow::Result<()> {
        let scale_factor = scale_factor.max(1.0);
        let raw_handles = Self::raw_handles(view);
        let instance = self
            .gpu_context
            .borrow()
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("iOS GPU context is unavailable"))?
            .instance
            .clone();
        let drawable_size = size(
            DevicePixels((width.max(1.0) * scale_factor).round() as i32),
            DevicePixels((height.max(1.0) * scale_factor).round() as i32),
        );
        let mut renderer = self.renderer.borrow_mut();
        renderer.replace_surface(
            &raw_handles,
            WgpuSurfaceConfig {
                size: drawable_size,
                transparent: self.background.get() != WindowBackgroundAppearance::Opaque,
                preferred_present_mode: Some(wgpu::PresentMode::Fifo),
            },
            &instance,
        )?;
        drop(renderer);

        self.view.set(view);
        *self.raw_handles.borrow_mut() = raw_handles;
        self.surface_attached.set(true);
        self.resize_from_host(width, height, scale_factor);
        self.request_frame(true);
        log::info!("GPUI iOS Metal surface replaced at scale factor {scale_factor:.2}");
        Ok(())
    }

    pub(crate) fn detach_surface_from_host(&self, view: NonNull<c_void>) {
        if self.view.get() != view || !self.surface_attached.replace(false) {
            return;
        }
        self.renderer.borrow_mut().unconfigure_surface();
    }

    pub(crate) fn request_frame(&self, force_render: bool) {
        if self.requesting_frame.replace(true) {
            return;
        }
        let mut callback = self.request_frame_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render,
            });
        }
        if self.request_frame_callback.borrow().is_none() {
            *self.request_frame_callback.borrow_mut() = callback;
        }
        self.requesting_frame.set(false);
    }

    pub(crate) fn dispatch_input(&self, input: PlatformInput) -> DispatchEventResult {
        if self.dispatching_input.replace(true) {
            self.pending_input.borrow_mut().push_back(input);
            return DispatchEventResult::default();
        }

        let mut first_result = None;
        let mut next = Some(input);
        while let Some(input) = next {
            let mut callback = self.input_callback.borrow_mut().take();
            let result = callback
                .as_mut()
                .map_or_else(DispatchEventResult::default, |callback| callback(input));
            if self.input_callback.borrow().is_none() {
                *self.input_callback.borrow_mut() = callback;
            }
            first_result.get_or_insert(result);
            next = self.pending_input.borrow_mut().pop_front();
        }
        self.dispatching_input.set(false);
        first_result.unwrap_or_default()
    }

    pub(crate) fn set_mouse_position(&self, position: Point<Pixels>) {
        self.mouse_position.set(position);
    }

    pub(crate) fn resize_from_host(&self, width: f32, height: f32, scale_factor: f32) {
        let size = size(px(width.max(1.0)), px(height.max(1.0)));
        let scale_factor = scale_factor.max(1.0);
        if self.bounds.get().size == size
            && (self.scale_factor.get() - scale_factor).abs() < f32::EPSILON
        {
            return;
        }

        self.bounds.set(Bounds::new(Default::default(), size));
        self.scale_factor.set(scale_factor);
        self.renderer.borrow_mut().update_drawable_size(gpui::size(
            DevicePixels((width * scale_factor).round().max(1.0) as i32),
            DevicePixels((height * scale_factor).round().max(1.0) as i32),
        ));
        let mut callback = self.resize_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(size, scale_factor);
        }
        if self.resize_callback.borrow().is_none() {
            *self.resize_callback.borrow_mut() = callback;
        }
        self.request_frame(true);
    }

    pub(crate) fn update_insets(&self, insets: WindowInsets) {
        if *self.insets.borrow() == insets {
            return;
        }
        *self.insets.borrow_mut() = insets.clone();
        let mut callback = self.insets_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(insets);
        }
        if self.insets_callback.borrow().is_none() {
            *self.insets_callback.borrow_mut() = callback;
        }
        self.request_frame(true);
    }

    pub(crate) fn update_active(&self, active: bool) {
        if self.active.replace(active) == active {
            return;
        }
        let mut callback = self.active_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(active);
        }
        if self.active_callback.borrow().is_none() {
            *self.active_callback.borrow_mut() = callback;
        }
    }

    pub(crate) fn update_appearance(&self, appearance: WindowAppearance) {
        if self.appearance.replace(appearance) == appearance {
            return;
        }
        let mut callback = self.appearance_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        if self.appearance_callback.borrow().is_none() {
            *self.appearance_callback.borrow_mut() = callback;
        }
        self.request_frame(true);
    }

    pub(crate) fn insert_text(&self, text: &str) {
        if let Some(handler) = self.input_handler.borrow_mut().as_mut() {
            handler.replace_text_in_range(None, text);
        }
        self.request_frame(true);
    }

    pub(crate) fn set_marked_text(&self, text: &str, start: usize, length: usize) {
        if let Some(handler) = self.input_handler.borrow_mut().as_mut() {
            handler.replace_and_mark_text_in_range(
                None,
                text,
                Some(start..start.saturating_add(length)),
            );
        }
        self.request_frame(true);
    }

    pub(crate) fn unmark_text(&self) {
        if let Some(handler) = self.input_handler.borrow_mut().as_mut() {
            handler.unmark_text();
        }
    }

    pub(crate) fn delete_backward(&self) {
        let keystroke = Keystroke {
            modifiers: self.modifiers.get(),
            key: "backspace".to_owned(),
            key_char: None,
        };
        self.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        }));
        self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        self.request_frame(true);
    }

    pub(crate) fn key_event(
        &self,
        key: String,
        key_char: Option<String>,
        modifiers: Modifiers,
        down: bool,
        repeat: bool,
    ) {
        self.modifiers.set(modifiers);
        let keystroke = Keystroke {
            modifiers,
            key,
            key_char,
        };
        let input = if down {
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held: repeat,
                prefer_character_input: true,
            })
        } else {
            PlatformInput::KeyUp(KeyUpEvent { keystroke })
        };
        self.dispatch_input(input);
        self.request_frame(true);
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        super::ffi::unregister_window(self);
        if let Some(callback) = self.close_callback.get_mut().take() {
            callback();
        }
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(self.raw_handles.borrow().window) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.raw_handles.borrow().display) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        self.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        super::ffi::host_configure_text_input(&configuration);
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        self.active.get()
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.background.get()
    }

    fn set_title(&mut self, _title: &str) {}

    fn set_background_appearance(&self, appearance: WindowBackgroundAppearance) {
        self.background.set(appearance);
        self.renderer
            .borrow_mut()
            .update_transparency(appearance != WindowBackgroundAppearance::Opaque);
    }

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.request_frame_callback.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.input_callback.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.active_callback.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.hover_callback.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.resize_callback.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.moved_callback.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.should_close_callback.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.hit_test_callback.borrow_mut() = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.close_callback.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.appearance_callback.borrow_mut() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let _ = self.renderer.borrow_mut().draw(scene);
    }

    fn schedule_frame(&self) {
        super::ffi::host_schedule_frame();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.borrow().sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.renderer.borrow().gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn insets(&self) -> WindowInsets {
        self.insets.borrow().clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        *self.insets_callback.borrow_mut() = Some(callback);
    }

    fn show_soft_keyboard(&self) {
        super::ffi::host_show_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        super::ffi::host_hide_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => super::ffi::host_show_keyboard(),
            TextInputStateChange::FocusLost => super::ffi::host_hide_keyboard(),
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {}
        }
    }
}
