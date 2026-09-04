# gpui-ios

`gpui-ios` is the UIKit platform backend for `gpui-pre` 0.3.3 used by the
EAuth iOS host. It is deliberately an embedded backend: `UIApplication` owns
the process and run loop, Swift supplies a `UIView`, and
`Application::run_embedded` keeps GPUI alive while UIKit drives frames and
input.

The public Rust entry point is:

```rust
pub fn platform() -> Rc<dyn gpui::Platform>
```

It is process-wide, lazily initialized, and must be called on the UIKit main
thread. On non-iOS targets the crate still compiles; calling `platform()`
panics with a platform-specific diagnostic.

## Architecture

The Swift host creates a `GPUIHostView` whose backing layer is
`CAMetalLayer`, then passes the unretained `UIView` pointer and its logical
geometry through `gpui_ios_attach_view`. `IosWindow` wraps that pointer in
`raw-window-handle`'s UIKit handle, creates a Metal-only `wgpu::Instance`, and
hands its context to the patched in-tree `gpui-pre-wgpu::WgpuRenderer`. Logical
resizes are converted to device pixels before `update_drawable_size`. The
UIKit content scale is also applied to the renderer at creation, on every
scale change, and immediately after a detached surface is replaced so glass
blur, thickness, refraction, and edge widths remain point-correct.

UIKit and GPUI both enter window/application state only on the main thread.
The process-wide platform, attached view, and active window are therefore
thread-local rather than marked `Send`. `IosDispatcher` sends foreground work
to the GCD main queue, background work to GCD global queues selected by GPUI
priority, delayed work through `dispatch_after_f`, and realtime work to a
named Rust thread. `CADisplayLink` calls `gpui_ios_request_frame` so animations,
touch gesture deadlines, and fling momentum keep advancing.

`IosTextSystem` uses CoreText for shaping and `zed-font-kit`'s CoreText loader
for metrics and rasterization. The default family is
`.AppleSystemUIFont`; CoreText's cascade supplies PingFang for Chinese and
Apple Color Emoji where needed. CoreText UTF-16 run indices are converted back
to the UTF-8 byte indices expected by GPUI.

Unsafe code and all C/Objective-C-facing state are confined to `src/ios/`.
Swift retains every UIKit object; Rust keeps only a non-owning pointer for the
lifetime of the attached platform window.

## C ABI exported to Swift

| Symbol | Purpose |
| --- | --- |
| `gpui_ios_init` | Lazily initialize the main-thread platform. |
| `gpui_ios_attach_view`, `gpui_ios_detach_view` | Attach/detach the host `CAMetalLayer` view and initial metrics. |
| `gpui_ios_request_frame` | Let the active GPUI window render one display-link frame. |
| `gpui_ios_touches_began/moved/ended/cancelled` | Forward a batch of UIKit touches with stable IDs, coordinates, force, and optional prediction. |
| `gpui_ios_resize`, `gpui_ios_scale_factor_changed` | Update logical bounds, drawable size, and display scale. |
| `gpui_ios_safe_area_changed` | Forward UIKit safe-area edges. |
| `gpui_ios_keyboard_frame_changed` | Forward the current bottom IME obstruction. |
| `gpui_ios_appearance_changed` | Map the current trait collection to light/dark `WindowAppearance`. |
| `gpui_ios_lifecycle_active/inactive/background/foreground` | Forward `UIScene` lifecycle phases. |
| `gpui_ios_memory_warning` | Notify GPUI of UIKit memory pressure. |
| `gpui_ios_insert_text` | Commit UTF-8 text through the focused `PlatformInputHandler`. |
| `gpui_ios_set_marked_text`, `gpui_ios_unmark_text` | Update or end an IME composition, including its UTF-16 selection. |
| `gpui_ios_delete_backward` | Deliver a Backspace key pair to the focused input. |
| `gpui_ios_key_event` | Forward hardware key up/down, modifiers, character, and repeat state. |
| `gpui_ios_open_url_received` | Deliver an incoming URL to the registered platform callback. |

The linked Swift executable provides the reverse callbacks
`gpui_ios_host_log`, `gpui_ios_host_schedule_frame`, `gpui_ios_host_show_keyboard`,
`gpui_ios_host_hide_keyboard`, `gpui_ios_host_configure_text_input`,
`gpui_ios_host_open_url`, and the three `gpui_ios_host_*clipboard*` functions.
The forward declarations live in `../eauth-ios/host/Sources/BridgingHeader.h`;
the reverse implementations live in `../eauth-ios/host/Sources/HostCallbacks.swift`.

## Touch and scrolling behavior

The host assigns a monotonically increasing `TouchId` when each `UITouch`
begins and sends all changed contacts in a single C array. Rust emits one raw
`PlatformInput::Touch` per contact. For moves, UIKit's last predicted touch is
included for latency compensation, while actual coordinates remain the source
of hit testing and velocity.

`gpui-pre` 0.3.3 contains the gesture arena, so this backend intentionally does
not also synthesize mouse events. GPUI selects the primary touch, defers the
mouse-down/up click pair until the tap wins, emits drag scrolling as
`ScrollWheelEvent { delta: ScrollDelta::Pixels(..) }`, advances iOS-style fling
momentum on later frames, and preserves its own long-press deadline. A second
touch can therefore participate in GPUI's multi-touch recognition without
creating a second mouse pointer, and small movements do not break click or
long-press timing. Application components consume the arena's phased
`LongPressEvent` through the shared `eauth-app` `LongPress` helper. There is no
UIKit-only recognizer or synthetic right-click path, so iOS and Android use the
same long-press threshold, cancellation, capture, and post-press click
suppression semantics.

## Keyboard bridge

The Swift host keeps a nearly transparent `UITextView` in the view hierarchy.
When GPUI reports text focus, it becomes first responder. Marked text is sent
through `replace_and_mark_text_in_range`; a confirmed Chinese candidate is
sent through `replace_text_in_range`, which replaces the marked range and
commits the composition. Text assistance and return-key presentation are
updated from `TextInputConfiguration`. Keyboard frame notifications update
`WindowInsets::ime`, while `safeAreaInsets` update `WindowInsets::safe_area`.

## Build and run

From the host directory:

```bash
./build.sh                 # arm64 iPhone 17 simulator
./build.sh --device        # arm64 physical-device build, unsigned
```

The script builds `eauth-ios` in debug mode by default, copies
`libeauth_ios.a` into the ignored `host/lib/` directory, regenerates the Xcode
project with XcodeGen, and builds the app. Set
`EAUTH_IOS_RUST_PROFILE=release` for an optimized Rust static library. The
default destination is an iPhone 17 on iOS 26.5; override the runtime explicitly
with `EAUTH_IOS_SIMULATOR_OS` when testing a different installed SDK.

The generated `EAuth` scheme includes the coordinate-based `EAuthUITests`
acceptance suite. It covers PIN setup/relaunch/wrong-and-right unlock,
background relock, debug seeding, list/grid and theme states, shared long press,
detail edit/save, scanner fallbacks and image QR import, settings depth,
security audit, logs, sync validation, export/share dismissal, rotation, and
liquid glass over moving content. Named screenshot attachments are checked in
under `eauth-ios/host/screenshots/`; `screenshots/PARITY.md` places the primary
states beside the SwiftUI iOS 26 references and records visible differences.

## Current limitations

- One attached UIKit view and one GPUI window are supported. Native secondary
  windows, dialogs, menus, drag-and-drop, screen capture, and cursor APIs are
  not implemented.
- Credentials return an explicit unsupported error; production authentication
  secrets must use the application-owned Keychain layer.
- IME candidate-window caret positioning and continuous interpolation of the
  keyboard animation are not yet implemented; endpoint insets are exact.
- Custom CoreText feature dictionaries and explicit fallback lists are retained
  in font cache keys but are not yet applied to attributed runs.
- Accessibility is limited to GPUI semantics that UIKit can observe indirectly;
  there is no native UIKit accessibility-tree adapter yet.
- The display abstraction currently describes the attached main screen only.

## Attribution

The module boundaries and host/backend handshake were informed by the
ISC-licensed `gpui-toolkit/crates/gpui-ios` reference supplied with this
worktree. This implementation was written for the different `gpui-pre` 0.3.3
traits and renderer APIs; no reference source was vendored or copied into this
crate.
