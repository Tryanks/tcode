# gpui-android

This crate is the Android `gpui::Platform` backend. Its Rust implementation is
compiled only for `target_os = "android"` so ordinary workspace builds remain
host-only.

## Android check prerequisites

An Android C toolchain is required even for `cargo check`. GPUI depends on
`stacksafe`, which depends on `psm`; `psm`'s build script assembles an AArch64
source file while dependencies are being checked. Installing only the Rust
`aarch64-linux-android` standard library is therefore insufficient.

The supported floor is Android API 26. This was verified with Homebrew's Android
NDK revision 29.0.14206865. A verification shell can be configured without
committing toolchain paths:

```sh
export CC_aarch64_linux_android=/opt/homebrew/share/android-ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android26-clang
export AR_aarch64_linux_android=/opt/homebrew/share/android-ndk/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-ar
cargo check -p gpui-android --target aarch64-linux-android
```

The prebuilt directory may be named `darwin-arm64` in other NDK distributions.
Do not point these variables at host Clang for a real Android build: host Clang
is useful only as an explicitly reported type-check fallback and is not an
Android linker/toolchain.

## JNI/NDK boundary

`AndroidHost` is the outbound, thread-safe half of the boundary. A bridge should
hold a `JavaVM` and global references rather than a thread-local `JNIEnv`, because
`wake_main_thread` can be called by Rust worker threads. It must post that wake
to the designated Android Looper, coalesce `request_frame` calls into one pending
Choreographer callback, and implement the Activity, intent, clipboard, credential,
back, and IME operations declared by the trait.

`AndroidEventSink` is the inbound, UI-thread-only half. Start in this order:

1. Convert the Java `Surface` to an owned `ndk::NativeWindow`, wrap it in
   `AndroidNativeWindow`, and pair it with validated physical dimensions and
   density in `AndroidSurface`.
2. Construct `AndroidPlatform` on the same Looper thread and retain its
   `event_sink`; launch GPUI while that initial surface remains valid.
3. After `wake_main_thread`, call `drain_main_thread`. After Choreographer fires,
   call `frame`; the sink invokes GPUI's frame callback and rearms while active.
4. Translate lifecycle, input, insets, back, memory, appearance, thermal, and
   InputConnection callbacks into the matching sink methods. Coordinates passed
   to `touch` are physical; insets and IME ranges must already use GPUI units.

Before Android releases a surface generation, call `surface_destroyed`. This
unconfigures rendering and retains the old `ANativeWindow` lease so wgpu cannot
outlive it. On recreation, acquire a new owned `NativeWindow` and call
`surface_created`; the renderer replaces its surface using the existing wgpu
instance, then releases the retired lease. Serialize these calls on the original
Looper thread and do not deliver a frame between destroy and recreate.
