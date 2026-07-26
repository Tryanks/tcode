# tcode-android

The Android app shell: a JNI bridge that drives the `gpui-android` platform
backend, plus the minimal Activity needed to package it.

The split from `gpui-android` is deliberate. That crate owns the GPUI `Platform`
implementation and knows nothing about Java; this one owns Java and knows
nothing about GPUI's internals. Keeping them apart is what lets the backend be
offered upstream on its own.

## Building

An Android C toolchain is required even for `cargo check`, because
`gpui → stacksafe → psm` assembles AArch64 while dependencies are checked.
Verified with Homebrew's NDK 29.0.14206865 and API 26:

```sh
export ANDROID_NDK_HOME=/opt/homebrew/share/android-ndk
cargo ndk -t arm64-v8a --platform 26 build -p tcode-android
```

Note `cargo ndk`'s `-p` is `--package`, not the platform level; the API level is
`--platform`.

The produced `libtcode_android.so` is what Gradle packages and what
`System.loadLibrary("tcode_android")` opens.

## What is verified

The shared library **links** for `aarch64-linux-android`, which a `cargo check`
never establishes — it means the Android linker resolved every symbol, including
those `gpui_wgpu`, `wgpu` and `psm` require. All ten JNI entry points are
exported and confirmed present in the ELF dynamic symbol table.

## What is not verified

Nothing has run on a device. There is no emulator or phone in the environment
this was built in, so the first execution is unexercised. The most likely first
failure is `WgpuRenderer::new`: raw-handle validity, Vulkan adapter and device
selection, and surface capabilities have never met real hardware.

## Constants shared with Kotlin

`touch_phase` and `lifecycle_phase` in `src/entry.rs` mirror the integer
constants in `TcodeActivity`. They are translated at the boundary on purpose —
an unrecognised value is rejected there rather than becoming some other phase
deeper in, where a dropped `Ended` would leave a pointer stuck down forever.
The two lists must be edited together.

## Deliberately unimplemented

These return honest errors rather than pretending to succeed:

- **Credential storage** needs an Android Keystore-backed host service. A store
  that silently discards secrets is worse than one that reports itself absent,
  because the caller would believe the secret was saved.
- **`open_path`** needs a FileProvider `content://` URI; Android cannot hand a
  raw filesystem path to another app.
- **IME positioning and text-input state** need an InputConnection reporting
  caret bounds through `CursorAnchorInfo`. Until then the platform IME places
  itself.
- **Thermal state** reports nominal. Android exposes this as a PowerManager
  subscription rather than a query, and guessing would make GPUI throttle for
  no reason.
