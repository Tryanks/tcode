//! Convenience crate that re-exports GPUI's platform traits and the
//! `current_platform` constructor so consumers don't need `#[cfg]` gating.

pub use gpui::Platform;

use std::rc::Rc;

/// Returns a background executor for the current platform.
pub fn background_executor() -> gpui::BackgroundExecutor {
    current_platform(true).background_executor()
}

pub fn application() -> gpui::Application {
    #[cfg(target_family = "wasm")]
    {
        application_with_web_backend(gpui_web::WebBackendPreference::Auto)
    }

    #[cfg(not(target_family = "wasm"))]
    gpui::Application::with_platform(current_platform(false))
}

pub fn headless() -> gpui::Application {
    gpui::Application::with_platform(current_platform(true))
}

#[cfg(target_family = "wasm")]
pub use gpui_web::WebBackendPreference;

#[cfg(target_family = "wasm")]
pub fn application_with_web_backend(backend_preference: WebBackendPreference) -> gpui::Application {
    let platform = Rc::new(gpui_web::WebPlatform::new_with_backend(
        true,
        backend_preference,
    ));
    let http_client = std::sync::Arc::new(platform.fetch_http_client());
    gpui::Application::with_platform(platform).with_http_client(http_client)
}

/// Unlike `application`, this function returns a single-threaded web application.
#[cfg(target_family = "wasm")]
pub fn single_threaded_web() -> gpui::Application {
    let platform = Rc::new(gpui_web::WebPlatform::new(false));
    let http_client = std::sync::Arc::new(platform.fetch_http_client());
    gpui::Application::with_platform(platform).with_http_client(http_client)
}

/// Initializes panic hooks and logging for the web platform.
/// Call this before running the application in a wasm_bindgen entrypoint.
#[cfg(target_family = "wasm")]
pub fn web_init() {
    console_error_panic_hook::set_once();
    gpui_web::init_logging();
}

/// Returns the default [`Platform`] for the current OS.
pub fn current_platform(headless: bool) -> Rc<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Rc::new(gpui_macos::MacPlatform::new(headless))
    }

    #[cfg(target_os = "windows")]
    {
        Rc::new(
            gpui_windows::WindowsPlatform::new(headless)
                .expect("failed to initialize Windows platform"),
        )
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        gpui_linux::current_platform(headless)
    }

    #[cfg(target_family = "wasm")]
    {
        let _ = headless;
        Rc::new(gpui_web::WebPlatform::new(true))
    }

    // Mobile port: native backends live alongside this crate (gpui-ios,
    // crates/gpui-android). Both return the process-wide platform created by
    // the host entry point.
    #[cfg(target_os = "ios")]
    {
        let _ = headless;
        gpui_ios::platform()
    }

    #[cfg(target_os = "android")]
    {
        let _ = headless;
        gpui_android::platform()
    }
}

/// Returns a new [`HeadlessRenderer`] for the current platform, if available.
#[cfg(any(feature = "bench-support", feature = "test-support"))]
pub fn current_headless_renderer() -> Option<Box<dyn gpui::PlatformHeadlessRenderer>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(
            gpui_macos::metal_renderer::MetalHeadlessRenderer::new(),
        ))
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
