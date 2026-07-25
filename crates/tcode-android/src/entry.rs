//! The native entry points `TcodeActivity` calls.
//!
//! Every function here runs on the Activity's Looper thread. That is not a
//! convenience — `AndroidEventSink` is `!Send` precisely so this invariant is
//! enforced by the type system rather than by hope, and the surface-lifecycle
//! contract in `crates/gpui-android/README.md` depends on these calls being
//! serialized against each other.

use std::cell::RefCell;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use std::rc::Rc;

use gpui::{
    App, AppContext as _, AppLifecyclePhase, ApplicationHandle, Context, IntoElement,
    ParentElement as _, Render, Styled as _, TouchPhase, Window, WindowOptions, div, rgb,
};
use gpui_android::{
    AndroidDisplayMetrics, AndroidEventSink, AndroidNativeWindow, AndroidPlatform, AndroidSurface,
};
use jni::JNIEnv;
use jni::objects::JObject;
use jni::sys::{jfloat, jint, jlong};
use ndk::native_window::NativeWindow;

use crate::bridge::JniHost;

// GPUI and its event sink, owned for the Activity's lifetime.
//
// Thread-local rather than a global: the sink is `!Send`, and putting it here
// means a call arriving on the wrong thread finds nothing instead of corrupting
// state that only the Looper thread may touch.
//
// The `allow` is for a confirmed clippy bug, not a real finding: this lint
// demands a `const` initializer and then reports the identical error when one is
// present. Verified by testing both forms against clippy 1.95 for
// aarch64-linux-android — `= const { RefCell::new(None) }` and
// `= RefCell::new(None)` produce the same message, so there is nothing to fix.
thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static RUNTIME: RefCell<Option<AndroidRuntime>> = const { RefCell::new(None) };
}

struct AndroidRuntime {
    host: Arc<JniHost>,
    sink: AndroidEventSink,
    /// Keeps GPUI alive. `run_embedded` returns once callbacks are installed,
    /// because Android — not GPUI — owns the main loop.
    _application: ApplicationHandle,
}

/// The smallest thing that proves the whole stack drew: a solid quad.
///
/// Deliberately not tcode's UI. This milestone is "the Android linker resolved
/// every symbol and wgpu produced a frame"; rendering the real app would mean a
/// failure could come from anywhere in 37k lines of UI instead.
struct FirstFrame;

impl Render for FirstFrame {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x1d4ed8))
            .flex()
            .items_center()
            .justify_center()
            .child(div().text_color(rgb(0xffffff)).child("tcode on Android"))
    }
}

/// Convert a Java `Surface` into the retained native window the backend needs.
///
/// # Safety
/// `surface` must be a live `android.view.Surface`. `NativeWindow::from_surface`
/// takes ownership of a new ANativeWindow reference, which `AndroidSurface` then
/// holds for the generation's lifetime — the backend releases it only after
/// `replace_surface` succeeds, so wgpu can never outlive the window it renders
/// into.
unsafe fn native_window(
    env: &mut JNIEnv<'_>,
    surface: &JObject<'_>,
) -> Result<AndroidNativeWindow> {
    let window = unsafe { NativeWindow::from_surface(env.get_raw(), surface.as_raw()) }
        .ok_or_else(|| anyhow!("android.view.Surface did not yield an ANativeWindow"))?;
    Ok(AndroidNativeWindow::new(window))
}

/// Java's touch-phase constants, mirrored from `TcodeActivity`.
///
/// Translated at the boundary on purpose: an unrecognised value from Java must
/// be rejected here rather than silently becoming some other phase deeper in,
/// where a dropped `Ended` would leave a pointer stuck down forever.
fn touch_phase(value: jint) -> Result<TouchPhase> {
    match value {
        0 => Ok(TouchPhase::Started),
        1 => Ok(TouchPhase::Moved),
        2 => Ok(TouchPhase::Ended),
        3 => Ok(TouchPhase::Cancelled),
        other => Err(anyhow!("unknown touch phase {other} from the Java side")),
    }
}

/// Java's lifecycle constants, mirrored from `TcodeActivity`.
fn lifecycle_phase(value: jint) -> Result<AppLifecyclePhase> {
    match value {
        0 => Ok(AppLifecyclePhase::Active),
        1 => Ok(AppLifecyclePhase::Inactive),
        2 => Ok(AppLifecyclePhase::Background),
        3 => Ok(AppLifecyclePhase::Foreground),
        other => Err(anyhow!(
            "unknown lifecycle phase {other} from the Java side"
        )),
    }
}

fn start(
    env: &mut JNIEnv<'_>,
    activity: &JObject<'_>,
    surface: &JObject<'_>,
    width: jint,
    height: jint,
    density: jfloat,
) -> Result<()> {
    let host = Arc::new(JniHost::new(env, activity).context("building the JNI host")?);
    let metrics = AndroidDisplayMetrics::new(width.max(0) as u32, height.max(0) as u32, density)
        .context("validating display metrics")?;
    let window = unsafe { native_window(env, surface) }?;
    let surface = AndroidSurface::new(window, metrics).context("building the initial surface")?;

    let platform = AndroidPlatform::new(host.clone(), surface)
        .context("constructing the Android GPUI platform")?;
    let sink = platform.event_sink();

    // The initial surface must stay valid across launch, per the contract.
    let application =
        gpui::Application::with_platform(Rc::new(platform)).run_embedded(|cx: &mut App| {
            cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| FirstFrame))
                .expect("the Android GPUI window must open");
            cx.activate(true);
        });

    RUNTIME.with(|slot| {
        *slot.borrow_mut() = Some(AndroidRuntime {
            host,
            sink,
            _application: application,
        });
    });
    Ok(())
}

/// Run `body` against the live runtime, logging rather than unwinding.
///
/// A panic crossing the JNI boundary aborts the process with no useful Java
/// stack, so every entry point funnels through here.
fn with_runtime(name: &'static str, body: impl FnOnce(&mut AndroidRuntime) -> Result<()>) {
    RUNTIME.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(runtime) = slot.as_mut() else {
            log::warn!("tcode-android: {name} arrived before the runtime was started");
            return;
        };
        if let Err(error) = body(runtime) {
            log::error!("tcode-android: {name} failed: {error:#}");
        }
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeStart(
    mut env: JNIEnv<'_>,
    activity: JObject<'_>,
    surface: JObject<'_>,
    width: jint,
    height: jint,
    density: jfloat,
) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("tcode"),
    );
    if let Err(error) = start(&mut env, &activity, &surface, width, height, density) {
        log::error!("tcode-android: failed to start: {error:#}");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeSurfaceCreated(
    mut env: JNIEnv<'_>,
    _activity: JObject<'_>,
    surface: JObject<'_>,
    width: jint,
    height: jint,
    density: jfloat,
) {
    let acquired = unsafe { native_window(&mut env, &surface) };
    with_runtime("surfaceCreated", |runtime| {
        let metrics =
            AndroidDisplayMetrics::new(width.max(0) as u32, height.max(0) as u32, density)
                .context("validating display metrics")?;
        let surface = AndroidSurface::new(acquired?, metrics)?;
        runtime.sink.surface_created(surface)?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeSurfaceDestroyed(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
) {
    // Must run before Android releases the surface: the backend unconfigures
    // rendering and keeps the retired lease until a replacement succeeds.
    with_runtime("surfaceDestroyed", |runtime| {
        runtime.sink.surface_destroyed();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeResized(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
    width: jint,
    height: jint,
    density: jfloat,
) {
    with_runtime("resized", |runtime| {
        let metrics =
            AndroidDisplayMetrics::new(width.max(0) as u32, height.max(0) as u32, density)
                .context("validating display metrics")?;
        runtime.sink.resized(metrics)?;
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeFrame(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
) {
    with_runtime("frame", |runtime| {
        // Clear before invoking: the sink rearms while the window is active, and
        // that rearm must be able to post a new callback.
        runtime.host.clear_frame_pending();
        runtime.sink.frame();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeDrainMainThread(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
) {
    with_runtime("drainMainThread", |runtime| {
        runtime.sink.drain_main_thread();
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeTouch(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
    pointer_id: jlong,
    phase: jint,
    x: jfloat,
    y: jfloat,
    pressure: jfloat,
) {
    with_runtime("touch", |runtime| {
        // A zero pressure is Android's "no force information", not a real zero
        // force reading, so it is reported as absent rather than as 0.0.
        let force = (pressure > 0.0).then_some(pressure);
        runtime
            .sink
            .touch(pointer_id as u64, touch_phase(phase)?, x, y, force);
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeLifecycle(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
    phase: jint,
) {
    with_runtime("lifecycle", |runtime| {
        runtime.sink.lifecycle_changed(lifecycle_phase(phase)?);
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeBackPressed(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
) {
    with_runtime("backPressed", |runtime| {
        // The bool says whether GPUI consumed it. Android needs that answer to
        // decide whether to pop its own back stack, which is a later wiring
        // step; logging keeps the information visible until then.
        let handled = runtime.sink.back_pressed();
        log::debug!("tcode-android: back press handled by gpui: {handled}");
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_tryanks_tcode_TcodeActivity_nativeMemoryWarning(
    _env: JNIEnv<'_>,
    _activity: JObject<'_>,
) {
    with_runtime("memoryWarning", |runtime| {
        runtime.sink.memory_warning();
        Ok(())
    });
}
