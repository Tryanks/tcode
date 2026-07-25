# tcode-ios

The iOS app shell: a C-ABI bridge that drives the `gpui-ios` platform backend.

Mirrors `tcode-android`. That crate owns Java and knows nothing about GPUI's
internals; this one owns Objective-C and likewise. The backends stay plain Rust
crates that could be offered upstream on their own.

## Building

```sh
cargo build -p tcode-ios --target aarch64-apple-ios --release
```

Produces a `staticlib` for Xcode to link into the app binary. No NDK-style
toolchain variables are needed — the Apple toolchain ships with Xcode.

## The seam

The shell registers four function pointers at launch and calls the
`tcode_ios_*` entry points. A function-pointer table rather than an
Objective-C protocol object, because these are invoked from arbitrary Rust
threads and a plain `extern "C"` pointer has no retain/release or
thread-affinity semantics to get wrong.

The shell must:

1. Own a `UIView` whose `layer` is a `CAMetalLayer`, and keep it alive for as
   long as any surface built from it.
2. Call `tcode_ios_start` once with that view, its pixel size, and
   `contentScaleFactor`.
3. Hop to the main queue inside `wake_main_thread` and call
   `tcode_ios_drain_main_thread`.
4. Schedule **one** `CADisplayLink` tick per `request_frame` and call
   `tcode_ios_frame`; the Rust side coalesces, so a tick per request is wrong.
5. Call `tcode_ios_surface_destroyed` **before** UIKit releases the drawable,
   and `tcode_ios_surface_created` when a new one arrives.

Touch and lifecycle constants are translated at the boundary
(`entry.rs`), so an unrecognised value is rejected where it appears rather than
becoming some other phase deeper in — a dropped `Ended` would leave a touch
stuck down forever. The Swift/Objective-C constants must match that list.

## What is verified

Type-check and clippy for `aarch64-apple-ios`, for both this crate and
`gpui-ios`.

## What is not

No device, no simulator, no Xcode project, and — unlike Android, where
`libtcode_android.so` links — **nothing here has been through a linker**. The
first unexercised step is `metal_context`: Metal adapter selection and surface
creation have never met real hardware.

## Deliberately unimplemented

- **Keychain credentials.** A store that silently discards secrets is worse than
  one reporting itself absent, because the caller would believe the secret was
  saved. Needs a Security.framework wrapper.
- **IME positioning and text-input state.** Need a `UITextInput` conformance on
  the hosting view reporting caret rects.
- **Thermal state** reports nominal. `ProcessInfo.thermalState` arrives as a
  notification rather than a poll; guessing would make GPUI throttle for nothing.
- **`finish_activity` does nothing.** iOS apps do not exit on demand — `exit(0)`
  is grounds for App Store rejection and reads as a crash.
