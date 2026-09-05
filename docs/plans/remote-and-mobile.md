# Remote work mode, headless host, mobile and web clients

Status: **in progress** (started 2026-09-05). This file is the resumable plan:
each phase has an executable "done" gate. Update the status lines as phases
land.

## Goal

One tcode *host* (the process that owns sessions, providers, PTYs, the store)
serves N tcode *clients* (desktop, iOS, Android, browser). Clients speak the
existing `tcode-protocol` NDJSON contract; the only new work is transport,
pairing, multi-client fan-out, and client shells. Mobile and browser clients are
UI-only by design: they never run providers locally.

Constraints from the brief:

- LAN or overlay network (tailscale, easytier) only. No relay service.
- Pairing by short code or QR; LAN auto-discovery where the platform allows.
- Host is reliable; the link is not. Clients must survive flapping links.
- Headless host installs on a shell-only server; the desktop app can also act
  as a host.
- The browser build ships only inside the headless host (`--features web`);
  the desktop app never embeds it.

## Decisions (do not re-litigate without new information)

1. **Wire = WebSocket carrying the existing NDJSON lines**, one JSON record
   per text frame. Works from browsers, through overlay VPNs, and reuses WS
   ping/pong for liveness. Native clients use `async-tungstenite` over
   `smol::Async<TcpStream>`; the browser uses `web-sys` WebSocket.
2. **Auth = bearer device token issued at pairing; plaintext transport.**
   `ponytail:` no TLS in v1. The brief scopes this to LAN/WireGuard overlays,
   and browsers cannot pin a self-signed cert anyway. Upgrade path: wrap the
   accepted stream in `futures-rustls` with a cert fingerprint carried in the
   pairing payload; the listener already accepts any `AsyncRead + AsyncWrite`.
   Never an unauthenticated port: every WS connection must present a valid
   token in its first frame or is closed.
3. **Pairing**: host mints a 6-digit code valid 5 minutes, at most 5 wrong
   attempts. Client `POST /pair {code, device_name}` → `{host_id, host_name,
   token}`. QR payload = `tcode://pair?v=1&host=<id>&name=<n>&addrs=<a,b,c>&port=<p>&code=<c>`.
   Host stores `remote.json` (`host_id`, `host_name`, `port`, `devices[]`
   with sha256(token)); client stores `hosts.json` (paired hosts with tokens).
4. **Discovery**: stdlib UDP beacon (broadcast to 255.255.255.255:47421 every
   2 s: `{"tcode":1,"host_id","name","port"}`). Desktop clients listen.
   Mobile v1 pairs by QR or manual entry only (iOS broadcast needs an Apple
   entitlement; overlay networks never carry broadcast). Discovery is a
   convenience layer over pairing, never a trust source.
5. **Multi-client = mux in front of one host loop.** Each connection gets a
   `conn_id`; the mux rewrites request ids to `(conn_id, local_id)` pairs,
   routes `Ack`/`QueryResult` back to the issuing connection, broadcasts
   `Event`s to all. The host loop is unchanged. `ponytail:` "active session"
   stays host-global, so two clients share the selected session; per-client
   selection is the upgrade path (commands would carry session ids).
6. **Reconnect**: the client-side `HostLink` outlives connections. Outgoing
   lines buffer in its channel while offline and flush on reconnect; the link
   re-sends every `Subscribe` it has seen, so snapshots replace the replicas.
   Backoff 1 s → 30 s, immediate retry on app foreground. `ponytail:` session
   snapshot on resubscribe is the full log; incremental tail (`after_seq`) is
   the upgrade if LAN reconnects ever feel slow.
7. **Client crate**: `tcode-client` owns `HostLink` (id correlation, pending
   map, event stream, subscribed-topic memory, connection state). It takes a
   `(Sender<String>, Receiver<String>)` pair, so in-process, native WS, and
   browser WS transports are interchangeable. `tcode-ui` depends on
   `tcode-client` always and on `tcode-runtime` only behind the default
   `local-host` feature (terminal handles, preview broker, import progress).
8. **Desktop remote mode = choose at launch.** The workspace store is built
   once per process around one link. Settings → Remote lists paired and
   discovered hosts; picking one relaunches the app with `--connect <host_id>`
   through the existing relaunch marker. "Back to local" relaunches without it.
9. **Mobile shell** is a new crate `tcode-mobile` (stack navigation: Hosts →
   Sessions → Chat) reusing `tcode-ui`'s markdown, chat timeline, composer,
   theme. The vendored set is our own `gpui-ios` and `gpui-android` backends plus
   `gpui-platform-shim`. The shim exists only because `gpui-base` unconditionally
   depends on `gpui-pre-platform` on non-wasm targets; it adds a fallback arm to
   the published crate. `gpui::Platform` is the intended extension point, so our
   iOS/Android backends stay in this repo and nothing is proposed to Zed. The
   fix is upstream in gpui-kit only: issue longbridge/gpui-kit#2962 and PR
   #2963 move the native examples into their own package so `gpui-base` no
   longer declares `gpui_platform`; verified
   locally that with it tcode builds for macOS, iOS, Android and wasm with no
   `[patch.crates-io]` at all. `tcode-web` constructs its platform directly from
   `gpui-pre-web` (default features off) instead of via `gpui-pre-platform`.
   Published `gpui-pre-wgpu` and `gpui-pre-web` are used directly.
10. **Remote parity gaps deferred to P4**: terminal byte streams
    (`Topic::Terminal` exists, unused), preview reverse RPC, remote directory
    browser for Add Project, attachment upload. Until then the terminal drawer
    and preview panel are hidden when the link is remote.

## Phases

### P0 — Client link seam (foundation)

- New crate `crates/client` (`tcode-client`): `HostLink::from_channels(tx, rx)`,
  `dispatch`, `command`, `query`, `subscribe`, `events()` (Events only; acks
  and query results are correlated inside the link), `subscribed_topics()`,
  `connection_state()`.
- `tcode-runtime`: `HostCx` no longer routes acks by a shared pending map; every
  `HostMessage` goes out on the one ordered output channel. `spawn_host`
  returns `LocalHost { link: HostLink, channels for the mux, terminals,
  preview_requests, import progress }`.
- `tcode-ui`: store uses `HostLink`; `local-host` feature gates
  `LocalTerminalRegistry`/`TerminalWorkspace`, preview broker, and import
  progress. Terminal drawer and preview panel compile out without it.
- Gate: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all --check`, `cargo check -p tcode-ui --no-default-features`.
- Status: **done** (commit follows this plan update).

### P1 — Remote transport, pairing, headless host

- New crate `crates/remote` (`tcode-remote`): listener + HTTP head parse + WS
  handshake + `async-tungstenite` framing, mux, pairing + token store, UDP
  beacon (host side and client browser), native WS client with reconnect,
  optional embedded static bundle (`web` feature, 404 without).
- New bin `crates/headless` (`tcode-headless`): `serve [--listen ADDR:PORT]
  [--name NAME]`, `pair` (asks the running server over loopback for a fresh
  code and prints it plus a Unicode QR).
- Desktop app: `--connect <host_id|ws-url>` builds the store on a remote link;
  Settings → Remote: enable hosting (shows code/QR), paired hosts list,
  discovered hosts, pair-by-code, switch host (relaunch).
- Gate: `tcode-remote` integration test — host + listener on 127.0.0.1:0, pair
  with code, connect two clients, both see an `IndexSnapshot` after one creates
  a project, wrong token is refused, dropping client A's socket and
  reconnecting re-seeds its replicas. Manual e2e on this Mac: `tcode-headless
  serve` + `tcode --connect …` runs a real provider turn.
- Status: **done** (P1a transport + headless; P1b desktop `--connect`, Settings → Remote, `tcode --pair`). Screenshots in `docs/images/remote/`.

### P2 — iOS and Android clients

- Vendor Eauth platform crates into `crates/platform/`; `[patch.crates-io]`.
- `crates/mobile` (`tcode-mobile`): Hosts and pairing screens (phone-only), then
  the desktop `SessionsSidebar`, `ChatView` and `Composer` hosted full-screen in
  a compact mode (docs/mobile-design.md §0.1). P2c **done** on the macOS phone
  preview (screenshots `docs/images/mobile/p2c-*.png`); native runtime
  (asset source, IME insets, camera QR, emulator runs) is P2d.
- `crates/ios` (staticlib + xcodegen host, camera QR via AVFoundation) and
  `crates/android` (cdylib + gradle host, QR via the same pattern as Eauth's
  `QrScannerActivity`).
- Gate: builds for `aarch64-apple-ios-sim` and `arm64-v8a`; app runs on the
  iPhone 17 simulator and the `Lims20Pixel6` AVD against `tcode-headless` on
  this Mac (`127.0.0.1` / `10.0.2.2`); screenshots of hosts, pairing, sessions,
  and a live chat turn checked into `docs/images/mobile/`.
- Status: **done** (P2a–P2d). Native runs verified on the iPhone 17 simulator and a Pixel 6 AVD: pairing, real turns, approvals, keyboard insets, camera permission flow, reconnect. Screenshots `docs/images/mobile/{ios,android}-p2d-*.png`.

#### P2a notes

P2a vendors the mobile GPUI backends and adds native shells with a safe-area-aware
placeholder view. The checked-in smoke-test captures are
`docs/images/mobile/ios-p2a-hello.png` and
`docs/images/mobile/android-p2a-hello.png`.

The only patched upstream crate is `gpui-platform-shim`, a source-identical copy
of published `gpui-pre-platform` plus a fallback `current_platform` arm for
targets without an upstream default backend. It exists for `gpui-base`'s
unconditional non-wasm dependency. Upstream follow-ups are a gpui-pre fallback
arm and making the gpui-kit dependency optional; delete the shim when either
lands. `gpui-ios` and `gpui-android` are our backends, not upstream patches, and
both compile against the published `gpui-pre-wgpu`.

The iOS host was built and run against the installed iOS 26.5 runtime on the
booted iPhone 17 simulator with:

```sh
xcrun simctl list runtimes
crates/ios/host/build.sh --simulator
xcrun simctl install booted crates/ios/host/build/Build/Products/Debug-iphonesimulator/Tcode.app
xcrun simctl launch booted com.tryanks.tcode
xcrun simctl io booted screenshot docs/images/mobile/ios-p2a-hello.png
```

The Android host was built for API 26/arm64-v8a, then run on the
`Lims20Pixel6` AVD in the reference project's non-destructive lavapipe mode:

```sh
crates/android/host/build.sh
/opt/homebrew/share/android-commandlinetools/emulator/emulator \
  -avd Lims20Pixel6 -qt-hide-window -no-audio -no-snapshot -gpu lavapipe
adb install -r crates/android/host/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -W -n com.tryanks.tcode/.GpuiActivity
adb exec-out screencap -p > docs/images/mobile/android-p2a-hello.png
```

The placeholder deliberately uses plain GPUI for P2a. Pulling in `tcode-ui`
currently also pulls its desktop-only runtime, terminal, webview, voice, and
services dependency graph; the shared mobile screens and theme remain P2 work
once that crate's ongoing portability changes land.

#### P2b notes

P2b splits the shared crates at their platform boundaries. `tcode-remote` now
has default-on `server` and `client` features; native phone clients select only
`client`. `tcode-ui` keeps the desktop experience in its default feature set,
while `remote-client`, `terminal`, and `desktop` independently gate the native
WebSocket client, PTY/grid UI, and native integrations. `agent`, `tcode-core`,
and `tcode-protocol` similarly use a default-on `process` feature so their
portable serde model remains available without ACP/process dependencies.

The UI's production timers and channels no longer depend on `smol`. A portable
`WorkspaceStore` subscribes and returns immediately, allowing its GPUI task to
apply the initial snapshots without blocking a single-threaded executor. Builds
with `local-host` retain the bounded synchronous seed needed by desktop startup,
which reads settings immediately after constructing the store. Native dialogs,
webviews, voice, terminal-grid rendering, and filesystem-backed content search
have portable fallbacks or compile out; theme, markdown, timeline/composer, and
the replicated store remain available.

The portability checks used for the native targets were:

```sh
IPHONEOS_DEPLOYMENT_TARGET=26.0 cargo check -p tcode-ui --no-default-features \
  --target aarch64-apple-ios-sim
IPHONEOS_DEPLOYMENT_TARGET=26.0 cargo check -p tcode-ui --no-default-features \
  --features remote-client --target aarch64-apple-ios-sim
IPHONEOS_DEPLOYMENT_TARGET=26.0 cargo check -p tcode-remote --no-default-features \
  --features client --target aarch64-apple-ios-sim

ANDROID_HOME=/opt/homebrew/share/android-commandlinetools \
ANDROID_NDK_HOME=/opt/homebrew/share/android-commandlinetools/ndk/27.1.12297006 \
JAVA_HOME=/opt/homebrew/opt/openjdk@21 CARGO_NDK_PLATFORM=26 \
  cargo ndk -t arm64-v8a check -p tcode-ui --no-default-features
ANDROID_HOME=/opt/homebrew/share/android-commandlinetools \
ANDROID_NDK_HOME=/opt/homebrew/share/android-commandlinetools/ndk/27.1.12297006 \
JAVA_HOME=/opt/homebrew/opt/openjdk@21 CARGO_NDK_PLATFORM=26 \
  cargo ndk -t arm64-v8a check -p tcode-ui --no-default-features \
    --features remote-client
ANDROID_HOME=/opt/homebrew/share/android-commandlinetools \
ANDROID_NDK_HOME=/opt/homebrew/share/android-commandlinetools/ndk/27.1.12297006 \
JAVA_HOME=/opt/homebrew/opt/openjdk@21 CARGO_NDK_PLATFORM=26 \
  cargo ndk -t arm64-v8a check -p tcode-remote --no-default-features \
    --features client
```

The P3 compile boundary is also clear: both
`cargo check -p tcode-ui --no-default-features --target wasm32-unknown-unknown`
and `cargo check -p tcode-client --target wasm32-unknown-unknown` pass. The web
shell, WebSocket implementation, and bundle/serve work remain P3; no deeper
`term` or `syntect` blocker remains in the shared UI graph.

#### P2d notes

P2d completes the native phone runtime. Both entries install
`tcode_ui::assets::Assets`; `tcode-mobile` registers DM Sans on every platform
and Lilex on Android for the configured monospace family. Native debug builds
selectively disable debug assertions for `gpui-kit-assets` and `rust-embed`, so
the existing SVG/font bundle is embedded instead of trying to read dependency
source paths from inside an app sandbox. The Threads header's new-thread and
settings actions now use `IconName::Plus` and the desktop sidebar's
`IconName::Settings`, retaining their 44 pt targets and listeners.

`MobileHost::insets` is default-implemented from the old safe area. The iOS and
Android backends expose their complete `WindowInsets`, and the phone root pads
the page by `max(safe_area.bottom, ime.bottom)`. Android republishes the current
root insets on resume. This keeps the shared Composer above both software
keyboards without making the trait non-portable to wasm.

The iOS host bridges `UIDevice.current.name` and an AVFoundation
`AVCaptureMetadataOutput` QR scanner. The Android host bridges `Build.MODEL`
and a CameraX + ML Kit `QrScannerActivity`; its result returns through the
existing activity, JNI, and `MobileHost::scan_qr` callback chain. Camera usage
descriptions/permissions are present on both platforms. The iOS simulator has
no camera, so its exercised endpoint was the native permission prompt followed
by denial and a clean return. The Android virtual camera reached its native
scanner/permission screen, and denial returned to the pairing sheet without a
crash.

The original `Lims20Pixel6` AVD remained behind an unknown PIN after keyevent 82
and the prescribed upward swipe. It was not wiped. Testing used a fresh
`tcode-p2d` Pixel 6 AVD from the installed
`system-images;android-35;google_apis;arm64-v8a` image; keyevent 82 plus the same
swipe unlocked it. iPhone 17 and that Android AVD paired to the same
`tcode-headless` data directory, displayed the seeded project, streamed real
Claude turns, approved host commands from the phone, showed reconnect state,
and recovered after host restart. Android system Back was exercised from chat
to Threads and from Threads to Hosts. The checked captures are
`docs/images/mobile/{ios,android}-p2d-{hosts,pair,scan,threads,thread,approval,keyboard,reconnecting}.png`.

### P3 — Browser client

- `agent`, `core`, `protocol`, `ui`, `client` compile for
  `wasm32-unknown-unknown` (feature-gate the ACP/process/ureq halves of
  `agent`; replace `smol` timers with gpui timers in `ui`).
- `crates/web` (`tcode-web`): wasm-bindgen entry, `web-sys` WebSocket link,
  token in `localStorage`, same screens as mobile.
- `tcode-headless --features web` embeds the bundle and serves it at `/`.
- Gate: `cargo build -p tcode-web --target wasm32-unknown-unknown` +
  wasm-bindgen; the headless host serves it; the embedded preview browser
  loads it, pairs, lists sessions, sends a turn.
- Status: **done** (P3a transport + serving, P3b real screens). Verified in Chrome against `tcode-headless --features web`; screenshots `docs/images/mobile/web-p3b-*.png`.

### P4 — Remote parity

- Terminal bytes over `Topic::Terminal` (`ServerEvent::TerminalOutput`,
  `Command::TerminalInput/ResizeTerminal`), preview reverse RPC, remote
  directory browser, attachment upload. Desktop-remote reaches feature parity
  with local.
- Status: pending.

## Verification conventions

Every phase: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
--locked -- -D warnings`, `cargo test --workspace --locked`, plus the phase gate.
Commit only after the gate passes on the integrated tree.

#### P3a notes

P3a adds `tcode-web`, an empty native library with all browser dependencies
behind `cfg(target_family = "wasm")`. The wasm shell supplies `WebHost` to
`run_with_host`; it uses the single-threaded GPUI web patch, retains the
`ApplicationHandle`, adopts GPUI's generated canvas, and embeds Noto Sans
(the accompanying OFL is in `crates/web/assets`) plus the existing DM Sans.
Pairing and sockets always use the page origin, irrespective of saved addresses.
Hosts/tokens and the last host use `tcode.hosts` / `tcode.last_host` in localStorage.
The transport authenticates before sending records, replays subscriptions by
topic before queued commands, retries at 1–30 seconds, and retries immediately
on online/visible events. Closing channels releases sockets, timers and listeners.

Build tools must match the pinned Rust bindings: `wasm-bindgen` 0.2.121,
`wasm-bindgen-futures` 0.4.71, `js-sys` and `web-sys` 0.3.98. No timer dependency
or multithreaded web feature is added. `wasm-opt` is optional (a notice is printed
when unavailable).

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.121 --locked
cargo check -p tcode-web --target wasm32-unknown-unknown
crates/web/build.sh
cargo build -p tcode-headless --features web
TCODE_DATA_DIR=/tmp/tcode-host-p3a target/debug/tcode-headless serve \
  --listen 127.0.0.1:47420 --name web-host
curl -sI http://127.0.0.1:47420/ | head -3
curl -sI http://127.0.0.1:47420/tcode_web_bg.wasm
```

The build writes `crates/web/dist/{index.html,tcode_web.js,tcode_web_bg.wasm}`
(and wasm-bindgen declaration files). Only headless's opt-in `web` feature embeds
those three runtime files; building it before the bundle exists reports the
`crates/web/build.sh` hint. Without `web`, static requests remain 404. Static
HEAD now returns the GET headers with no body, including the WASM MIME type.

Open the browser URL in the preview tools, wait for
`document.body.dataset.tcodeReady === 'true'`, inspect the canvas and console,
and save `docs/images/mobile/p3a-browser.png`. The explicit debug entry uses
the same `MobileHost::pair` and `connect` methods as the screens:

```sh
curl -s http://127.0.0.1:47420/admin/pair
```

```js
const web = await import('/tcode_web.js');
await web.debug_pair_and_connect('CODE_FROM_CURL'); // first index_snapshot line
web.debug_connection_state(); // JSON: state, transition history, index_snapshots
```

Stop and restart the same host/data directory, invoking
`debug_connection_state()` while stopped and after restart. Expect
`Reconnecting { attempt: … }` then `Connected`, and another index snapshot
without sending another subscribe. Dispatching an `online` event or making the
page visible interrupts backoff. Debug exports retain only the most recent
probe transport and are intended for this transport smoke test.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p tcode
! strings target/debug/tcode | grep -q tcode_web_bg
```

Validation for P3a uses an isolated checkout of HEAD `d7bfe6663` plus the P3a
changes because phone/UI work is concurrently in progress. This honors the
HEAD MobileHost contract without changing the other engineer's files. Native
workspace checks include `tcode-web`; its native library has no dependencies.

P3a verification on this Mac: native fmt/Clippy and workspace tests passed
(956 passed, 0 failed, 2 ignored), wasm check and optimized bundle build passed,
and the desktop binary scan found no `tcode_web_bg` marker. The requested
47420 port was already held by a pre-existing `mac-host`; the browser smoke
test used 47422 without stopping it. `GET`/`HEAD` returned 200 HTML and
`application/wasm`; a separate headless build without `web` returned 404.
Preview MCP pairing returned an index snapshot; stopping/restarting the same
host produced Reconnecting → Connected and replayed the index subscription.

`preview_screenshot` was attempted but refused with “preview is not visible;
the user is viewing another conversation”. Multiple tcode processes were
running, and the Computer Use app surface did not control the instance owning
that preview. The saved `docs/images/mobile/p3a-browser.png` is therefore a
Chrome DevTools capture of the same served bundle at 390×844 CSS pixels
(780×1688 PNG), showing HEAD's placeholder. Chrome's final console showed only
the two GPUI graphics initialization info messages; the inline favicon avoids
an automatic favicon 404. The literal preview-screenshot gate remains for an
orchestrator with the owning conversation visible. Optional wasm Clippy with
`-D warnings` hits HEAD's unused `Rc` import in `crates/mobile`; normal wasm
check passes with that warning and a platform `instance_flags` dead-code warning.

#### P3b notes

P3b replaces the placeholder browser proof with the real compact mobile
workspace. The web `Application` installs `tcode_ui::assets::Assets`, and the
compact-mode GPUI icons used by hosts, sessions, chat, streaming and approval
are embedded synchronously on wasm. This avoids the component asset loader's
retry/error loop on the browser thread. `tcode-ui` uses `web-time` for the
`Instant` and `SystemTime` call sites reachable from the browser; native builds
continue to use `std::time`. The model-mismatch warning uses an ASCII marker on
wasm because the bundled fonts do not contain U+26A0.

The P3a transport probes are now behind `tcode-web/debug-exports`, which is off
by default. The normal bundle exports only the application entry point.

An isolated host on port 47430 was paired from a 393×852 browser client. The
saved host resolved to the page origin (`127.0.0.1:47430`), its project and
thread appeared, and Claude streamed real turns into the reused `ChatView` and
`Composer`. No approval was requested because the selected provider ran in
Full access mode. Killing and restarting the host produced
Reconnecting → Connected without a reload and preserved the session list.
The same client remained usable at 1280×800. Browser error capture returned
`[]` for load, populated chat, streaming, disconnect and recovery.

The preview canvas was exercised at both requested sizes, but its screenshot
API again refused because the owning conversation was not visible. The P3b
images in `docs/images/mobile/` are therefore captures from a dedicated Chrome
instance connected to the same URL; the streaming frame uses Chrome's page
capture because macOS window capture omits cached GPU layers during animation.

One shared mobile rendering issue remains outside P3b's allowed edit scope:
the Threads header passes the Unicode strings `＋` and `⚙` to the text button
helper, but the browser font set lacks those glyphs. Replace those two text
buttons in `crates/mobile/src/screens.rs` with `tcode_ui::icon::Icon` children
(`IconName::Plus` and an asset-backed settings icon), retaining the existing
44-pixel hit targets and listeners. The sidebar's asset-backed controls render
correctly.
