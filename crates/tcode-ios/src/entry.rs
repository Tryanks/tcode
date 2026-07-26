//! The C entry points the Swift/Objective-C shell calls.
//!
//! Every function here must run on the main thread, for the same reason as the
//! Android side: `IosEventSink` is `!Send`, and the surface lifecycle depends on
//! create/destroy/frame being serialized against each other. Storing the runtime
//! thread-locally means a call arriving elsewhere finds nothing rather than
//! corrupting state.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_void};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use client_app::native_transport::NativeTransportFactory;
use client_app::{
    ClientApp, ClientConnection, ClientIdentity, ConnectionStore, FileStore, TransportFactory,
};
use gpui::{App, AppContext as _, AppLifecyclePhase, ApplicationHandle, TouchPhase, WindowOptions};
use gpui_ios::{IosDisplayMetrics, IosEventSink, IosLayer, IosPlatform, IosSurface};

use crate::bridge::{HostCallbacks, UiKitHost};

thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static RUNTIME: RefCell<Option<IosRuntime>> = const { RefCell::new(None) };
}

struct IosRuntime {
    host: Arc<UiKitHost>,
    sink: IosEventSink,
    /// Keeps GPUI alive: `run_embedded` returns once callbacks are installed,
    /// because UIKit — not GPUI — owns the run loop.
    _application: ApplicationHandle,
}

/// Java's phases have integer constants; UIKit's do too, and both are translated
/// at the boundary so an unrecognised value is rejected where it appears rather
/// than becoming some other phase deeper in. A dropped `Ended` would leave a
/// touch stuck down forever.
fn touch_phase(value: i32) -> Result<TouchPhase> {
    match value {
        0 => Ok(TouchPhase::Started),
        1 => Ok(TouchPhase::Moved),
        2 => Ok(TouchPhase::Ended),
        3 => Ok(TouchPhase::Cancelled),
        other => Err(anyhow::anyhow!("unknown touch phase {other} from UIKit")),
    }
}

fn lifecycle_phase(value: i32) -> Result<AppLifecyclePhase> {
    match value {
        0 => Ok(AppLifecyclePhase::Active),
        1 => Ok(AppLifecyclePhase::Inactive),
        2 => Ok(AppLifecyclePhase::Background),
        3 => Ok(AppLifecyclePhase::Foreground),
        other => Err(anyhow::anyhow!(
            "unknown lifecycle phase {other} from UIKit"
        )),
    }
}

/// The app container's Documents directory.
///
/// On iOS the process `HOME` is the app sandbox, so `$HOME/Documents` is the
/// per-app store the OS isolates — where a durable token belongs. Falls back to
/// the temp dir only if `HOME` is somehow unset, which keeps the app running
/// rather than refusing to start over persistence.
fn documents_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Documents")
}

/// Where this client attaches, and what it calls itself.
///
/// The token comes from pairing on the connect screen and is kept in `store`;
/// the code is typed once. `TCODE_SYNC_URL` survives only as a convenience that
/// prefills the host address — never a secret, exactly like the browser's
/// `?url=`.
fn client_connection(store: Arc<dyn ConnectionStore>) -> ClientConnection {
    ClientConnection {
        identity: ClientIdentity {
            client_id: format!("tcode-ios-{}", std::process::id()),
            display_name: "tcode iOS".into(),
            platform: "ios".into(),
            app_version: env!("CARGO_PKG_VERSION").into(),
        },
        url: std::env::var("TCODE_SYNC_URL")
            .ok()
            .filter(|v| !v.is_empty()),
        store,
    }
}

fn with_runtime(name: &'static str, body: impl FnOnce(&mut IosRuntime) -> Result<()>) {
    RUNTIME.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(runtime) = slot.as_mut() else {
            log::warn!("tcode-ios: {name} arrived before the runtime was started");
            return;
        };
        if let Err(error) = body(runtime) {
            log::error!("tcode-ios: {name} failed: {error:#}");
        }
    });
}

fn start(
    view: *mut c_void,
    width: u32,
    height: u32,
    scale: f32,
    callbacks: HostCallbacks,
) -> Result<()> {
    // SAFETY: the shell guarantees `view` is a live UIView whose layer is a
    // CAMetalLayer, and that it outlives every surface built from it.
    let layer = unsafe { IosLayer::from_raw(view) }.context("wrapping the UIKit view")?;
    let metrics =
        IosDisplayMetrics::new(width, height, scale).context("validating drawable metrics")?;
    let surface = IosSurface::new(layer, metrics).context("validating the initial surface")?;

    let host = Arc::new(UiKitHost::new(callbacks));
    // The platform owns its scheduler, exactly as the Android backend does.
    let platform =
        IosPlatform::new(host.clone(), surface).context("constructing the iOS GPUI platform")?;
    let sink = platform.event_sink();
    // One runtime for the process; the client reconnects through this factory.
    let transport: Arc<dyn TransportFactory> =
        Arc::new(NativeTransportFactory::new().context("starting the client's network runtime")?);
    // The paired token lands in the app's Documents dir, which the connect
    // screen fills by pairing — the phone equivalent of the browser's
    // localStorage.
    let store: Arc<dyn ConnectionStore> = Arc::new(FileStore::new(documents_dir()));

    let handle = gpui::Application::with_platform(std::rc::Rc::new(platform)).run_embedded(
        move |cx: &mut App| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let connection = client_connection(store.clone());
                cx.new(|cx| ClientApp::new(connection, transport.clone(), window, cx))
            })
            .expect("the iOS GPUI window must open");
            cx.activate(true);
        },
    );

    RUNTIME.with(|slot| {
        *slot.borrow_mut() = Some(IosRuntime {
            host,
            sink,
            _application: handle,
        });
    });
    Ok(())
}

/// # Safety
/// `view` must be a live `UIView*` with a `CAMetalLayer`, and every callback in
/// `callbacks` must be safe to call from any thread for the process lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcode_ios_start(
    view: *mut c_void,
    width: u32,
    height: u32,
    scale: f32,
    wake_main_thread: extern "C" fn(*mut c_void),
    request_frame: extern "C" fn(*mut c_void),
    show_keyboard: extern "C" fn(*mut c_void),
    hide_keyboard: extern "C" fn(*mut c_void),
    context: *mut c_void,
) {
    let callbacks = HostCallbacks {
        wake_main_thread,
        request_frame,
        show_keyboard,
        hide_keyboard,
        context,
    };
    if let Err(error) = start(view, width, height, scale, callbacks) {
        log::error!("tcode-ios: failed to start: {error:#}");
    }
}

/// # Safety
/// `view` must satisfy the same contract as [`tcode_ios_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcode_ios_surface_created(
    view: *mut c_void,
    width: u32,
    height: u32,
    scale: f32,
) {
    // SAFETY: delegated to the caller by this function's own contract.
    let layer = unsafe { IosLayer::from_raw(view) };
    with_runtime("surfaceCreated", |runtime| {
        let metrics = IosDisplayMetrics::new(width, height, scale)?;
        runtime
            .sink
            .surface_created(IosSurface::new(layer?, metrics)?)
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_surface_destroyed() {
    // Must precede UIKit releasing the drawable: the backend drops its renderer
    // so wgpu cannot present into a layer iOS has reclaimed.
    with_runtime("surfaceDestroyed", |runtime| {
        runtime.sink.surface_destroyed();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_resized(width: u32, height: u32, scale: f32) {
    with_runtime("resized", |runtime| {
        runtime
            .sink
            .resized(IosDisplayMetrics::new(width, height, scale)?)
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_frame() {
    with_runtime("frame", |runtime| {
        // Cleared before the sink runs so a rearm during this frame can schedule
        // the next tick.
        runtime.host.clear_frame_pending();
        runtime.sink.frame();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_drain_main_thread() {
    with_runtime("drainMainThread", |runtime| {
        runtime.sink.drain_main_thread();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_touch(id: u64, phase: i32, x: f32, y: f32, force: f32) {
    with_runtime("touch", |runtime| {
        // A non-positive force is UIKit's "this device reports no force", not a
        // real zero reading.
        let force = (force > 0.0).then_some(force);
        runtime.sink.touch(id, touch_phase(phase)?, x, y, force);
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_lifecycle(phase: i32) {
    with_runtime("lifecycle", |runtime| {
        runtime.sink.lifecycle_changed(lifecycle_phase(phase)?);
        Ok(())
    });
}

/// # Safety
/// `tag` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcode_ios_init_logging(tag: *const c_char) {
    let _ = tag;
    // SAFETY: the caller guarantees a valid C string; the value is only read for
    // diagnostics and never retained.
    let name = if tag.is_null() {
        "tcode".to_owned()
    } else {
        unsafe { CStr::from_ptr(tag) }
            .to_string_lossy()
            .into_owned()
    };
    log::info!("tcode-ios: logging initialised for {name}");
}
