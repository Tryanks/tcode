# gpui-android

Android platform backend for the `gpui-pre` 0.3.3 snapshot used by EAuth. The
crate is an ordinary Rust dependency on every target, but its implementation is
compiled only for Android. Calling `platform()` elsewhere fails with a clear
panic instead of pulling Android libraries into host builds.

## Architecture

`eauth-android` enters through `android_main`, initializes this crate with the
`android_activity::AndroidApp`, and constructs `gpui::Application` with the
process-local platform. Android's native activity loop remains the GPUI
foreground executor. Work submitted to the background executor is distributed
over a small Rust worker pool; delayed work uses timer threads and foreground
continuations wake the Android looper.

The platform exposes one full-screen `PlatformWindow`. It owns the current
`ANativeWindow`, a `gpui-pre-wgpu::WgpuRenderer`, and the shared `WgpuContext`.
`InitWindow` creates or replaces the Vulkan surface. `TerminateWindow`
unconfigures it before Android invalidates the native window, while preserving
the device, pipelines, and sprite atlas for resume. Density converts Android
device pixels into GPUI logical pixels. `uiMode` supplies light/dark appearance.

`CosmicTextSystem` is populated from `/system/fonts` because fontdb does not
load Android system fonts automatically. This includes Android's Noto CJK fonts
and allows mixed Latin/Chinese strings to shape and render without bundling a
font in the APK.

## Java/JNI surface

The Gradle host supplies `com.eauth.gpui.GpuiActivity`, a `NativeActivity`
subclass with a one-pixel focusable editor view. Its `BaseInputConnection`
provides the IME protocol Android requires without covering or intercepting the
native rendering surface.

Rust calls these activity methods on Android's Java UI thread:

- `gpuiShowKeyboard()` and `gpuiHideKeyboard()`
- `gpuiConfigureInput(boolean, int, boolean, int)`
- `gpuiFinish()`

Java calls these exported JNI functions; each is queued and handled on the
native activity/GPUI thread:

- `nativeCommitText(String)`
- `nativeSetComposingText(String)`
- `nativeFinishComposingText()`
- `nativeDeleteBackward()`
- `nativeKeyEvent(int, boolean, int, int)`
- `nativeOnInsets(int, int, int, int, int)`
- `nativeOnBack(boolean)`

Committed and composing text is applied through `PlatformInputHandler` using
UTF-16 ranges. Hardware/IME key events become GPUI `KeyDown`/`KeyUp` events.
System-bar, display-cutout, and IME geometry becomes `WindowInsets`. A GPUI
window back handler takes precedence; `set_back_callback` exposes otherwise
unhandled system back actions to the host application.

## Pointer mapping

Android `MotionEvent`s are forwarded as GPUI `TouchEvent`s, using every pointer,
Android's per-gesture pointer ids, logical coordinates, pressure, and the
corresponding started/moved/ended/cancelled phase. Pointer ids are paired with
the motion stream's monotonic down time so a reused Android id cannot collide
with an earlier GPUI touch.

Gesture interpretation lives in gpui-pre's portable gesture arena, the same
path used by `gpui-ios`. Android supplies only platform tuning: a 450 ms long
press and `ScrollPhysics::android()`. Tap synthesis, touch slop, scroll capture,
drag-cancels-click, velocity sampling, and momentum are therefore shared with
iOS rather than reimplemented in this backend.

## Current limitations

- Android supports a single GPUI window; desktop window management operations
  are intentionally no-ops.
- Generic GPUI file dialogs, system credential storage, notifications,
  accessibility bridging, and URL intents are not implemented in this backend;
  applications can provide those services in their Java/Kotlin host (as EAuth
  does).
- The text clipboard is bridged to Android `ClipboardManager`; GPUI clipboard
  representations other than text are not exposed yet. EAuth's application
  Host additionally marks copied codes sensitive and clears them on a timer.
- Raw multi-touch reaches the portable gesture arena, but this backend does not
  yet translate stylus buttons, hover, or hardware mouse-wheel axes.

## Attribution

The architecture and Android integration patterns were studied from
`gpui-toolkit/crates/gpui-android` and its showcase host, copyright 2025 Pierre
F. Aubert, licensed under the ISC license. This backend was written for the
different `gpui-pre` 0.3.3 interfaces rather than vendoring that source. The
reference's ISC permission and warranty notice remain applicable to ideas and
adapted integration patterns: use, copying, modification, and distribution are
permitted with the copyright and permission notice retained; the software is
provided “AS IS” without warranty.
