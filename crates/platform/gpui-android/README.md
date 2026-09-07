# gpui-android

Android platform backend for the `gpui-pre` 0.3.3 snapshot used by tcode. The
crate is an ordinary Rust dependency on every target, but its implementation is
compiled only for Android. Calling `platform()` elsewhere fails with a clear
panic instead of pulling Android libraries into host builds.

## Architecture

`tcode-android` enters through `android_main`, initializes this crate with the
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
for mixed Latin/Chinese text. The loader excludes the system Noto Color Emoji
face so the application's bundled CBDT/CBLC font can supply supported color
bitmap glyphs. tcode also registers its shared UI and monospace fonts.

## Java/JNI surface

The Gradle host supplies `com.tryanks.tcode.GpuiActivity`, a `NativeActivity`
subclass with a one-pixel focusable editor view. Its `BaseInputConnection`
provides the IME protocol Android requires without covering or intercepting the
native rendering surface.

The Java declarations live in
[GpuiActivity.java](../../android/host/app/src/main/java/com/tryanks/tcode/GpuiActivity.java).
The matching JNI exports in [tcode-android](../../android/src/lib.rs) forward
callbacks to this backend. Rust calls activity methods on Android's Java UI
thread; incoming callbacks are queued for the native activity/GPUI thread.
Keep the method signatures at these two ends synchronized.

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

## Build and run

From the repository root, with the Android SDK/NDK, JDK and `cargo-ndk`
installed:

```sh
crates/android/host/build.sh
adb install -r crates/android/host/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -W -n com.tryanks.tcode/.GpuiActivity
```

The script builds the arm64 Rust library and Gradle Debug APK. Set
`ANDROID_HOME`, `ANDROID_NDK_HOME` and `JAVA_HOME` to your local installations;
the script's defaults are Homebrew paths. `CARGO_NDK_PLATFORM` defaults to 26.
The debug build embeds the shared font/SVG assets so it does not depend on
source paths from the development machine.

For testing performance on a phone, run `crates/android/host/build.sh --release`.
This uses the `android-release` Cargo profile: release optimization (including
fat LTO), unwind panics, and unstripped symbols with line information for
`ndk-stack`. The matching unstripped library is in
`crates/android/host/app/src/main/jniLibs/arm64-v8a`; preserve it alongside crash
logs before another build overwrites it. Gradle still runs `assembleDebug`, so
the APK is debug-signed and installs with the same `adb install -r` command above
without signing setup. Both modes print the APK path and size and overwrite the
same APK. Omitting the flag keeps the native dev build.

See [the design spec](../../../docs/DESIGN.md#compact-layout) for application behavior and
platform verification, and [remote work mode](../../../docs/remote.md) for
pairing with a host. Android emulator loopback is the emulator itself; use a
host address reachable from the device.

## Current limitations

- Android supports a single GPUI window; desktop window management operations
  are intentionally no-ops.
- Generic GPUI file dialogs, system credential storage, notifications,
  accessibility bridging, and URL intents are not implemented in this backend;
  applications must provide any required services in their host.
- The text clipboard is bridged to Android `ClipboardManager`. Non-text GPUI
  clipboard data is retained only in-process.
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
