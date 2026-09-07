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

   Startup prints a connection code, its expiry, a
   `tcode://pair` link and a terminal QR code. Release builds also print browser
   HTTP URLs. Use an address reachable from your device, not `0.0.0.0`.
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
6. Open this machine from another device using the instructions below. To
   prepare projects with the local desktop UI, stop `tcode-headless` first and
   open the desktop app with the same data directory. Do not run two local tcode
   processes against the same directory.

On Unix, Ctrl-C requests shutdown and a store flush. The CLI reference is:

```sh
tcode-headless --help
```

Add the installed directory to your `PATH` to use that short command. Help is a
top-level option; the `serve` and `pair` subcommands do not accept `--help`.

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
   `http://192.168.1.10:47420/#code=123456`. The link carries the single-use
   connection code and adds the machine automatically on first load.
2. The browser exchanges the code, saves the device token, removes the fragment,
   and opens Threads. Later visits reconnect with that token. If the code has
   expired, generate a fresh code and enter it in the code-only form.
3. The address stays fixed to the page's origin and nearby search is hidden.
   Open another machine's URL to use that machine. Resizing the page switches
   between the shared wide and compact layouts.

The browser stores machines and tokens in this origin's `localStorage` under
`tcode.hosts`, and the last machine under `tcode.last_host`. Clearing site data
means you must add the machine again. WebSockets follow the page's scheme:
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
| `47420/TCP` by default | HTTP listener: adding devices, WebSockets, and the optional browser app. Use your configured port if you change it. |
| `5353/UDP` on the LAN | Bonjour / mDNS search for `_tcode._tcp.local.` machines. Optional when you enter an address yourself. |
| Your dev server's TCP port | Direct access from another desktop's preview browser. Separate from the tcode listener. |

Connect the machine and your other devices to the same LAN or overlay, such as
Tailscale or EasyTier. Allow the listener through the machine's firewall and any
overlay access rules. tcode provides no relay or public discovery service. Treat
nearby-machine search as LAN-only: it does not cross a normal overlay connection.
Enter the machine's overlay address and port when it does not appear nearby.
Nearby-machine search advertises identity and address hints; it does not grant
access.

For desktop preview, tcode rewrites `localhost`, `127.0.0.1` and `0.0.0.0` in
HTTP(S) preview URLs to the hostname in the added machine's origin, preserving the
port, path, query and fragment. For example, `http://localhost:5173/app` becomes
`http://192.168.1.10:5173/app` for that machine. Configure the dev server to
listen on all interfaces (`0.0.0.0` or `::`) and allow its port from your LAN or
overlay. URL rewriting does not tunnel the connection or make a loopback-only
server reachable.

## Security

The LAN transport is plain HTTP and `ws://`. Anyone on the LAN who captures
traffic can read the device token and the work sent over the connection. Use a
tunnel or VPN on untrusted networks. tcode never provisions or manages TLS.

### Device tokens and storage

Adding a device issues a random bearer device token. The machine's `remote.json`
stores device names, IDs and token hashes, not raw tokens. Native apps store raw
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

The six-digit code and device-token authorization remain required through a
tunnel or VPN. Protect tunnel access and your stored tokens. tcode does not
encrypt stored JSON or project files; preview servers and provider connections
use their own transports.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Connection code is wrong or expired | Generate **New code** on the machine, or run `tcode-headless pair`. Codes expire after five minutes, after use, after five wrong attempts, or when replaced. Get a separate code for each device. |
| Cannot reach the machine | Check that tcode is running, that the address is reachable from this device, and that the TCP port is allowed. Use the overlay address if no nearby machine appears. This device's `127.0.0.1` points to itself. |
| `tcode-headless pair` cannot reach a running listener | Check its port and IPv4/IPv6 family. The listener must accept loopback connections; `pair` always uses loopback. |
| Browser shows 404 | Use a `tcode-headless` build with `web`. The desktop app and builds without the bundle do not serve the browser app. |
| Browser fails before adding the machine | Check the HTTP host or your HTTPS tunnel. Check that JavaScript and site storage are allowed. |
| Connection rejected after adding the machine | Check whether the device was removed. Add the machine again with a fresh code if access is intended. Keep the machine and device builds on a matching protocol version. |
| Preview cannot load a dev server | Make the dev server listen on all interfaces, open its own port and check the first address saved for the added machine. The tcode port does not carry the preview page connection. |

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
- Desktop preview needs LAN- or overlay-reachable dev servers. There is no TCP
  tunnel for preview pages. Preview rewriting uses the first saved machine
  address, which may differ from an address chosen by transport reconnection.
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
