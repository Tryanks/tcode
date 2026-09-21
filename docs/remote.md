# Use Tcode from other devices

Tcode runs on the machine that holds your projects, provider tools and terminal
processes. You can open that machine from another desktop, a phone, a tablet or
a browser. Native apps reach the machine over **Traverse**: one encrypted
connection per device, direct when the network allows it and through a relay
when it does not. The browser client is a direct-access page served by
`tcode-headless` on the same machine or LAN. For a shorter introduction, see
[Use Tcode from other devices in the README](../README.md#use-tcode-from-other-devices).
The plan behind this model is [#376](https://github.com/Tryanks/tcode/issues/376).

## Concepts

| Term | Meaning |
| --- | --- |
| Machine | The computer where Tcode runs providers and terminals and stores projects and threads. It is the desktop app with **Let other devices connect to this machine** on, or `tcode-headless serve`. A machine is identified by a key it generates once and keeps in `traverse.json` in its data directory; the public half is its **machine id** (64 hex characters). |
| Device | A desktop, phone or tablet running the Tcode app that opens a machine. A device also has its own key, kept in `device.json` in its data directory. The machine keeps an allow list of device ids; every connection is authenticated by that key, so a device that is not on the list is refused before any application data flows. |
| Invitation | A `tcode://pair?…` link, shown as a QR code and copyable as text. It carries the machine id, its name, where it is reachable right now and a random 16-byte secret. Scanning or pasting the link is the whole pairing: an invitation lasts five minutes, admits one device, is replaced by the next one and is invalidated after five wrong secrets. The machine enforces all of this; Traverse never sees an invitation. |
| Traverse | The relay and lookup service a machine publishes to so devices off its network can find and reach it. **Official** (the default) uses the relays and lookup service listed in the manifest bundled with Tcode; **Self-hosted** uses your own `tcode-traverse` instance; **Off** uses no service at all. Traverse sees only encrypted traffic; the machine authenticates devices itself. |
| Direct / Relay | How one live connection is carried. Direct means the two ends exchange UDP packets with each other; Relay means the packets go through a Traverse relay because no direct path was found. The connection banner and the machine's device list show which one is in use, and it can change while connected. |

Pair each device separately. Project files, provider processes and terminal
processes stay on the machine; devices receive the thread content and other
data they need to show and operate that work.

## Use the desktop app as the machine

1. Open the desktop app on the machine where your projects and agent CLIs live.
   Configure the providers and add your projects there.
2. Open **Settings → Other devices**. Set **Machine name** (shown to devices)
   and choose a **Traverse** mode: **Official Traverse**, **Self-hosted
   Traverse** with the base URL of your instance, or **Off**. Use **Apply** to
   apply changes while hosting is already on.
3. Turn on **Let other devices connect to this machine**. The desktop binds its
   Traverse endpoint to UDP port `47420` on all IPv4 and IPv6 interfaces (IPv4
   only where IPv6 is unavailable), so invitation addresses and firewall rules
   survive restarts. Allow that UDP port through the machine's firewall for
   direct LAN connections; a relayed connection needs only outbound access.
4. Under **Invitation**, scan the QR code with the phone, or use **Copy
   invitation link** and paste the link into Tcode on the other device. The QR
   is redrawn with the machine's current relay and addresses while the
   invitation is valid, so a link copied a minute later still works. Use **New
   invitation** when one expires or you need to add another device. **Machine
   ‹fingerprint›** copies the full machine id.
5. Check **Connected devices** after pairing. Each row shows the device's name
   and operating system (for example `Xiaomi 15 · Android 15`), when it first
   connected, and **Direct**, **Relay via ‹host›** or **Offline**; the list
   refreshes every two seconds. Use **Remove** to withdraw a device's access.

Hosting keeps running while this window is connected to another machine:
devices that opened this machine continue working, and **Let other devices
connect to this machine** stays available. The desktop app accepts native
devices only; it does not serve the browser app.

## Run Tcode without the desktop app

### Install and start

1. Download an archive from
   [Releases](https://github.com/Tryanks/tcode/releases). Names follow
   `tcode-headless-<version>-<platform>-<arch>`: Linux uses `.tar.gz`, macOS and
   Windows use `.zip`; platforms are `linux`, `macos`, `windows`, and
   architectures are `x64` or `arm64`. Windows contains `tcode-headless.exe`.
2. Extract the archive and install the executable. For Linux x64, replace
   `VERSION` with the release version without its leading `v`:

   ```sh
   tar -xzf tcode-headless-VERSION-linux-x64.tar.gz
   install -d "$HOME/.local/bin"
   install -m 755 tcode-headless "$HOME/.local/bin/tcode-headless"
   ```

3. Install and authenticate your agent CLIs under the account that will run
   Tcode. Ensure that account's `PATH` includes them and that it can access your
   project directories.
4. Start Tcode with a persistent data directory:

   ```sh
   "$HOME/.local/bin/tcode-headless" serve \
     --name build-server \
     --data-dir "$HOME/.local/share/tcode-host"
   ```

   Startup prints `Machine id: …`, then the invitation: its remaining
   lifetime, `Relay: …` (or `Relay: none (LAN only)` with Traverse off), the
   machine's current `Addresses:`, the `tcode://pair?…` link and its QR code.
   With Traverse on, `serve` waits up to five seconds for a relay before
   printing the invitation, so the link works off the LAN. It then prints
   **Set a password on first open** or **Password protected** and
   `Browser: http://127.0.0.1:47420/`, the browser page (see
   [From a browser](#from-a-browser)).

   `--traverse official` (the default), `--traverse off`, or
   `--traverse https://traverse.example` selects the Traverse mode; see
   [Traverse](#traverse). `--browser-listen ADDR:PORT` binds the browser page
   elsewhere (`--listen` is accepted as an alias). To preset the browser
   password, supply `serve --password PASSWORD` or set `TCODE_PASSWORD` in the
   service environment; the CLI option takes precedence. A preset updates the
   password on startup and keeps existing browser tokens.
5. The headless machine binds its Traverse endpoint to a UDP port the operating
   system picks; the invitation and each device's saved record carry the
   current port, and a device learns a changed port from Traverse or from the
   next connection. For direct LAN connections allow inbound UDP to the
   process; a relayed connection needs only outbound access.
6. To see the invitation again while it is still valid, run in another shell
   on the same machine:

   ```sh
   "$HOME/.local/bin/tcode-headless" pair --data-dir "$HOME/.local/share/tcode-host"
   ```

   `pair` reads `invitation.json`, which `serve` writes in the data directory
   at startup and removes on shutdown. It cannot mint a new invitation, and it
   does not know about invitations created later from a paired device: after
   five minutes, or once a device has used it, create one from a paired
   device's **Settings → Other devices → New invitation** or restart `serve`.
7. From a paired device (or the logged-in browser), **Settings → Other
   devices** shows this machine's invitation QR and link, creates a new
   invitation, lists connected devices with their path, and removes devices.
   **Allow other devices** controls whether the machine accepts pairings; it
   defaults to on and is saved in `traverse.json`. Turning it off discards the
   current invitation and does not disconnect already paired devices.
8. Open this machine from another device using the instructions below. To
   prepare projects with the local desktop UI, stop `tcode-headless` first and
   open the desktop app with the same data directory. Do not run two local Tcode
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

`TCODE_PASSWORD` also supplies the password for this command. Existing browser
tokens remain valid by default, so logged-in browsers stay logged in. Add
`--revoke-tokens` to invalidate every browser token, then restart the host.
Update any service preset too, or it will replace the password on the next
startup. Do not run this command against a data directory used by a running
process. The password and browser tokens are separate from native pairing:
`set-password` never changes the machine key or the device allow list.

### Data directory

The `tcode-headless --data-dir` option takes precedence over `TCODE_DATA_DIR`.
Without the option, `TCODE_DATA_DIR` selects the store; otherwise Tcode uses the
platform app-data directory with a `tcode` subdirectory. This includes settings,
threads, the machine key and allow list (`traverse.json`), browser login
records (`remote.json`), the cached Traverse manifest and the current
invitation. It does not move project working directories into the store.

For example, this selects the same store as the explicit path above:

```sh
TCODE_DATA_DIR="$HOME/.local/share/tcode-host" \
  "$HOME/.local/bin/tcode-headless" serve --name build-server
```

Keep this directory across restarts and back it up: the machine key is the
machine's identity, and a device stays paired with that key, not with an
address. Use a separate directory for a separate machine identity. The desktop
app also honors `TCODE_DATA_DIR`, including as a device when adding or
connecting to a machine.

### Run with systemd

On a Linux system with systemd, create a user unit at
`~/.config/systemd/user/tcode-headless.service`. This example uses the
executable and data directory above. Extend `PATH` with the absolute directories
containing your agent CLIs; a service does not read your interactive shell
setup.

```ini
[Unit]
Description=Tcode without the desktop app

[Service]
Type=simple
WorkingDirectory=%h
Environment=PATH=/usr/local/bin:/usr/bin:/bin
ExecStart=%h/.local/bin/tcode-headless serve --name build-server --data-dir %h/.local/share/tcode-host
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
KillMode=mixed
TimeoutStopSec=60

[Install]
WantedBy=default.target
```

`SIGINT` uses Tcode's shutdown-and-flush path. Stop any foreground Tcode process
using the same data directory, then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now tcode-headless.service
journalctl --user -u tcode-headless.service -f
```

Treat the journal as sensitive: startup prints the invitation link, and the
link is the secret. Configure user lingering through your system administrator
if this service must run after logout. Reprint the invitation with the same
`pair` command above while it is valid.

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
`auth.mjs`, `tcode_web.js` and `tcode_web_bg.wasm` from `crates/web/dist`. It
is a build option, not a `serve` flag. The resulting executable is
`target/release/tcode-headless`.

Without `web`, the browser listener answers static requests with 404 while
native devices still pair and connect over Traverse. The browser runs the same
shell as the desktop app; it lays itself out from the canvas size, so a wide
tab gets the desktop split and a narrow one gets the compact stack.

## Open a machine

### From another desktop

1. Open **Machines** on this device — the sidebar entry under the search field,
   or the opening screen when no machine has been added yet.
2. Under **Add a machine**, paste the machine's invitation link
   (`tcode://pair?…`). The form shows **Invitation from ‹name› · ‹fingerprint›**
   once the link is valid.
3. Choose **Add a machine**, then **Connect to ‹machine›**.
4. The same window immediately opens that machine's projects and threads.
   Choose another machine under **Your machines** to switch again;
   **This machine** restores the local workspace, and a machine row's
   **⋯ → Disconnect** leaves without removing it. Switching closes only this
   device's link, not Tcode on either machine. Opening **Machines** by itself
   does not change the connection.

You can also pair from the desktop executable. Replace the link with the
machine's current invitation:

```sh
tcode --pair 'tcode://pair?v=2&id=…&secret=…&name=…'
```

This saves the machine in this device's `hosts.json` and prints its machine
id. Replace `MACHINE_ID` below with the printed id:

```sh
tcode --connect MACHINE_ID
```

`--connect` accepts a saved machine id, not an address. Use the same device
data directory for `--pair` and `--connect`.

### From a phone or tablet

1. Install the Android debug APK or re-sign and install the unsigned iOS IPA.
2. The app opens on **Machines** — the same screen every device has. Choose
   **Scan a QR code** and point the camera at the invitation on the machine, or
   **Paste an invitation link**. Allow camera access when the system asks.
   Native QR scanning uses AVFoundation on iOS and CameraX/ML Kit on Android;
   a simulator's permission and cancel flows do not prove that real camera
   recognition works.
3. Choose **Add a machine**, then **Connect to ‹machine›**.
4. Open a thread from the list, or use **+** to start one. Read replies, send or
   queue a message, steer a running turn, stop it, and answer approvals — the
   same views the desktop shows, laid out for the width.
5. **Settings** is the full settings page, not a reduced copy. **Back** returns
   list → machines, which disconnects; leaving a thread keeps the connection.
   You connect to one machine at a time.

A tablet wide enough for the split gets the split, and rotating it back to
portrait returns to the stack with the same thread and draft. A desktop window
never enters the stack, however narrow it is.

### From a browser

The browser client is for direct access: a tab on the machine itself, or on
its LAN when you bind the listener there. It does not use Traverse and has no
relay; to reach a machine from elsewhere, use the native app.

1. Open the `Browser:` link printed by `tcode-headless`. By default the page is
   served on `http://127.0.0.1:47420/`, reachable only from the machine. To
   use it from another device on the LAN, set a password first, then start
   with an explicit bind such as `--browser-listen 0.0.0.0:47420` and open
   `http://<machine LAN address>:47420/`.
2. On first open, set a password with at least eight characters and confirm it.
   Later visits without a saved token show **Log in**. A successful login saves
   a browser token and opens Threads; a valid stored token skips the form.
3. A rejected or revoked token returns to the login form. Five wrong passwords
   lock browser login for five minutes. Native pairing is unaffected by this
   lockout.
4. **Settings → Other devices** manages this machine's native pairing: **Allow
   other devices**, the current invitation QR and link, **New invitation**, and
   removing paired devices. Phones and other desktops always pair with an
   invitation, never with the browser password.
5. The address is fixed to the page's origin. Open another machine's URL to use
   that machine. Resizing the page switches between the shared wide and compact
   layouts.

The browser stores machines and tokens in this origin's `localStorage` under
`tcode.hosts`, the last machine under `tcode.last_host`, its device id under
`tcode.device_id` and pending writes under `tcode.outbox.<machine id>`.
Clearing site data means you must log in with the password again. The page is
plain HTTP and its WebSocket is `ws://`; anyone who can capture that LAN
traffic can read the password, the token and the work. If you put a TLS
proxy or tunnel in front of it, that is your own setup: the WebSocket follows
the page's scheme, so `https://` pages use `wss://`.

### Device preferences

Appearance, language and device name belong to this device, not the machine:
an explicit choice on this device overrides the connected machine's replicated
setting, and restoring that row reveals the machine's setting again. Native apps
keep these preferences in `mobile.json` next to `hosts.json` in their own data
directory; the browser keeps them in `localStorage`. iOS seeds the device name
from the device name, Android from the device model; the name and operating
system are sent to the machine when pairing and connecting, and the machine's
device list follows them.

## Traverse

A machine has one Traverse setting: **Official**, **Self-hosted** (a base
URL) or **Off**. On the desktop it is in **Settings → Other devices →
Traverse**; headless uses `serve --traverse official|off|<url>`. Devices need
no Traverse setting: the invitation carries the machine's Traverse URL (absent
for the official service), and each saved machine keeps its own, so one phone
can use an official-Traverse machine and a self-hosted one at the same time.

A Traverse instance describes itself with a JSON manifest: a list of relays
(`url`, optional `quic_port`, `region`, `home_rtt_max_ms`), a list of pkarr
lookup URLs and, optionally, DNS lookup origins. Relay and pkarr URLs must be
`https`. The machine builds its relay list from the manifest and publishes a
signed record of where it can be reached to every pkarr URL in it; a device
resolves the machine id through the same pkarr URLs. A refreshed manifest is
applied to the running endpoint: relays that disappeared are removed, new ones
added, lookup services rebuilt.

| Mode | Machine | Devices | The service sees |
| --- | --- | --- | --- |
| **Official** (default) | Uses the manifest bundled with Tcode, refreshed from the repository. At the time of writing it lists n0's public relays (`*.relay.n0.iroh.link`, regions `na-east`, `na-west`, `eu`, `ap`, QUIC port 7842) and n0's lookup service (`https://dns.iroh.link/pkarr`, DNS origin `dns.iroh.link.`). These are n0's infrastructure: n0 states that the public relays are rate-limited and offer no uptime guarantee, and that the lookup service is fine for production when its performance is acceptable. | A device's own endpoint also uses the bundled relay list for its home relay and the bundled lookup service to resolve machines. There is no device-side switch. | Relays see machine and device ids, the encrypted connection and its volume. The lookup service stores, per machine id, the machine's signed record: its relay URL only, republished every five minutes; direct addresses are filtered out before publication and are exchanged over the encrypted connection instead. Anyone who knows a machine id can read that record. Devices publish nothing. |
| **Self-hosted** | Fetches `<base>/relays.json` from your instance and uses its relays and pkarr store. With nothing cached yet, hosting waits for one fetch and fails to start if the instance is unreachable; it never falls back to the official service. | A device takes the base URL from the invitation, fetches the same manifest, and adds that instance's lookup to what it already has. Its own home relay still comes from the bundled official list. | Your instance sees what the official one would. The official relays still see the device's end of the connection; the official lookup service is not used for this machine. |
| **Off** | No relay and no lookup: the endpoint publishes nothing and dials nothing but direct addresses. The invitation carries only the machine's current addresses (`Relay: none (LAN only)`). | The device dials the addresses from the invitation and the ones it learned on later connections. Its own endpoint still has the official relay list for its side. | Nothing about this machine. |

A device that connected successfully saves how it reached the machine —
the direct addresses that worked, newest first, up to 16, and the relay —
ahead of what the invitation said, so the next launch starts from what worked
last. With Traverse off, a machine whose address changes cannot be found again
until you pair with a new invitation.

### Offline, unreachable or rate-limited

- **The manifest cannot be fetched.** The official mode keeps the cached copy
  (`traverse-manifest-official.json` in the data directory) or the bundled one;
  a fetched manifest stays fresh for one hour, and after a failure the next
  attempt waits five minutes, so an offline machine does not pay a network
  timeout on every check. A bundled manifest newer than the cache wins. A
  self-hosted machine keeps its cached copy; with no cache, see the table.
- **No route to a relay.** The machine still hosts; the invitation carries
  direct addresses only, and devices on the same network connect directly.
- **A relay is rate-limited or refuses a connection.** Connections fall back
  to whatever path remains: a direct path if one exists, another relay from
  the manifest otherwise. Tcode does not show a separate error for this; the
  connection banner shows **Reconnecting** with the failure reason until a path
  works. A self-hosted instance is the way out of another operator's limits.
- **The lookup record is stale or missing.** A device keeps dialling the relay
  and addresses it saved from the last connection, so a machine that has not
  moved is still reachable without the lookup.

### Self-hosting Traverse

`tcode-traverse` is one binary that is a complete Traverse instance: an iroh
relay with QUIC address discovery on UDP 7842, a pkarr store for machine
lookup, and the `relays.json` manifest a machine reads. It is the same server
for any operator. Releases publish it as
`tcode-traverse-<version>-<platform>-<arch>` archives next to `tcode-headless`.
Configuration, TLS modes (Let's Encrypt, manual, reloading, or plain HTTP
behind your own proxy), the pkarr store limits, metrics, the optional region
lock and a Docker build are in the
[server README](../crates/traverse-server/README.md).

Point a machine at it with **Self-hosted Traverse** and the base URL on the
desktop, or `tcode-headless serve --traverse https://traverse.example.org`.
Nothing is configured on devices; they take the URL from the invitation.

## Remote Preview routing

Windows and Android embedded previews use the connected machine's network for
all HTTP and HTTPS traffic. `http://localhost:5173/app` opens the dev server on
the machine, even when it listens only on loopback. The URL stays unchanged in
the address bar, redirects, scripts and `preview_status`. No dev-server port
needs to be exposed. Open-in-system-browser still opens on this device and
cannot use the embedded preview's routing.

Preview traffic rides the attachment's own Traverse connection: each browser
connection becomes one tunnel that names its `host:port`, and the machine
dials that address the way any local program would. There is no separate proxy
port, no proxy credential and nothing to configure in hosting settings; the
paired, end-to-end encrypted connection is the authority. Plain HTTP is
forwarded; HTTPS stays an opaque tunnel with certificate validation performed
by the client webview, without TLS interception. The machine connects directly
to destinations; OS routing and global TUNs apply. Tcode does not read
upstream-proxy environment variables or system proxy settings.

One attachment holds at most 56 tunnels at once. Dialling a destination times
out after 10 seconds, and a tunnel idle for 60 seconds is closed. Revoking a
device or stopping hosting closes its connection and every tunnel on it; while
the attachment is reconnecting, opening a tunnel fails at once, and after it
reconnects new tunnels use the new connection. Connection logs contain the
destination host and port, never request bodies. For headless diagnostics use
`RUST_LOG=tcode_traverse=info`.

Android requires WebView's `PROXY_OVERRIDE` capability. It removes implicit
localhost bypasses, waits for the override before navigation, and clears it when
the attachment's views are destroyed. The override is process-wide, so embedded
preview views belong to one attachment.

macOS attached Preview uses per-port forwarding for HTTP(S) loopback URLs:
`localhost`, IPv4 loopback addresses and `::1`. The requested host and port are
sent unchanged in the tunnel's opening line. The viewing Mac allocates its own
loopback port, so a viewer service at the remote port cannot collide with it.
`localhost` reserves both IPv4 and IPv6 listeners at that port. Numeric loopback
URLs need that same address to be bindable on the viewing Mac; unassigned
addresses such as `127.0.0.2` can report an allocation error. Tcode does not
add loopback aliases to the OS. No OS DNS, hosts file or proxy settings are
changed. Public/LAN destinations are **direct from the viewer**, as the Preview
note explains; this is a remote dev server workflow, not arbitrary remote
browsing.

Each browser slot owns its forwarding table and a separate nonpersistent WebKit
website store. Relative assets and WebSockets derived from the page's actual
location use the same forwarded port. Top-level loopback links/redirects pass
through the same mapper, including redirects to another port. The native request
is retained when changing its URL; HTTP bodies, site authorization, WebSocket
frames and TLS records are not interpreted by the forwarding transport. Closing
the preview or changing attachment drops listeners and accepted tunnels; merely
hiding a retained preview keeps them alive. A new attachment receives new routes
and a fresh store.

The address bar, copy and stored URL retain logical remote intent. Native
back/forward/reload reuse the live mappings. `preview_status.url` and
`preview_wait_for` use the logical remote URL; `actual_url` is added when it
differs. `preview_evaluate`, page scripts and `window.location` see the
**actual local URL**. Open externally uses that live mapped URL and stops
working when the owning slot closes. Without a suitable live route, it reports
that Preview must open the page first, rather than opening the viewer's
unrelated original-port service.

Mapping preserves the requested hostname but changes its port. The HTTP Host
header and browser Origin therefore contain the viewing port. HTTPS retains the
hostname/SNI and normal WebKit certificate validation; a development certificate
must already be trusted and cover that hostname. Port-sensitive host/origin
allowlists, CORS, CSP and OAuth callbacks can need app configuration. Cookies are
not isolated by port, which is why attached slots use separate website stores.
There is no blanket certificate bypass or credential injection into site auth.

Absolute subresource URLs, hardcoded API/HMR ports and worker-owned endpoints are
not automatically rewritten. Configure them to derive their endpoint from the
actual page location or an explicitly resolved live forward; entering an
additional page URL creates its mapping but does not rewrite application code.
Navigation, transport and authentication failures use Preview's existing error
surface and automation errors. Local macOS Preview retains ordinary direct
networking and its existing store.

Windows uses WebView2's proxy configuration with implicit loopback bypass
disabled; Android uses the WebView proxy override. Both point the engine at
an attachment-owned loopback HTTP proxy that speaks `CONNECT host:port` and
absolute-form requests. The proxy opens one tunnel per browser connection,
rewrites an absolute-form request line to origin form, drops its own
hop-by-hop headers and copies everything else verbatim, WebSocket upgrades
included. It serves one request per connection and answers ambiguous
framing (chunked plus `Content-Length`, unknown transfer codings, origin-form
requests) with `400`. Unsupported proxy facilities fail closed; there is no
direct-network fallback. Closing the attachment cancels the listener and
active connections. macOS keeps its per-browser URL mapping over the same
tunnels.

When the main connection reconnects and its saved addresses change, existing
Preview browsers follow the new connection: their local proxy/forwarding ports,
URLs, history and website stores survive, connections made over the old
connection end, and new ones use the replacement. Tcode does not replay
interrupted requests or reload pages automatically: use the existing Reload
action if a page was interrupted. Another machine cannot retarget an existing
attachment's Preview.

The iOS embedded preview remains unsupported. The browser client cannot
override its browser's proxy.

## Security

### What is encrypted, and what a relay sees

Every native connection is a QUIC connection between the device key and the
machine key, encrypted end to end. A relay forwards packets between two ids;
it sees the ids, timing and volume, not the content. A direct connection on
your LAN is encrypted the same way. Traverse is never part of authentication:
the machine accepts a connection only from a device id on its allow list, and
answers anything else with `unpaired` and closes it.

The lookup service holds a public, signed record per machine: the machine's
relay URL, never its IP addresses. It tells anyone who knows the machine id
which relay reaches it; it does not let them connect, because the machine
refuses unpaired devices.

The browser client is different: it is plain HTTP on the machine's own
listener, protected by the password and a bearer token. Keep it on loopback or
a trusted LAN.

### Keys and files

The machine's `traverse.json` holds its secret key, its name, the allow list
(device id, name, operating system, first-connection time) and the **Allow
other devices** switch. A machine upgraded from the previous transport keeps
the identity seed found in its old `remote.json`, so its id does not change;
devices must still pair again. `remote.json` holds the browser side: the
password hash (randomly salted PBKDF2-HMAC-SHA256, 600,000 iterations), one
record per logged-in browser with its token hash, and that legacy seed. The
password and the raw tokens are never stored.

A device's `device.json` holds its secret key, name and platform. `hosts.json`
holds the machines it paired with: id, name, Traverse URL, relay, direct
addresses and last connection time — routing hints, not credentials. Pending
writes are in `outbox-<hex of machine id>.json`. Phone records are in the
app's private data directory; Android uses its `filesDir`.

On Unix, Tcode writes these files with mode `0600`. That is a filesystem
permission, not encryption. **A stolen `device.json` is that device's
access:** whoever holds the key can connect to every machine that lists the
device until the device is removed on each machine. **A stolen
`traverse.json` is the machine's identity:** its holder can run a machine that
paired devices would trust. There are no per-device project permissions or
read-only device roles; a paired device has the same access as the machine's
own window, including Preview tunnels to the machine's loopback and network.

### Revocation

On the machine, use **Settings → Other devices → Connected devices → Remove**
(from the desktop, a paired device or the logged-in browser). The device is
taken off the allow list and saved, its live connections are closed at once,
and a new connection from it is refused from that moment. The device shows
**Access rejected · Pair again** and stops retrying. Pairing it again needs a
new invitation.

**Remove** under **Your machines** on a device only forgets that device's
record; it does not withdraw anything on the machine.

There is no `tcode-headless` remove subcommand. To remove a device without a
paired client or the browser, stop `tcode-headless`, back up `traverse.json`,
remove the matching entry from its `devices` array, preserve its permissions,
and restart. Do not edit that file while Tcode is running: it holds the allow
list in memory and writes it back.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| **That is not a Tcode invitation link** | The pasted text is not a complete `tcode://pair?v=2&…` link with a valid machine id and secret. Copy the link again from the machine or scan the QR. |
| **The machine rejected this invitation** | The invitation expired (five minutes), was already used, was replaced by a newer one, or five wrong secrets invalidated it. Create **New invitation** on the machine or restart `serve`; get a separate invitation for each device. |
| **This machine is not accepting new device pairings** | Turn on **Allow other devices** on the machine (desktop **Settings → Other devices**, or from a paired device or the logged-in browser). |
| **Could not reach ‹machine›** while pairing | The device reached none of the invitation's addresses or its relay within 20 seconds. Check that Tcode is running, that the two are on the same network or the machine has a relay (`Relay:` in the `serve` output), and that the machine's UDP port is not blocked on the LAN. With Traverse off, only the printed addresses work. |
| **Reconnecting to ‹machine›… (attempt N)** stays up | The device keeps trying the saved relay and addresses with growing delays. If the machine moved network with Traverse off, pair again with a new invitation. Direct paths need UDP between the two ends; a relayed connection reaches the relay over HTTPS (TCP), so it still works where UDP is blocked, as long as the machine publishes to a relay. |
| **Access rejected · Pair again** | The machine no longer lists this device (removed, or the machine's data directory was replaced). Pair again with a new invitation if access is intended. |
| **Protocol mismatch · Update the app** | The machine and the device run different protocol versions. Update both. |
| **Connected … · Relay** when both are on the same LAN | Hole punching has not found a direct path yet, or the LAN blocks UDP between the two. The path can switch to **Direct** while connected; a fixed desktop UDP port (`47420`) that is allowed through the firewall helps. |
| `tcode-headless pair` says there is no invitation or it has expired | `pair` only reprints what a running `serve` wrote. Restart `serve`, or create a new invitation from a paired device's **Settings → Other devices**. |
| `could not start Traverse` on a self-hosted URL | The instance's `relays.json` could not be fetched and nothing is cached. Check the URL, the instance and its certificate; Tcode does not fall back to the official service. |
| Browser password is wrong or forgotten | After five wrong attempts, wait five minutes. To reset it, stop the host and use `set-password`; add `--revoke-tokens` if logged-in browsers should lose access. |
| Browser page unreachable from another device | The listener binds `127.0.0.1:47420` by default. Start `serve --browser-listen 0.0.0.0:47420` (with a password set) and open the machine's LAN address. |
| Browser shows 404 | Use a `tcode-headless` build with `web`. The desktop app and builds without the bundle do not serve the browser app. |
| Preview cannot load a dev server | Check that the dev server runs on the machine, hosting is on, the device is still paired, and its WebView supports proxy routing. Use `localhost` for a machine-loopback server. If a request was interrupted by a network move, wait for the main connection to recover and use Reload. |

**Syncing… / 同步中** means the workspace is waiting for its baseline: applied
Index and Settings snapshots, plus SessionStatus and SessionEvents for the
selected thread. A newly attached workspace shows neutral loading rows until
these arrive; it never labels an unknown list as empty. Cached lists and thread
content remain visible during reconnect, with Syncing until replay completes.
**Reconnecting · attempt N** appears immediately when a connection is lost,
with its failure reason. Retry delays grow exponentially from one to thirty
seconds with ±20% jitter (and a final 30 s cap). Only a connection stable for
at least 30 s resets backoff; a quick handshake followed by another disconnect
does not. A network change reported by the device ends the current wait
early and tries again at once.

A connection attempt has 20 seconds to find a path and complete the handshake.
Once connected, both ends send QUIC keep-alives every 10 seconds and drop a
connection with no packets for 30 seconds. On top of that the device sends an
application ping after 10 seconds without a message from the machine and
reconnects if nothing arrives within another 20 seconds. Browsers send the
application ping after 15 seconds of silence with the same 20-second reply
budget; browser timers may be delayed in background tabs, and making the page
visible or coming online wakes reconnection.

Native foreground cancels pending backoff. After at least 10 s in the background
the app reconnects outright and replays subscriptions from applied cursors;
after a shorter absence it probes once with the application ping. Foreground
and Active from the same return do not issue duplicate probes. On Android, the
system's default-network callback (a change of transport or validation, not a
signal-strength update) and every resume, and on iOS every return to the
foreground, additionally tell the device endpoint to rebind its paths and probe
every live connection at once, so a stale link does not wait for the idle
timer. The desktop relies on the timers alone.

Outgoing commands and queries are bounded to 256 lines and 8 MiB per attachment,
including lines held for retry. A full queue rejects the new line with
`queue_full` (QueueFull) and logs the error; older lines are retained. Subscription
updates coalesce by topic outside those slots. The store exposes the pending
count through `queued_outgoing()`.

A disconnected device keeps cached thread content for reading; unvisited threads
may have only cached list information. Queries need a live connection, while
retained writes follow the [outbox policy](#weak-networks) and remain pending
until acknowledged. Subscriptions resume after reconnecting. Temporary network
failures keep retrying. A machine that refuses the device stops at **Offline**
with **Pair again**; protocol mismatches stop with **Update the app**.

## Limits

- Each device selects its own thread independently. Navigating on your phone
  does not change the thread selected on another desktop. Actions still operate
  on the same machine's projects and threads; selection independence is not
  access isolation.
- A device's terminal scrollback is the machine's retained ring, capped at
  **1000 rows**. Older output is gone from the machine too, so no device can
  scroll further back than that. During a large output burst the visible screen
  keeps updating, while scrollback catches up after scrolling has stopped for
  100 ms. This avoids repeatedly sending the whole retained ring between PTY reads.
- Android has an embedded WebView preview with history, JavaScript automation,
  navigation errors and visible-page PNG capture. iOS and the browser client
  retain URL/open/copy actions without an embedded preview backend.
- Phones, tablets and browsers do not run providers, terminals or project files
  locally; the machine does. Every product view is present on them — threads,
  chat, approvals, the terminal, diff, plan, preview, search, import and export —
  and what differs is which *operations* the device can perform. A native file
  dialog, an embedded preview browser, macOS permission grants, dictation and
  "open in editor" need a capability the device either has or does not; where it
  does not, the view says which machine can do it and offers what it can (open
  externally, copy, type the value). Prepare projects and provider installations
  on the machine.
- One machine holds at most 64 open streams per device connection, of which
  56 may be Preview tunnels; a device that opens more is refused with **too
  many tunnels** until one closes.
- The Android arm64 APK is a release build signed with Gradle's debug key, so
  the key may change between releases; uninstall the previous build if an
  install is rejected. iOS is not published yet; build it from source with
  `crates/ios/host/build.sh`. For Android, enable USB debugging and authorize
  your computer, then replace `VERSION` with the downloaded release version:

  ```sh
  adb install -r tcode-VERSION-android-arm64.apk
  ```

## History replay limits

Opening a conversation subscribes to its latest 200 event records. A session
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
relaunch. Native clients atomically replace an `outbox-<encoded machine id>.json`
file beside `hosts.json`, with private file permissions; browsers use the
origin's `tcode.outbox.<machine id>` localStorage entry. Clearing client data
clears these pending writes. An admission storage error is a real failure, not a
successful send.

After hello and subscription replay, the oldest write is sent first; the next
waits for its Ack. The same key is used on every redelivery. The outbox is bounded
at 256 entries and 8 MiB of serialized entries. The oldest surplus entry fails
with `outbox_full`. A write already received by the host cannot be recalled by
client-side eviction.

The runtime retains the last 512 completed command keys and Ack results per
authenticated device, including rejected commands. Cache hits refresh their LRU
position and return the original Ack without executing again. This cache survives
a reconnect, but not a host-process restart or eviction from the 512-key
window. Exactly-once redelivery applies within that window. Keys are scoped by
the authenticated device id on the machine, never by a prefix the client
supplies, so one device's cache cannot be shared with or spoofed by another.

If a connected retained command has no Ack for 30 seconds, the client logs a
stalled acknowledgement and forces reconnect. The pending command remains in the
outbox. The request deadlines and liveness constants live together in
`crates/client/src/heartbeat.rs`: native idle probe at 10 seconds, browser idle
probe at 15 seconds, a 20-second liveness reply budget, a 15-second query
timeout and the 30-second command timeout.

A pending user message is muted with **Sending…**, or **Waiting for connection…**
when the workspace is reconnecting or syncing. It becomes normal only after Ack.
A rejected send or outbox overflow shows an inline error with **Retry** and
**Discard**. Retrying a terminal failure creates a new action and key. An approval
decision in the outbox disables the approval buttons and keeps a pending caption.
Transient reconnects belong in the connection banner, not failure toasts.

### Finding the machine again

A pairing identifies the machine by its key, independently of its address.
After every successful connection the device saves the relay and the direct
addresses that actually carried it, so the next attempt starts from what
worked last, and the invitation's hints come after. Between attempts the
lookup service (when the machine publishes to one) supplies the machine's
current relay and addresses; a home relay lets the two ends learn each other's
direct addresses and try hole punching, and the connection can move from
**Relay** to **Direct** — or back — without reconnecting. The path shown in
the banner and the machine's device list follows that change.

With Traverse off there is no lookup and no relay: the device has only the
addresses it saved, and a machine whose address changed needs a new
invitation. On the same network, iroh's own discovery of direct addresses is
limited to what the two ends can observe about each other; Tcode does no LAN
broadcast or subnet scanning of its own.
