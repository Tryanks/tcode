# Working remotely: the browser and phone clients

Tcode runs agents on a computer. A phone cannot run Claude Code, and neither can
a browser tab — there is no process to spawn, no filesystem to edit, no PTY.

So the other clients are not smaller copies of the desktop app. They are
**remote controls for a machine that is already running one**. The desktop app
(or a headless server) keeps being the place where work happens; the browser and
the phone watch it, send turns to it, and answer its approval prompts.

Everything on this page follows from that. If the host is asleep, there is
nothing to connect to.

---

## What actually works today

Be precise about this, because "it compiles" and "somebody used it" are very
different claims.

| | Status |
| --- | --- |
| Desktop host, embedded | Working. Exercised end to end: a remote client sent a turn and approved a command, and the agent ran it. |
| Headless host (`tcode-server`) | Working. Same, driven from the CLI probe. |
| Browser client | Builds and runs. Follows a session, sends turns, answers approvals. |
| Android client | **Links** for `aarch64-linux-android`, all JNI entry points exported. Has never run on a device. |
| iOS client | **Links** for `aarch64-apple-ios` against the iOS SDK. Has never run on a device or simulator. |

The two mobile rows are the honest ones. Linking is a real result — it proves
every symbol that GPUI, `gpui_wgpu`, `wgpu`, Metal, Vulkan and `psm` need
resolves, which a `cargo check` never establishes. It is not the same as having
seen a frame. The most likely first failure on both is renderer creation meeting
real hardware for the first time. See
[`crates/tcode-android/README.md`](../crates/tcode-android/README.md) and
[`crates/tcode-ios/README.md`](../crates/tcode-ios/README.md) for exactly what
each one does and does not implement.

---

## Turning on the host

### From the desktop app

Nothing to enable. The app starts a sync host on loopback at launch and writes
two things to its log:

```text
sync host listening at ws://127.0.0.1:54321/sync
pair a device within 180s with code K7M2QX
```

The address is what a client connects to. The code is what it connects *with* —
see [Pairing](#pairing) below.

### Headless

`tcode-server` is the same host with a different `main`. Use it when the machine
doing the work has no display, or when you want the agent to keep running after
you close the laptop lid on the app.

```sh
cargo run -p tcode-server
cargo run -p tcode-server -- --bind 0.0.0.0:7777
cargo run -p tcode-server -- --print-token
```

It has no screen to print a pairing code onto, so it hands out the durable token
directly with `--print-token`.

### A word about `--bind`

The default is `127.0.0.1`, and that default is load-bearing.

The sync protocol runs over plain WebSocket. There is no TLS in it, and the
token is the only thing protecting the connection. On loopback that is fine: to
reach the socket you already have local access, and if you have local access you
could have run the agent yourself.

Widen the bind address and both assumptions break at once. The token now crosses
a network in the clear, and the six-character pairing code — which is defensible
precisely because an attacker must first reach the socket — becomes something
anyone on the network can start guessing at.

If you need this across machines, put it behind something that provides
transport security and access control: a WireGuard or Tailscale network, an SSH
tunnel, or a reverse proxy terminating TLS. Do not simply bind `0.0.0.0` on a
network you do not control.

---

## Pairing

A new device gets its credential by trading a short code for a durable token,
once. After that it reconnects with the token and never sees a code again.

1. Read the six-character code off the host.
2. Enter the host address and the code on the client's connect screen.
3. The client stores the token it gets back and goes straight to the session
   list from then on. **Sign out** discards it.

The code is deliberately small and deliberately weak on its own:

- it expires in **three minutes**;
- it is **single use**, so even a correct guess still has to beat the real
  client to it;
- it is worth nothing to anyone who cannot already reach the host's socket.

Its alphabet omits the characters people misread aloud or mistype — no `O`/`0`,
no `I`/`1`/`L`, no `S`/`5`, no `B`/`8` — because a code exists to be read off one
screen and typed into another. Case and separators do not matter: `k7 m2 qx` is
the same code as `K7M2QX`.

There is no account, no identity provider, and no sign-in. That was tried and
removed on purpose; the reasoning is in
[`docs/multiplatform-plan.md`](multiplatform-plan.md) §8.2. The short version:
device pairing is the same shape as Syncthing's device IDs, Tailscale's
pre-auth keys, or `authorized_keys` — a one-time manual admission per device,
which is a cost you pay once and which never depends on a third party being
reachable.

---

## The browser client

The web client is a **full** client, not a viewer: it follows a session, sends
turns, and answers approval requests.

It is not a web page wrapping the desktop UI. It is the same Rust UI code
compiled to WebAssembly and drawn with WebGPU, which is why it looks and behaves
like the app rather than like a website.

### Building and serving it

This one has real prerequisites. The pinned GPUI revision needs nightly Rust, a
locally rebuilt standard library, and the wasm atomics features:

```sh
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals" \
RUSTC_BOOTSTRAP=1 \
cargo +nightly -Zbuild-std=std,panic_abort check \
  -p tcode-web --target wasm32-unknown-unknown
```

The full link and serve invocations are in
[`crates/tcode-web/README.md`](../crates/tcode-web/README.md); they are long
enough that copying them from one place is better than maintaining two.

### The header requirement that will bite you

WebAssembly threads need `SharedArrayBuffer`, and browsers only expose it to a
cross-origin isolated page. Whatever serves the built assets **must** send:

```text
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Opener-Policy: same-origin
```

Without them the dispatcher cannot start its worker threads and the client will
not run. `trunk serve` is already configured to send them; a plain static file
server is not. This is the single most likely reason a working build fails to
start in a browser.

### Credentials

The token lives in `localStorage`, so a reload reconnects without retyping.

`?url=` prefills the host address as a convenience — an address is not a secret.
There is deliberately no `?token=`: a durable credential has no business in an
address bar, in browser history, or in a referrer header.

---

## The phone clients

Android and iOS mount the same shared client the browser does. Each platform
crate is small on purpose: it owns the window, the touch events and the
lifecycle, and nothing else.

The split is two layers:

- **`gpui-android` / `gpui-ios`** implement GPUI's `Platform` trait for those
  targets — window, display, dispatcher, text system, input. They know nothing
  about tcode and could be offered upstream on their own.
- **`tcode-android` / `tcode-ios`** are the app shells. One owns Java and JNI,
  the other Objective-C and UIKit. Neither knows GPUI's internals.

Build instructions and prerequisites — including the Android NDK revision, and
why an NDK is required even for a `cargo check` — are in each crate's README.

### What they do not do yet

Both are honest about this rather than pretending: credential storage returns an
error instead of silently discarding secrets (it needs Keystore on Android and
Keychain on iOS), IME caret positioning is left to the platform, and thermal
state reports nominal rather than guessing. Neither has run on hardware.

---

## Under the hood

A tcode session is already an event log — an append-only sequence of
`AgentEvent`s that a pure reducer folds into the visible timeline. That is what
makes remote work cheap: a client does not need a parallel implementation of the
app's state, it needs the same events and the same reducer.

So the wire protocol is thin. Clients send commands, the host sends events, and
each event carries a per-session sequence number that a reconnecting client uses
as a cursor. Frames, version negotiation and delivery guarantees are specified
in [`docs/sync-protocol.md`](sync-protocol.md).

The architecture, the decisions taken, and the approaches rejected along the way
are recorded in [`docs/multiplatform-plan.md`](multiplatform-plan.md).
