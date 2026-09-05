# Remote work mode, headless host, mobile and web clients

Status: **feature-complete on branch `remote-and-mobile`** (started 2026-09-05). `gpui-base` comes from a git rev until a release includes the dependency fix (Decision 9). This file is the resumable plan:
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
2. **Auth = bearer device token issued at pairing; TLS on every connection.**
   P4c wraps the smol listener in futures-rustls/rustls with the ring provider.
   First start creates a persistent rcgen self-signed certificate for the host
   id; `remote-cert.der` and PKCS#8 `remote-key.der` live beside `remote.json`
   with mode 0600. Partial/corrupt identities fail startup instead of rotating.
   Native clients pin SHA-256 of the entire DER certificate before sending a
   code or token. QR links carry the pin; manual pairing records the certificate
   through TOFU. Legacy empty pins migrate once on first connect, persist, and
   warn without logging tokens or fingerprints. Changed pins fail hard with
   Offline / “执行端证书已变化，请重新配对”. Browsers use HTTPS and browser trust:
   the self-signed interstitial must be accepted on first visit; browser `fp`
   is a display value, not JavaScript certificate verification.
3. **Pairing**: host mints a 6-digit code valid 5 minutes, at most 5 wrong
   attempts. Client `POST /pair {code, device_name}` → `{host_id, host_name,
   token}`. QR payload = `tcode://pair?v=1&host=<id>&name=<n>&addrs=<a,b,c>&port=<p>&code=<c>`.
   Host stores `remote.json` (`host_id`, `host_name`, `port`, `devices[]`
   with sha256(token)); client stores `hosts.json` (paired hosts with tokens).
4. **Discovery = mDNS/DNS-SD `_tcode._tcp.local.`**, with TXT `host_id`,
   `name`, `port`, `fp`. Hosts, desktop and Android use `mdns-sd`; Android holds
   a WifiManager.MulticastLock for each bounded browse. iOS uses the system
   NetServiceBrowser/NetService resolver through the Swift host bridge, with
   Bonjour service and local-network usage declarations. Every phone pairing
   sheet lists nearby hosts; selecting one fills address, port and fingerprint
   and focuses the still-required code. Discovery is only an address hint,
   never a trust source. Overlay networks generally do not forward mDNS.
5. **Multi-client = mux in front of one host loop; selection is client-local.**
   Every formerly implicit session mutation/query carries `session_id`, including
   drafts. `StartDraft`, fork and plan handoff return an id to their requesting
   client. Stores select by subscribing to that session's `SessionStatus`,
   `SessionEvents` and `GitStatus`, and unsubscribe on navigation. There is no
   host-global active session or `Topic::ActiveSession`. The mux tracks each
   connection's subscriptions and filters events, retaining request-id routing
   for acknowledgements, queries and subscription snapshots. A final unsubscribe
   or disconnect releases the host subscription; another client's subscription
   keeps the session resident. Sidebar activity flags travel with `Index`.
6. **Reconnect**: the client-side `HostLink` outlives connections. Outgoing
   lines buffer while offline. `Subscription.after` is the number of stored
   records already applied by the store. Session snapshots contain `from` and
   only the remaining `records`; `from: 0` replaces, a matching offset appends,
   and a mismatched nonzero offset requests a full replacement. The store calls
   `HostLink::update_after` after applying records, sending the latest subscribe
   line so transport replay caches retain its cursor. The link also replays its
   current subscriptions on Connected and replays retired-topic unsubscriptions.
   Backoff remains 1 s → 30 s with immediate retry on app foreground.
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
   theme. The in-repo platform crates are our own `gpui-ios` and `gpui-android`
   backends, constructed with `gpui::Application::with_platform` (the intended
   extension point); nothing is patched or proposed upstream in gpui itself.
   `gpui-base` 0.6.0 on crates.io declared `gpui-pre-platform` as an unused hard
   dependency that does not compile for iOS/Android; the fix (moving the native
   examples into their own package) is merged on gpui-kit main, so `gpui-base`
   is pinned to that git rev in `[patch.crates-io]` until a release includes it.
   Then the patch line goes away and no `[patch.crates-io]` remains. `tcode-web`
   constructs its platform directly from `gpui-pre-web` (default features off).
   Published `gpui-pre-wgpu` and `gpui-pre-web` are used directly.
10. **Desktop remote affordances (P4b)**: terminal output streams on
    `Topic::Terminal`, preview reverse RPC on per-session `Topic::Preview`, and
    attachment byte upload/readback reach desktop clients. Terminal and preview
    panels are visible remotely. Preview loopback URLs are rewritten to the
    paired host address; dev servers must listen on all interfaces (`0.0.0.0`
    or `::`) and expose their port on the LAN/overlay. `ponytail:` full terminal
    scrollback and a full TCP tunnel multiplexed over the WebSocket. Remote
    Add Project directory browsing remains separate P4 work.

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

No upstream crate is patched. `gpui-ios` and `gpui-android` are our own
backends, and both compile against the published `gpui-pre-wgpu`. `gpui-base`
is pinned to a gpui-kit git revision until a release stops declaring the
unused `gpui-pre-platform` dependency (Decision 9).

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
- Status: **done** (P4a per-client selection + filtered mux + incremental snapshots; P4b terminal over the link, preview reverse RPC, remote attachments; P4c TLS pinning, mDNS discovery, Android emoji; P4d follow-ups).

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


#### P4c notes

P4c upgrades the earlier P1 plaintext transport / UDP notes and the P2 manual-only
mobile discovery limitation. TLS serves both HTTP pairing/static content and
WebSockets on the same port; plaintext is no longer served. The browser loading
page explains the one-time self-signed HTTPS warning; WebSockets follow the page
scheme (`wss` on HTTPS). `/admin/pair`, headless output, URLs and QR codes all
include the full SHA-256 `fp`. The browser saves `/pair`'s `fp` for display only.

Native pairing compares `/pair`'s `fp` to the actual TLS certificate. A pin verifier
also verifies TLS 1.2/1.3 handshake signatures with ring. UI cards display the first
eight four-hex-digit groups; all 32 digest bytes are pinned using constant-time
comparison. Manual pairing shows a comparison page before connecting. Saved
legacy records gain a persistent pin on first connect; later last-used saves
cannot erase it. RemoteClient exposes a typed `OfflineReason::CertificateChanged`
and the existing transport-neutral state stays `Offline`. The protocol-2 integration
also reports expected/received hello versions and caches both subscribe and
unsubscribe lines for reconnect replay. Native screens read
that reason through the host accessor, avoiding changes to shared client state.

Discovery has a three-second desktop/Android browse and four-second Bonjour
browse. TXT parsing bounds all strings and result counts, validates the fingerprint
and checks the TXT/SRV port agrees. Android's JNI bridge reference-counts the
multicast lock and releases it when the browse completes. iOS resolves numeric
addresses through NetService and returns bounded JSON through the existing GPUI
main-thread dispatch bridge. No Apple raw multicast entitlement is needed.

Android bundles the upstream CBDT/CBLC `NotoColorEmoji.ttf` and OFL license in
APK assets, and registers the font database face before any text is shaped.
cosmic-text 0.19's Android fallback eventually scans registered faces; GPUI 0.3.3
recognizes the `NotoColorEmoji` PostScript name, selects Swash ColorBitmap BestFit,
and uploads color BGRA pixels. Do not explicitly load this family as a primary
font: upstream `load_family` removes faces without ASCII `m`; automatic fallback
uses `get_or_insert_font` and bypasses that filter. No gpui-pre-wgpu patch is used.
Font SHA-256: `72a635cb3d2f3524c51620cdde406b217204e8a6a06c6a096ff8ed4b5fd6e27b`.
Upstream: https://github.com/googlefonts/noto-emoji (fonts/LICENSE, OFL 1.1).

P4c acceptance (2026-09-05): workspace formatting and strict Clippy pass. Two
full workspace runs passed (977 tests, 4 ignored), but the latest integrated
reruns fail `store::tests::providers_and_git_replicas_match_live_after_representative_mutations`
in the concurrently owned `crates/ui/src/store/mod.rs`: timeout waiting for
provider/Git replicas at line 1949; an isolated rerun instead found differing
GitStatusStatus values at line 2763. A reduced-concurrency full rerun reproduces
the timeout (309 UI tests pass, 1 fails). No shared store code was changed.
The latest workspace suite therefore is **not all green**. TLS integration tests
cover correct pin, different certificate rejection, pairing TOFU, legacy pin
persistence, 0600 identity files, restart identity stability, and plaintext
refusal (9 integration tests pass). The real mDNS loopback advertise/browse test
also passes; this was not reduced to TXT-only testing. Ring client checks pass
for aarch64-apple-ios-sim and Android arm64-v8a; both native host build scripts
pass and the apps install and launch.

Bonjour on iPhone 17 / iOS 26.5 Simulator, Android AVD `tcode-p2d`, and the macOS
phone preview all discover the Mac. The sheet de-duplicates each host and avoids
loopback addresses. Both devices pair by code, display the certificate pin,
connect over TLS and send a turn. The default Claude provider hit its session
limit; the existing configured Codex provider completed the retry (iOS: `iOS TLS
works.`; Android: `Hello 😀 🎉 🦀 🚀`). The sidebar initially showed its stale
empty-project text while the new-conversation picker correctly listed projects;
after creating a conversation the sidebar updated. This belongs to the shared
store/sidebar work and was not modified in P4c. Chrome accepted the self-signed
interstitial once, paired over HTTPS, displayed the pin and connected over WSS.

**Android emoji limitation remains at font selection.** The bundled CBDT font
is registered, but `/system/fonts/NotoColorEmoji.ttf` has the same family and
PostScript name and is loaded first by
`crates/platform/gpui-android/src/android/platform.rs:272`. On this AVD it is
COLR v1/CPAL (not CBDT/CBLC). Direct Android GPUI diagnostics selected system glyph
IDs 1754/213/2500/1951 with `is_emoji=true`, then returned 0×0 raster bounds. An
independent probe using the exact Swash 0.2.9 rendering sources used by
`gpui-pre-wgpu` reproduced empty masks for those system glyphs. With the bundled
font, the same probe produced `Content::Color`, 59×56 pixels, 13,216 RGBA bytes
for each of 😀 🎉 🦀 🚀. Thus CBDT rendering works; automatic fallback chooses the
incompatible system face. Explicit family fallback is not a workaround because
GPUI's `load_family` removes fonts lacking ASCII `m`.

The smallest viable follow-up is to exclude the system `NotoColorEmoji.ttf`
from that Android platform font-loader list, allowing the registered bundled
CBDT face to win. This does not require a gpui-pre-wgpu patch, but
`crates/platform/**` is outside this task's allowed files, so it was not changed.
Temporary diagnostic instrumentation was removed. `android-p4c-emoji.png`
records the real failure: the completed response contains emoji on the host,
but Android paints only `Hello`. No color-chat success is claimed.

Measured debug APK baseline: 251,731,281 bytes. The final integrated P4c APK:
260,195,576 bytes (+8,464,295 bytes / 8.072 MiB; concurrent native
refactoring also contributes). The font itself is 10,673,480 bytes and occupies
9,956,367 compressed APK bytes; OFL adds 1,986 compressed bytes. The attributable
font+license payload is 9,958,353 bytes (9.50 MiB). The raw APK delta is smaller than that payload because concurrent native
refactoring changed the library size.

Screenshots: `docs/images/mobile/{ios,android}-p4c-{pair,discovery,fingerprint}.png`,
`ios-p4c-turn.png`, `preview-p4c-discovery.png`, `android-p4c-emoji.png`, and
`web-p4c-https.png`.

#### P4a notes

P4a replaces `ResidentSessions.active: Option<_>` with a map of viewed resident
sessions. The parked map and provider generation checks, retained queues,
background task accounting, idle grace/LRU reaper, terminal restoration and
shutdown store barrier remain. Re-adoption moves only the requested session;
opening a second session never removes the first. Commands and detached
completions carry the target id independently of subsequent client navigation.
Draft resource/UI keys use the draft's id so two drafts in one project cannot
share a provider, terminal workspace or composer state.

The protocol is version 2. It removes `SelectSession`, `ActiveSession` and its
replacement event; adds `Unsubscribe`, subscription `after`, per-session Git
status and targeted command/query fields; and changes session snapshots to
`{from, records}`. Subscription reply envelopes carry an optional `request_id`,
which the mux maps back to the requester before the corresponding ack removes
its route. This prevents a second client's snapshot cursor from replacing the
first client's replica. The runtime keeps canonical record vectors alongside
pending FIFO writes so an immediate resubscribe cannot read behind a live event.
Background timeline re-adoption no longer broadcasts redundant full snapshots.
The store advances the remembered subscription after applying each record. The
link sends that latest cursor to update transport replay memory and replays it
on reconnect. The store rejects superseded subscription replies when applying
its event queue, including replies already queued before a newer live record.
This prevents stale empty tails from resetting an up-to-date replica.

The test-only single-client lifecycle fixture owns its selection and forwards
explicit ids into AppState. Existing lifecycle scenarios remain, alongside
real HostLink/HostMux tests with independent scripted providers and store
reconnect/mismatched-tail coverage.

Transport-owner integration: the concurrently owned native/server hello paths
now use `tcode_protocol::PROTOCOL_VERSION` and reject unequal versions. The
server's current rejection reason is "invalid token or protocol version"; the
owner should split its version check to report expected and received versions
explicitly in `hello_rejected` (the constant bump is included here). Those hello
implementations remain owned by the other engineer in
`crates/remote/src/{client,server}.rs`. The native subscription replay key
should recognize both `subscribe` and `unsubscribe` (as the browser now does),
so the latest line per topic also preserves a retired topic. HostLink's retired
subscription replay and inbound topic filter preserve selection isolation even
with an older transport cache.

P4a validation on the integrated branch: workspace format, strict Clippy,
workspace tests, desktop/headless build, the iOS simulator remote-client check
and the mobile WebAssembly check pass. The simultaneous desktop/phone GUI
smoke test remains pending: an isolated headless host and separately paired
clients were prepared, but Computer Use rejected access to the isolated test
app. No claim of manual no-cross-talk verification is made.

#### P4b notes

Terminal PTYs optionally expose a raw-output receiver installed before their
output bridge starts. The runtime owns a 256 KiB `VecDeque` per live terminal;
mailbox callbacks append/evict bytes before publishing them. Subscription replay
and live output share that mailbox, with requester-correlated replay envelopes.
Replays and restarts set `TerminalOutput.reset`; the client replaces its parser
and grid before feeding the retained raw bytes. Restart generations reject late
output from the previous PTY, and destroying a terminal releases its ring.
`cols`/`rows` accompany replay and output; terminal title/exit state remains in
`SessionStatus`. A supplied `LocalTerminalRegistry` selects direct local handles;
without it the drawer renders a client `GridEmulator`, uploads input and resize
commands, and subscribes/unsubscribes terminal topics with the selected session.
Host emulation answers PTY protocol queries once, so remote viewers do not send
duplicate device replies. The ring is recent raw output, not a serialized screen
or complete scrollback: truncation can begin inside an escape/UTF-8 sequence.
`ponytail:` full scrollback and a richer checkpoint format.

Preview payloads have portable protocol DTOs and serde mirrors in `preview-mcp`.
The runtime owns the broker receiver and correlates requests with replies from
per-session `Topic::Preview` subscribers. First response removes the pending
request; a 60-second host timeout returns an error when no WebView responds.
Desktop `AppShell` dispatches these requests into its existing `PreviewPanel`,
including in-process desktop clients. The former local preview receiver remains
an optional compatibility affordance; newly spawned hosts use the wire path.
A missing preview service is started by the runtime, which lets the unchanged
headless launcher register preview MCP with providers. This adds only the
existing workspace `mcp-host` crate as a direct runtime dependency.

Desktop navigation rewrites HTTP(S) loopback authorities (`localhost`,
`127.0.0.1`, `0.0.0.0`) to the paired host's connection address while preserving
port/path/query/fragment, including bracketed IPv6 targets. Dev servers must
listen on all interfaces and be reachable on the LAN/overlay. `ponytail:` a full
TCP tunnel multiplexed over the WebSocket removes that requirement. This phase
uses the first address in the desktop's `PairedHost.addrs`; automatic selection
of a different address by transport reconnect is not exposed to the UI yet.

Image upload already used the host `SessionStatus.attachments_dir`. The missing
piece was rendering: composer thumbnails, timeline thumbnails and lightboxes
still opened the path on the client filesystem. They now fetch `ReadFileBytes`
through a shared host-scoped GPUI asset cache and retain GPUI image decoding.
Compact paste/drop suppression is unchanged. The timeline thumbnail and
lightbox changes are each a single renderer call, needed alongside the composer
change to complete image parity.

Validation: workspace format, strict Clippy and workspace tests passed (983
passed, 4 ignored), as did desktop/headless builds and the iOS simulator and
mobile WebAssembly checks. Three production-mux acceptance tests cover bounded
terminal replay/live PTY input/resize, preview first-response/timeout routing,
and host-directory attachment save/readback. Portability checks retain existing
warnings. A regression test also checks that reading a remote terminal tab's
exit state preserves damage for the drawer renderer.

Mac manual verification is incomplete. An isolated host was seeded and paired,
and its remote desktop client opened the terminal drawer with the host project
working directory. Computer Use subsequently reported missing Screen Recording
permission, blocking visual verification and the three requested screenshots;
composer focus/paste and provider-driven preview were not verified. No manual
parity or screenshot-completion claim is made. The capture-selection-to-composer
command carries optional client-selected text, preserving local fallback while
letting the remote drawer upload its own selection.

#### P4d notes

Discovery now ranks resolved addresses against the interfaces which received
the mDNS answer, ignoring virtual bridge interfaces for same-/24 preference.
Native browsing creates a fresh daemon for every request on a dedicated thread.
The reported late-beacon miss did not reproduce with the long-lived phone
preview; it found a host started later after the three-second browse, while also
reproducing the bad bridge-address choice. The admin pairing curl completed with
HTTP 200 and OpenSSL negotiated TLS 1.3, so the TLS accept path was unchanged.
The terminal replay test uses a mux-delivered readiness marker after suppressing
the shell prompt, eliminating startup-output races without relaxing its exact
256 KiB replay assertion.
