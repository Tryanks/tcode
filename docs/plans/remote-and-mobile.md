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
   theme. Platform backends (`gpui-ios`, `gpui-android`,
   `gpui-platform-mobile`, `gpui-wgpu`, `gpui-apple`, `gpui-web`) are vendored
   from Eauth under `crates/platform/` and applied via `[patch.crates-io]`.
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
- `crates/mobile` (`tcode-mobile`): Hosts screen (paired list, pair by
  code/QR, discovered), Sessions screen (projects grouped, status glyphs),
  Chat screen (timeline, approvals, composer, model picker, connection banner).
- `crates/ios` (staticlib + xcodegen host, camera QR via AVFoundation) and
  `crates/android` (cdylib + gradle host, QR via the same pattern as Eauth's
  `QrScannerActivity`).
- Gate: builds for `aarch64-apple-ios-sim` and `arm64-v8a`; app runs on the
  iPhone 17 simulator and the `Lims20Pixel6` AVD against `tcode-headless` on
  this Mac (`127.0.0.1` / `10.0.2.2`); screenshots of hosts, pairing, sessions,
  and a live chat turn checked into `docs/images/mobile/`.
- Status: pending.

#### P2a notes

P2a vendors the mobile GPUI backends and adds native shells with a safe-area-aware
placeholder view. The checked-in smoke-test captures are
`docs/images/mobile/ios-p2a-hello.png` and
`docs/images/mobile/android-p2a-hello.png`.

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
- Status: pending.

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
