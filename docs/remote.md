# Use tcode from other devices

tcode runs on the machine that holds your projects, provider tools and terminal
processes. You can open that machine from another desktop, a phone, a tablet or
a browser over your LAN or overlay network. There is no relay service. For a
shorter introduction, see
[Use tcode from other devices in the README](../README.md#use-tcode-from-other-devices).

## Concepts

| Term | Meaning |
| --- | --- |
| Machine | The computer where tcode runs providers and terminals and stores projects and threads. You can use the desktop app or `tcode-headless` there. |
| Device | A desktop, phone, tablet or browser that opens a machine and sends actions to it. |
| Browser password | Protects the web page served by `tcode-headless`. Set it on first open, or preset it with `TCODE_PASSWORD`. A successful login issues a device token. |
| Adding a machine | Exchanging a single-use, six-digit connection code for a device token. A code expires after five minutes; five wrong attempts invalidate it. Generating a new code replaces the previous code. |
| Connected device | One saved device record on the machine, with a name and a token that you can remove. |

Add the machine separately on each device. Project files, provider processes and
terminal processes stay on the machine. Your devices receive the thread content
and other data they need to show and operate that work.

## Use the desktop app on your machine

1. Open the desktop app on the machine where your projects and agent CLIs live.
   Configure the providers and add your projects there.
2. Open **Settings → Other devices**. Set **Port** (default `47420`) and
   **Machine name**. Use **Apply** to apply changes to an already running
   listener.
3. Enable **Let other devices connect to this machine**. The listener binds all
   IPv4 interfaces. Allow its TCP port through your firewall from the LAN or
   overlay you use.
4. Read the connection code, or scan the QR code from your
   phone. Use **New code** if the code expires or you need to add another
   device.
5. Check **Connected devices** after adding a device. Use **Remove** to withdraw
   its access. Keep the app and machine running while other devices use it.

The desktop keeps listening and letting nearby devices find this machine when
its window connects to another machine. **Let other devices connect to this
machine** therefore remains available, and devices that already opened this
machine continue working. The desktop app accepts native app connections; it
does not serve the browser app.

## Run tcode without the desktop app

### Install and start

1. Download an archive from
   [Releases](https://github.com/Tryanks/tcode/releases). Names follow
   `tcode-headless-<version>-<platform>-<arch>`: Linux uses `.tar.gz`, macOS and
   Windows use `.zip`; platforms are `linux`, `macos`, `windows`, and architectures
   are `x64` or `arm64`. Windows contains `tcode-headless.exe`.
2. Extract the archive and install the executable. For Linux x64, replace
   `VERSION` with the release version without its leading `v`:

   ```sh
   tar -xzf tcode-headless-VERSION-linux-x64.tar.gz
   install -d "$HOME/.local/bin"
   install -m 755 tcode-headless "$HOME/.local/bin/tcode-headless"
   ```

3. Install and authenticate your agent CLIs under the account that will run
   tcode. Ensure that account's `PATH` includes them and that it can access your
   project directories.
4. Start tcode with a persistent data directory:

   ```sh
   "$HOME/.local/bin/tcode-headless" serve \
     --listen 0.0.0.0:47420 \
     --name build-server \
     --data-dir "$HOME/.local/share/tcode-host"
   ```

   Startup prints `Browser: http://host:port/` without a code fragment and
   **Set a password on first open** or **Password protected**. Open that URL
   and set a password (at least eight characters, entered twice). It also prints
   a connection code and QR for native desktop and phone clients. Use an address
   reachable from your device, not `0.0.0.0`.

   To preset the browser password, supply `serve --password PASSWORD` or set
   `TCODE_PASSWORD` in the service environment. The CLI option takes precedence.
   A preset updates the password on startup and keeps existing device tokens.
5. Allow inbound TCP `47420` from your LAN or overlay. In another shell on the
   same machine, generate a new code when needed:

   ```sh
   "$HOME/.local/bin/tcode-headless" pair --listen 127.0.0.1:47420
   ```

   `pair` contacts the running tcode process over loopback. Its `--listen`
   selects the port and IPv4/IPv6 family; it does not contact the supplied
   address. Keep the listener reachable on loopback, as with the default
   wildcard bind. A listener bound only to a specific LAN address cannot answer
   this command.
6. After browser login, open **Settings → Other devices** to view the code and
   QR, generate a new code, list connected devices, or revoke a device. **Allow
   other devices** controls native code pairing; it defaults to on and is saved
   in the data directory. Disabling it does not stop web login or disconnect
   already authorized devices. Use **Remove** to revoke their access.
7. Open this machine from another device using the instructions below. To
   prepare projects with the local desktop UI, stop `tcode-headless` first and
   open the desktop app with the same data directory. Do not run two local tcode
   processes against the same directory.

On Unix, Ctrl-C requests shutdown and a store flush. The CLI reference is:

```sh
tcode-headless --help
```

Add the installed directory to your `PATH` to use that short command. Help is a
top-level option; the subcommands do not accept `--help`.

To change or reset the browser password, stop the headless process, then run:

```sh
tcode-headless set-password --data-dir /path/to/tcode-host --password NEW_PASSWORD
```

`TCODE_PASSWORD` also supplies the password for this command. Existing tokens
remain valid by default, so saved phones and browsers remain connected. Add
`--revoke-tokens` to invalidate every device token, then restart the host. Update
any service preset too, or it will replace the password on the next startup.
Do not run this command against a data directory used by a running process.

### Data directory

The `tcode-headless --data-dir` option takes precedence over `TCODE_DATA_DIR`.
Without the option, `TCODE_DATA_DIR` selects the store; otherwise tcode uses the
platform app-data directory with a `tcode` subdirectory. This includes settings,
threads and connection records. It does not move project working directories
into the store.

For example, this selects the same store as the explicit path above:

```sh
TCODE_DATA_DIR="$HOME/.local/share/tcode-host" \
  "$HOME/.local/bin/tcode-headless" serve --name build-server
```

Keep this directory across restarts and back it up. Use a separate directory
for a separate machine identity. The desktop app also honors `TCODE_DATA_DIR`, including when adding or connecting to a machine.

### Run with systemd

On a Linux system with systemd, create a user unit at
`~/.config/systemd/user/tcode-headless.service`. This example uses the
executable and data directory above. Extend `PATH` with the absolute directories
containing your agent CLIs; a service does not read your interactive shell
setup.

```ini
[Unit]
Description=tcode without the desktop app

[Service]
Type=simple
WorkingDirectory=%h
Environment=PATH=/usr/local/bin:/usr/bin:/bin
ExecStart=%h/.local/bin/tcode-headless serve --listen 0.0.0.0:47420 --name build-server --data-dir %h/.local/share/tcode-host
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
KillMode=mixed
TimeoutStopSec=60

[Install]
WantedBy=default.target
```

`SIGINT` uses tcode's shutdown-and-flush path. Stop any foreground tcode process
using the same port or data directory, then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now tcode-headless.service
journalctl --user -u tcode-headless.service -f
```

Treat the journal as sensitive: startup includes the connection code and link.
Configure user lingering through your system administrator if this service must
run after logout. Generate subsequent codes with the same `pair` command above.

### Build with browser support

Release archives include the browser bundle. For a source build, run these
commands from the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.121 --locked
crates/web/build.sh
cargo build -p tcode-headless --release --features web
```

The script optionally uses `wasm-opt` if installed. Build the bundle before the
`tcode-headless` binary: the `web` Cargo feature embeds `index.html`,
`tcode_web.js` and `tcode_web_bg.wasm` from `crates/web/dist`. It is a build
option, not a `serve` flag. The resulting executable is
`target/release/tcode-headless`.

tcode serves the browser app at `/` over HTTP, on the same port as the
connection-code exchange and WebSockets. Without `web`, static requests
return 404 while native apps can still add and connect to the machine. The
browser runs the same shell as the desktop app; it lays itself out from the
canvas size, so a wide tab gets the desktop split and a narrow one gets the
compact stack.

## Open a machine

### From another desktop

1. Open **Machines** on this device — the sidebar entry under the search field,
   or the opening screen when no machine has been added yet.
2. Under **Nearby machines**, choose **Refresh**, then pick a machine. That fills
   the address on the **Add a machine** page; you still
   need the connection code. Otherwise choose **Add a machine** and fill it in
   yourself. You can also paste the machine's connection link into the address
   or code field.
3. Choose **Add a machine**, confirm the machine name, then choose **Connect to ‹machine›**.
4. The same window immediately opens that machine's projects and threads.
   Choose another machine under **Your machines** to switch again;
   **This machine** restores the local workspace, and a machine row's
   **⋯ → Disconnect** leaves without removing it. Switching closes only this
   device's old link, not tcode on either machine. Opening **Machines** by itself
   does not change the connection.

You can also add a machine from the desktop executable. Replace the sample
address and code with the machine's current values:

```sh
tcode --pair 192.168.1.10 47420 123456
```

This saves the machine in this device's `hosts.json` and prints its machine ID.
Replace
`HOST_ID` below with the printed ID:

```sh
tcode --connect HOST_ID
```

`--connect` accepts a saved machine ID, not an address or WebSocket URL. Use the
same device data directory for `--pair` and `--connect`.

### From a phone or tablet

1. Install the Android debug APK or re-sign and install the unsigned iOS IPA.
2. The app opens on **Machines** — the same screen every device has. Scan the
   machine's QR code, or fill in the address and connection code.
   Selecting a machine under **Nearby machines** fills the address; you still enter the code. Allow camera or local-network access
   when the system asks. Native QR scanning uses AVFoundation on iOS and
   CameraX/ML Kit on Android; a simulator's permission and cancel flows do not
   prove that real camera recognition works.
3. Choose **Add a machine**, confirm the machine name, then choose **Connect to ‹machine›**.
4. Open a thread from the list, or use **+** to start one. Read replies, send or
   queue a message, steer a running turn, stop it, and answer approvals — the
   same views the desktop shows, laid out for the width.
5. **Settings** is the full settings page, not a reduced copy. **Back** returns
   list → machines, which disconnects; leaving a thread keeps the connection.
   You connect to one machine at a time.

A tablet wide enough for the split gets the split, and rotating it back to
portrait returns to the stack with the same thread and draft. See
[the layout rule](DESIGN.md#one-shell-one-layout-rule).

### From a browser

1. Open the `Browser:` link printed by `tcode-headless`, for example
   `http://192.168.1.10:47420/`.
2. On first open, set a password with at least eight characters and confirm it.
   Later visits without a saved token show **Log in**. A successful login saves
   a device token and opens Threads; a valid stored token skips the form.
3. A rejected or revoked token returns to the login form. Five wrong passwords
   lock browser login for five minutes. Code pairing for native clients remains
   available during this password lockout.
4. **Settings → Other devices** manages this headless machine's native pairing:
   enable or disable new pairings, read its current six-digit code and QR,
   generate a new code, and remove paired devices. The code can also be obtained
   with `tcode-headless pair`. Phones and other desktop apps always use this code,
   never the browser password.
5. The address stays fixed to the page's origin and nearby search is hidden.
   Open another machine's URL to use that machine. Resizing the page switches
   between the shared wide and compact layouts.

The browser stores machines and tokens in this origin's `localStorage` under
`tcode.hosts`, and the last machine under `tcode.last_host`. Clearing site data
means you must log in with the password again. WebSockets follow the page's scheme:
`ws://` for HTTP and `wss://` for HTTPS supplied by an external tunnel.

### Machine addresses

The **Address** field accepts `host`, `host:port`, or a full HTTP(S) origin.
Shorthand defaults to HTTP and port `47420`; explicit URLs keep their standard
port, so `https://tunnel.example.com` uses 443. IPv6 literals are supported,
including `[fd00::1]:47420`. Paths, credentials, queries and fragments are not
machine origins.

Saved machines contain one `origin`, for example `http://192.168.1.10:47420`.
Older `hosts.json` records migrate their first saved address and port to an HTTP
origin, preserving the machine ID, name, token and last connection time. Saving
writes the new format. Discovery and pairing invitations provide origin hints.

### Device preferences

Appearance, language and device name belong to this device, not the machine:
an explicit choice on this device overrides the connected machine's replicated
setting, and restoring that row reveals the machine's setting again. Native apps
keep these preferences next to `hosts.json` in their own data directory; the
browser keeps them in `localStorage`. iOS seeds the device name from the device
name, Android from the device model.

## Networking

| Port | Purpose |
| --- | --- |
| `47420/TCP` by default | HTTP listener: adding devices, WebSockets, authenticated preview proxy, and the optional browser app. Use your configured port if you change it. |
| `5353/UDP` on the LAN | Bonjour / mDNS search for `_tcode._tcp.local.` machines. Optional when you enter an address yourself. |

Connect the machine and your other devices to the same LAN or overlay, such as
Tailscale or EasyTier. Allow the listener through the machine's firewall and any
overlay access rules. tcode provides no relay or public discovery service. Treat
nearby-machine search as LAN-only: it does not cross a normal overlay connection.
Enter the machine's overlay address and port when it does not appear nearby.
Nearby-machine search advertises identity and address hints; it does not grant
access.

### Preview browses from the machine

Windows and Android embedded previews use the connected machine's network for
all HTTP and HTTPS traffic. `http://localhost:5173/app` opens the dev server on
the machine, even when it listens only on loopback. The URL stays unchanged in
the address bar, redirects, scripts and `preview_status`. No dev-server port
needs to be exposed to the LAN. Open-in-system-browser still opens on this
device and cannot use the embedded preview's routing.

The forward proxy runs whenever hosting is enabled, on the same configured TCP
port as pairing and WebSockets (default `47420`). It requires
`Proxy-Authorization: Basic` with username `tcode` and the paired device token
as password. Plain HTTP is forwarded; HTTPS uses opaque CONNECT tunnels with
certificate validation performed by the client webview, without TLS interception.
The machine connects directly to destinations; OS routing and global TUNs apply.
tcode does not read upstream-proxy environment variables or system proxy settings.
There is nothing new to configure in hosting settings.

The listener admits at most 256 concurrent connections, including WebSockets.
Proxy connection setup times out after 10 seconds, and traffic idle for 60 seconds
is closed. Revoked proxy tokens are rechecked every five seconds; stopping hosting
closes tunnels. Connection logs contain destination host/port and peer, never
request bodies or tokens. For headless diagnostics use `RUST_LOG=tcode_remote=info`.

Android requires WebView's `PROXY_OVERRIDE` capability. It removes implicit
localhost bypasses, waits for the override before navigation, and clears it when
the attachment's views are destroyed. The override is process-wide, so embedded
preview views belong to one attachment.

macOS attached previews are unavailable: WebKit bypasses its Network proxy
configuration for destinations on the client’s local interfaces, including
localhost and its own LAN addresses. Clearing excluded domains, explicit match
domains, disabling failover, and installing the authenticated configuration before
creating the WebView do not prevent this bypass. tcode refuses attached WebView
creation through the existing error surface so navigations, redirects, and
subresources cannot connect directly. Local macOS previews remain available.

Windows uses WebView2's proxy configuration and proxy authentication callback, with implicit
loopback bypass disabled. Unsupported proxy facilities fail closed; there is no
unauthenticated IP allowlist or direct-network fallback. Desktop proxying currently
requires a direct HTTP machine origin, typically over a trusted LAN or VPN.
HTTP reverse-proxy tunnels must support forward-proxy requests and CONNECT to
carry preview traffic; ordinary website forwarding is insufficient.

The iOS embedded preview remains unsupported. A future backend can use
WKWebView `proxyConfigurations` on iOS 17+ with the same paired credential and
attachment lifetime. The browser client cannot override its browser's proxy.

## Security

The LAN transport is plain HTTP and `ws://`. Anyone on the LAN who captures
traffic can read the device token and the work sent over the connection. Use a
tunnel or VPN on untrusted networks. tcode never provisions or manages TLS.

Anyone holding a paired device token can browse through the machine, including
its loopback and private-network services. Treat it as network access to that
machine. Proxy Basic authentication is not encryption; protect the connection
with a trusted LAN or VPN. Removing a connected device revokes its proxy access.

### Device tokens and storage

Adding a device issues a random bearer device token. The machine's `remote.json`
stores device names, IDs and token hashes, not raw tokens. For headless browser
login it also stores a randomly salted PBKDF2-HMAC-SHA256 password hash with
600,000 iterations; it never stores the password itself. Native apps store raw
tokens and origins in `hosts.json` in their own data directory.
Phone records are in the app's private data directory; Android uses its
`filesDir`. Browser records use `localStorage` as described above.

On Unix, tcode writes `remote.json` and `hosts.json` with mode `0600`. These are filesystem permissions, not file
encryption. Protect this device's data directory and browser profile: possession
of a device token grants that device's access. There are no per-device project
permissions or read-only device roles.

In the desktop app on the machine, use **Settings → Other devices → Connected
devices → Remove**. New connections using that token are refused; an existing
connection is checked at a server keepalive tick and closed when removal is
detected. Do not rely on an immediate disconnect. **Remove** under **Your
machines** only forgets this device's record; it does not withdraw the token on
the machine.

There is no `tcode-headless` remove subcommand. To remove access without the
desktop UI, stop `tcode-headless`, back up `remote.json`, remove the matching
entry from its `devices` array, preserve its permissions, and restart. Do not
edit that file while tcode is running: it holds the device list in memory.

## Reaching your machine from outside

- **Tailscale / WireGuard:** once your machine and device are on the VPN, there
  is nothing to configure in tcode. Save the machine's VPN address, such as
  `http://100.64.0.10:47420`, and allow the listener in the VPN's access rules.
  Discovery usually stays on the LAN, so type the address when needed.
- **Cloudflare Tunnel / frp:** forward your tunnel's HTTPS endpoint to tcode's
  local HTTP listener, including WebSocket upgrades. Save the tunnel's HTTPS
  origin, such as `https://tunnel.example.com`. The tunnel manages TLS.
- **Tailscale HTTPS:** save its HTTPS origin when using its HTTPS forwarding.
  Native clients use standard trusted roots and hostname validation.

Browser password login, native six-digit pairing codes, and device-token
authorization keep the same behavior through a tunnel or VPN. Protect tunnel access and your stored tokens. tcode does not
encrypt stored JSON or project files; preview servers and provider connections
use their own transports.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Browser password is wrong or forgotten | After five wrong attempts, wait five minutes. To reset it, stop the host and use `set-password`; add `--revoke-tokens` if saved devices should lose access. |
| Native pairing is disabled | Enable **Allow other devices** in the logged-in browser’s **Settings → Other devices**. Web login is independent. |
| Connection code is wrong or expired | Generate **New code** on the machine, or run `tcode-headless pair`. Codes expire after five minutes, after use, after five wrong attempts, or when replaced. Get a separate code for each device. |
| Cannot reach the machine | Check that tcode is running, that the address is reachable from this device, and that the TCP port is allowed. Use the overlay address if no nearby machine appears. This device's `127.0.0.1` points to itself. |
| `tcode-headless pair` cannot reach a running listener | Check its port and IPv4/IPv6 family. The listener must accept loopback connections; `pair` always uses loopback. |
| Browser shows 404 | Use a `tcode-headless` build with `web`. The desktop app and builds without the bundle do not serve the browser app. |
| Browser fails before adding the machine | Check the HTTP host or your HTTPS tunnel. Check that JavaScript and site storage are allowed. |
| Connection rejected after adding the machine | Check whether the device was removed. Log in again in the browser, or add the native device with a fresh code if access is intended. Keep the machine and device builds on a matching protocol version. |
| Preview cannot load a dev server | Check that the dev server runs on the machine, hosting is on, the device is still paired, and its WebView supports proxy routing. Use `localhost` for a machine-loopback server. |

**Syncing… / 同步中** means the workspace is waiting for its baseline: applied
Index and Settings snapshots, plus SessionStatus and SessionEvents for the
selected thread. A newly attached workspace shows neutral loading rows until
these arrive; it never labels an unknown list as empty. Cached lists and thread
content remain visible during reconnect, with Syncing until replay completes. **Reconnecting · attempt N** appears immediately when a connection
is lost, with its failure reason. Retry delays grow exponentially from one to thirty seconds with ±20% jitter
(and a final 30 s cap). Only a connection stable for at least 30 s resets backoff;
a quick handshake followed by another disconnect does not. Native clients
race addresses resolved for the saved origin at 250 ms intervals, with one 15 s budget per address for
TCP, optional TLS, WebSocket upgrade and hello. The first successful address wins.

Native clients probe after 10 s without an inbound frame and reconnect if no
frame arrives in the next 20 s. Every connected write has a 10 s deadline.
Browsers send an application ping after 15 s without an inbound message and
reconnect after another 20 s without a reply. Browser timers may be delayed in
background tabs; making the page visible or coming online wakes reconnection.

Native foreground cancels pending backoff. After at least 10 s in the background
it opens a new socket and replays subscriptions from applied cursors. After a
shorter absence it probes once, allowing 3 s for a reply before reconnecting.
Foreground and Active from the same return do not issue duplicate probes.
Native network-path callbacks are not installed; lifecycle wake is the native
wake signal.

When a saved HTTP LAN origin is unreachable or times out, the native client
browses nearby machines during retry. A changed LAN hint for the same machine ID
updates and persists the origin and wakes an immediate retry. The token is never
changed. HTTPS tunnels and the browser's fixed origin skip this refresh.

Outgoing commands and queries are bounded to 256 lines and 8 MiB per attachment,
including lines held for retry. A full queue rejects the new line with
`queue_full` (QueueFull) and logs the error; older lines are retained. Subscription
updates coalesce by topic outside those slots. The store exposes the pending
count through `queued_outgoing()`.

A disconnected device keeps cached thread content for reading and disables
writes; unvisited threads may have only cached list information. Subscriptions
resume after reconnecting. Temporary network failures keep retrying. Rejected
authentication stops at **Offline** with **Pair again**; protocol mismatches
stop with **Update the app**. TLS errors on an HTTPS tunnel are logged and
reported as unreachable.

## Limits

- Each device selects its own thread independently. Navigating on your phone
  does not change the thread selected on another desktop. Actions still operate
  on the same machine's projects and threads; selection independence is not
  access isolation.
- A device's terminal scrollback is the machine's retained ring, capped at
  **1000 rows**. Older output is gone from the machine too, so no device can
  scroll further back than that.
- The terminal renders text. In-grid images (sixel, iTerm2, kitty graphics) are
  not supported on any device, including the machine's own desktop window.
- Android has an embedded WebView preview with history, JavaScript automation,
  navigation errors and visible-page PNG capture. iOS and the browser client
  retain URL/open/copy actions without an embedded preview backend. Android
  does not scan local development ports; enter a machine-relative URL.
- Phones, tablets and browsers do not run providers, terminals or project files
  locally; the machine does. Every product view is present on them — threads,
  chat, approvals, the terminal, diff, plan, preview, search, import and export —
  and what differs is which *operations* the device can perform. A native file
  dialog, an embedded preview browser, macOS permission grants, dictation and
  "open in editor" need a capability the device either has or does not; where it
  does not, the view says which machine can do it and offers what it can (open
  externally, copy, type the value). Prepare projects and provider installations
  on the machine. See
  [capability-appropriate UI](DESIGN.md#capability-appropriate-ui).
- Mobile release artifacts are development builds: the Android arm64 APK is a
  debug build, and the iOS arm64 IPA is an unsigned debug build. Re-sign the IPA
  with your own signing identity and provisioning profile before device
  installation. For Android, enable USB debugging and authorize your computer,
  then replace `VERSION` with the downloaded release version:

  ```sh
  adb install -r tcode-VERSION-android-arm64-debug.apk
  ```

## History replay limits

Protocol version 3 requires both host and client to support bounded history;
older peers receive the existing update-required handshake response. Opening a
conversation subscribes to its latest 200 event records. A session
snapshot includes the absolute `from` offset and `total` record count; the client
can request older records with `SessionHistoryPage { session_id, before, limit }`.
The exclusive `before` cursor pages backwards; replies are always chronological
and include their absolute `from`. Backwards pages contain at most 200 records.

Each snapshot or history page is capped at 8 MiB including its serialized wire
envelope. The host shrinks the contiguous range and sets `truncated` when the byte
limit reduces the requested count. A record that cannot fit alone returns
`history_record_too_large`; records are never split or silently skipped.

`Subscription.after` remains the absolute next event cursor, not a count of loaded
records. A reconnect replays from that cursor exactly. Byte-limited forward tails
start at `after`; advancing the applied cursor obtains the next contiguous tail.
A fresh subscription (no cursor) starts at the latest history window. Switching
conversations retires its subscription request IDs, and stale replies cannot
replace the currently selected conversation.

## Weak networks

Reads and writes have separate lifetimes. A transport-reported disconnect fails
in-flight queries immediately with `disconnected`; the workspace refreshes its
baseline during Syncing. A read issued without a connection fails the same way.
While connected, a query without a result for 15 seconds fails with `timeout`.

Retained commands use a client-generated UUID v4 key. They remain in an ordered
outbox until Ack, including across reconnects, attachment changes and client
relaunch. Native clients atomically replace an `outbox-<encoded-host-id>.json`
file beside `hosts.json`, with private file permissions; browsers use the
origin's `tcode.outbox.<host-id>` localStorage entry. Clearing client data clears
these pending writes. An admission storage error is a real failure, not a
successful send.

After hello and subscription replay, the oldest write is sent first; the next
waits for its Ack. The same key is used on every redelivery. The outbox is bounded
at 256 entries and 8 MiB of serialized entries. The oldest surplus entry fails
with `outbox_full`. A write already received by the host cannot be recalled by
client-side eviction.

The runtime retains the last 512 completed command keys and Ack results per
authenticated device, including rejected commands. Cache hits refresh their LRU
position and return the original Ack without executing again. This cache survives
WebSocket replacement, but not a host-process restart or eviction from the 512-key
window. Exactly-once redelivery applies within that window. Device identity is
assigned by the authenticated server, never supplied by a client key prefix.

If a connected retained command has no Ack for 30 seconds, the client logs a
stalled acknowledgement and forces reconnect. The pending command remains in the
outbox. The request deadlines and liveness constants live together in
`crates/client/src/heartbeat.rs`: native idle probe at 10 seconds, browser idle
probe at 15 seconds, and a 20-second liveness reply budget. Transport send and
handshake budgets still apply independently.

A pending user message is muted with **Sending…**, or **Waiting for connection…**
when the workspace is reconnecting or syncing. It becomes normal only after Ack.
A rejected send or outbox overflow shows an inline error with **Retry** and
**Discard**. Retrying a terminal failure creates a new action and key. An approval
decision in the outbox disables the approval buttons and keeps a pending caption.
Transient reconnects belong in the connection banner, not failure toasts.

Protocol 4 adds the optional `key` field. Hello advertises support for versions 3
and 4 using the version-3 hello as its baseline, allowing a strict older host to
accept it. A version-3 host ignores `key` and still works, but cannot deduplicate
redelivery; a lost Ack can therefore repeat a mutation. Upgrade both ends for the
bounded deduplication guarantee. Hosts older than version 3 remain incompatible
with bounded history replay.
