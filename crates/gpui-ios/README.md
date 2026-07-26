# gpui-ios

The iOS `gpui::Platform` backend. Compiled only for `target_os = "ios"`, so host
workspace builds are unaffected.

Same division of labour as `gpui-android`: this crate owns the platform
implementation and the wgpu/Metal plumbing and links no Objective-C. The UIKit
shell implements `IosHost`, which keeps the backend a plain Rust crate that could
be offered upstream on its own.

## iOS needs no upstream change

An earlier plan recorded that it did, because `gpui_wgpu::WgpuContext::instance`
hardcodes `Backends::VULKAN | GL`. That was wrong, and the correction is the most
useful thing in this file.

That helper is a convenience, not the only entry point:

- `WgpuContext::new(instance, surface, compositor_gpu)` accepts an instance the
  caller builds.
- `WgpuRenderer::new` takes `GpuContext = Rc<RefCell<Option<WgpuContext>>>` — a
  shared slot it populates *only when empty*.

So `metal_context()` builds a `Backends::METAL` instance, selects an adapter, and
hands back a pre-filled slot. The renderer then uses it and never reaches the
VULKAN|GL default. Backend selection happens once, in code we own.

Measured against `aarch64-apple-ios`: `gpui` compiles, `gpui_wgpu` compiles, and
this crate compiles, all with zero errors.

## What is verified

Type-checking and clippy for `aarch64-apple-ios`. Nothing more.

## What is not

No device, no simulator, no linked binary. Unlike the Android side — where
`libtcode_android.so` links and so proves symbol resolution — nothing here has
been through a linker. The first unexercised step is `metal_context` itself:
adapter selection and surface creation have never met a real Metal device.

Touch is also parked. GPUI's core touch dispatch is still marked
"implementation pending" upstream, so `IosEventSink::touch` converts
coordinates but does not deliver; a usable app must synthesize mouse events the
way `gpui_web` does.

## Still needed for a running app

- `tcode-ios`: a staticlib plus an Xcode project, `UIViewController` and
  `CAMetalLayer`, and an `IosHost` implementation.
- A `CADisplayLink` driving `frame`, coalesced per the trait's contract.
- Mouse-event synthesis from touches.
- Keychain-backed credentials and a `UITextInput` conformance for IME.
