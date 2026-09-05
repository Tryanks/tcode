# Remote work mode

Use one tcode host from another desktop, a phone or a browser over your LAN or
overlay network. There is no relay service. For a shorter introduction, see
[Remote work mode in the README](../README.md#remote-work-mode).

## Concepts

| Term | Meaning |
| --- | --- |
| Host | The tcode process that runs providers and terminals and stores projects and threads. A desktop app or `tcode-headless` can host. |
| Client | A screen connected to the host. Your desktop, phone or browser sends actions to it. |
| Pairing | Exchanging a single-use, six-digit pairing code for a device token. A code expires after five minutes; five wrong attempts invalidate it. Generating a new code replaces the previous code. |
| Fingerprint | The SHA-256 digest of the host's TLS certificate. Native clients save it to recognize the same host on later connections. |
| Device | One pairing record on the host, with a name and a token that you can revoke. |

Pair each client separately. Project files, provider processes and terminal
processes stay on the host. Clients receive thread content and other data needed
to display and operate them.

## Desktop hosting

1. Open the desktop app on the computer where your projects and agent CLIs live.
   Configure the providers and add your projects there.
2. Open **Settings → Remote**. Set **Port** (default `47420`) and **Host name**.
   Use **Apply** to apply changes to an already running listener.
3. Enable **Host this computer**. The listener binds all IPv4 interfaces.
   Allow its TCP port through your firewall from the LAN or overlay you use.
4. Read the pairing code and fingerprint, or scan the QR code from your phone.
   Use **New code** if the code expires or you need to pair another device.
5. Check **Paired devices** after pairing. Use **Revoke** to withdraw a device's
   access. Keep the app and computer running while clients use the host.

Hosting is unavailable while that desktop window is connected to another host.
Choose **Back to local** to relaunch in local mode first. Desktop hosting serves
native clients; it does not embed the browser client.

## Headless host

### Install and start

1. Download a headless archive from
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

3. Install and authenticate your agent CLIs under the account that will run the
   host. Ensure that account's `PATH` includes them and that it can access your
   project directories.
4. Start the host with a persistent data directory:

   ```sh
   "$HOME/.local/bin/tcode-headless" serve \
     --listen 0.0.0.0:47420 \
     --name build-server \
     --data-dir "$HOME/.local/share/tcode-host"
   ```

   Startup prints a pairing code, its expiry, the full fingerprint, a
   `tcode://pair` link and a terminal QR code. Release builds also print browser
   HTTPS URLs. Use an address reachable from your client, not `0.0.0.0`.
5. Allow inbound TCP `47420` from your LAN or overlay. In another shell on the
   host, generate a new code when needed:

   ```sh
   "$HOME/.local/bin/tcode-headless" pair --listen 127.0.0.1:47420
   ```

   `pair` contacts the running host over loopback. Its `--listen` selects the
   port and IPv4/IPv6 family; it does not contact the supplied remote address.
   Keep the listener reachable on loopback, as with the default wildcard bind.
   A listener bound only to a specific LAN address cannot answer this command.
6. Connect a client using the instructions below. To prepare projects with the
   local desktop UI, stop headless first and open the desktop app with the same
   data directory. Do not run two local hosts against the same directory.

On Unix, Ctrl-C requests shutdown and a store flush. The CLI reference is:

```sh
tcode-headless --help
```

Add the installed directory to your `PATH` to use that short command. Help is a
top-level option; the `serve` and `pair` subcommands do not accept `--help`.

### Data directory

The headless `--data-dir` option takes precedence over `TCODE_DATA_DIR`. Without
the option, `TCODE_DATA_DIR` selects the store; otherwise tcode uses the platform
app-data directory with a `tcode` subdirectory. This includes settings, threads,
pairing records and the host's TLS identity. It does not move project working
directories into the store.

For example, this selects the same store as the explicit path above:

```sh
TCODE_DATA_DIR="$HOME/.local/share/tcode-host" \
  "$HOME/.local/bin/tcode-headless" serve --name build-server
```

Keep this directory across restarts and back it up with its certificate and key.
Use a separate directory for a separate host identity. The desktop app also
honors `TCODE_DATA_DIR`, including when pairing or connecting.

### Run with systemd

On a Linux system with systemd, create a user unit at
`~/.config/systemd/user/tcode-headless.service`. This example uses the executable
and data directory above. Extend `PATH` with the absolute directories containing
your agent CLIs; a service does not read your interactive shell setup.

```ini
[Unit]
Description=tcode headless host

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

`SIGINT` uses the host's shutdown-and-flush path. Stop any foreground host using
the same port or data directory, then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now tcode-headless.service
journalctl --user -u tcode-headless.service -f
```

Treat the journal as sensitive: startup includes the pairing code and link.
Configure user lingering through your system administrator if this service must
run after logout. Generate subsequent codes with the same `pair` command above.

### Build with the browser client

Release headless archives include the browser bundle. For a source build, run
these commands from the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.121 --locked
crates/web/build.sh
cargo build -p tcode-headless --release --features web
```

The script optionally uses `wasm-opt` if installed. Build the bundle before the
headless binary: the `web` Cargo feature embeds `index.html`, `tcode_web.js` and
`tcode_web_bg.wasm` from `crates/web/dist`. It is a build option, not a `serve`
flag. The resulting executable is `target/release/tcode-headless`.

The host serves the browser app at `/` over HTTPS, on the same port as pairing
and secure WebSockets. Without `web`, static requests return 404 while native
clients can still pair and connect. The browser app uses the compact phone UI.

## Connecting

### Another desktop

1. Open **Settings → Remote** on the client.
2. Under **Find hosts on this network**, choose **Search**, then **Use** beside
   a discovered host. This fills the address, port and fingerprint; you still
   need the pairing code. Alternatively, fill **Pair with a code** manually.
   You can also paste the host's pairing link into the address or code field.
3. Choose **Pair**. Compare the displayed fingerprint with the host's fingerprint
   through a trusted channel before choosing **Connect** under **Paired hosts**.
4. Let the app relaunch. It now uses that host's projects and threads. To switch,
   select another paired host and choose **Connect**; **Back to local** relaunches
   without a remote connection.

You can also pair from the desktop executable. Replace the sample address and
code with the host's current values:

```sh
tcode --pair 192.168.1.10 47420 123456
```

This saves the host in the client's `hosts.json` and prints its host ID. It uses
TOFU and does not print a fingerprint comparison prompt. Check the saved
fingerprint against the host before connecting. Replace `HOST_ID` below with
the printed ID:

```sh
tcode --connect HOST_ID
```

`--connect` accepts a saved host ID, not an address or WebSocket URL. Use the
same client data directory for pairing and connecting.

### Phone

1. Install the Android debug APK or re-sign and install the unsigned iOS IPA.
2. Open **Hosts → Pair a host**. Choose **Scan QR code** and scan the host's QR
   code, or enter **Address**, **Port** and **Pairing code**. Selecting an entry
   under **Nearby hosts** fills the address, port and fingerprint; enter the
   pairing code to continue. Allow camera or local-network access when needed.
3. Choose **Pair** for manual entry. Compare the fingerprint on the confirmation
   screen with the host, then choose **Connect to host**.
4. Open a thread from **Threads**, or use **New thread** and choose a project.
   Read replies, send or queue a message, steer a running turn, stop it, and
   answer approval requests from the chat screen.
5. Open **Settings** from the thread list to change appearance, language or
   **Device name**, or choose **Disconnect**. Return to **Hosts** to select
   another host. You connect to one host at a time.

### Browser

1. Open the headless host's URL, for example `https://192.168.1.10:47420/`.
   Use HTTPS; the host does not serve plaintext HTTP.
2. Inspect the certificate in the browser warning and compare its SHA-256
   fingerprint with the host through a trusted channel. Accept the self-signed
   certificate exception for this host. Acceptance is normally needed once in
   that browser profile, not a guarantee that warnings never recur.
3. Choose **Pair a host**, enter the pairing code and choose **Pair**. The page
   fixes the address and port to its own origin; you enter only the code.
4. Check the displayed fingerprint and choose **Connect to host**. Use the
   thread list and chat as on the phone. To use a different host, open that
   host's HTTPS URL.

The browser stores paired hosts and tokens in this origin's `localStorage`
under `tcode.hosts`, and the last host under `tcode.last_host`. Clearing site
data requires pairing again. The in-page fingerprint is a display value from
the host; JavaScript does not verify or pin the browser's TLS certificate.

## Networking

| Port | Purpose |
| --- | --- |
| `47420/TCP` by default | TLS listener: pairing, secure WebSockets, and the optional browser client. Use your configured port if you change it. |
| `5353/UDP` on the LAN | Bonjour / mDNS discovery of `_tcode._tcp.local.` hosts. Optional when you enter an address yourself. |
| Your dev server's TCP port | Direct access from a remote desktop's preview browser. Separate from the tcode listener. |

Connect the host and clients to the same LAN or overlay, such as Tailscale or
EasyTier. Allow the listener through the host firewall and any overlay access
rules. tcode provides no relay or public discovery service. Treat discovery as
LAN-only: it does not cross a normal overlay connection. Enter the host's overlay
address and port when it does not appear nearby. Discovery advertises identity
and address hints; it does not authorize pairing.

For desktop preview, tcode rewrites `localhost`, `127.0.0.1` and `0.0.0.0` in
HTTP(S) preview URLs to the paired host's first saved address, preserving the
port, path, query and fragment. For example, `http://localhost:5173/app` becomes
`http://192.168.1.10:5173/app` for that host. Configure the dev server to listen
on all interfaces (`0.0.0.0` or `::`) and allow its port from your LAN or overlay.
URL rewriting does not tunnel the connection or make a loopback-only server
reachable.

## Security

### Certificate lifecycle and comparison

The host creates a per-host self-signed certificate on first start. It stores
`remote-cert.der` and the PKCS#8 private key `remote-key.der` beside `remote.json`
and reuses them after restart. A partial, unreadable or corrupt identity causes
startup to fail instead of silently rotating it. Preserve both files when moving
the host. An intentionally replaced certificate requires clients to pair again.

The full fingerprint is SHA-256 over the entire DER certificate. Headless output,
pairing links and QR payloads carry the full value. UI cards display an abbreviated
value (eight groups of four hexadecimal digits); native clients pin all 32 digest
bytes. Compare the displayed groups with the same prefix on the host, or compare
the full saved fingerprint with headless output for a full check.

A native client pairing from a link or QR code checks the supplied fingerprint
before sending the pairing code. Obtain that link from the host through a trusted
channel. Nearby discovery is only a hint, even though it supplies a fingerprint.

Manual address and code entry without a supplied fingerprint uses TOFU: the
client accepts and saves the first certificate it reaches. The six-digit code
does not independently authenticate that certificate. An attacker on that first
connection could impersonate the host. Compare fingerprints through a trusted
channel before proceeding; the comparison screen appears after the pairing
exchange, so it does not protect a code already sent to the wrong endpoint.
Native clients reject a changed pin before sending a saved device token.
Legacy saved hosts with an empty fingerprint acquire and persist a pin on their
first connection, also using TOFU.

Browsers use their own TLS certificate trust and exceptions. The fingerprint
returned by the page is not a JavaScript certificate check. Compare the actual
certificate in browser certificate details when establishing trust.

### Device tokens and storage

Pairing issues a random bearer device token. The host's `remote.json` stores
device names, IDs and token hashes, not raw tokens. Native clients store raw
tokens and certificate fingerprints in `hosts.json` in their own data directory.
Phone records are in the app's private data directory; Android uses its
`filesDir`. Browser records use `localStorage` as described above.

On Unix, tcode writes `remote.json` and `hosts.json` with mode `0600` and keeps
the certificate and key at `0600`. These are filesystem permissions, not file
encryption. Protect the client data directory and browser profile: possession
of a device token grants that device's access. There are no per-device project
permissions or read-only device roles.

On a desktop host, use **Settings → Remote → Paired devices → Revoke**. New
connections using that token are refused; an existing connection is checked at
a server keepalive tick and closed when revocation is detected. Do not rely on
an immediate disconnect. **Remove** under **Paired hosts** only forgets the
client's record; it does not revoke the token on the host.

There is no headless revoke subcommand. To revoke without the desktop hosting
UI, stop the headless host, back up `remote.json`, remove the matching entry from
its `devices` array, preserve its permissions, and restart. Do not edit that file
while the host is running: it holds the device list in memory.

### What is encrypted

TLS encrypts pairing traffic, device tokens and the remote WebSocket traffic
between a client and the host, including thread events, terminal bytes and
attachment transfers. The embedded browser files are served over HTTPS too.
The host decrypts this traffic and runs the requested work.

tcode does not encrypt its stored JSON files or project files at rest. mDNS
advertisements are not encrypted. Preview pages connect directly to the dev
server: HTTP preview traffic is not protected by the tcode TLS connection.
Provider connections use each provider's own transport; remote mode does not
add encryption to them.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Pairing code is wrong or expired | Generate **New code** on the host, or run headless `pair`. Codes expire after five minutes, after use, after five wrong attempts, or when replaced. Get a separate code for each device. |
| Cannot reach the host | Check that it is running, that the address is reachable from this client, and that the TCP port is allowed. Use the overlay address if discovery finds nothing. A client's `127.0.0.1` points to itself. |
| Headless `pair` cannot reach a running listener | Check its port and IPv4/IPv6 family. The listener must accept loopback connections; `pair` always uses loopback. |
| Browser shows 404 | Use a headless build with `web`. Desktop hosting and headless builds without the bundle do not serve the browser app. |
| Browser fails before showing pairing | Use HTTPS and review the certificate exception. Check that JavaScript and site storage are allowed. |
| “The host’s certificate changed. Pair again.” | Stop and compare the host's current fingerprint through a trusted channel. Check for a replaced data directory or identity. Restore the original certificate/key from your backup, or pair again only after verifying the replacement. Native clients go **Offline** on a pin mismatch. |
| Connection rejected after successful pairing | Check whether the device was revoked. Pair again with a fresh code if access is intended. Keep host and client builds on a matching protocol version. |
| Preview cannot load a dev server | Make the dev server listen on all interfaces, open its own port and check the first address saved for the paired host. The tcode port does not carry the preview page connection. |

**Connecting…** means the client is establishing its view. **Reconnecting ·
attempt N** means it is retrying a lost connection. Retry delays grow from one
to thirty seconds. The phone and browser UI shows **Offline** after thirty
seconds disconnected, but transport retries continue. Bring the phone to the
foreground or make the browser page visible to retry promptly.

Phone and browser screens keep the available cached thread content for reading
and disable writes while disconnected; unvisited threads may have only cached
list information. After reconnecting, subscriptions resume and thread records
catch up. A certificate mismatch is different from a temporary network failure:
it stops the native connection until you resolve the identity change.

## Limits

- Each client selects its own thread independently. Navigating on your phone
  does not change the thread selected on another desktop. Actions still operate
  on the same host projects and threads; selection independence is not access
  isolation.
- Remote desktop terminal replay contains the last **256 KiB** of raw output
  per live terminal. It is not complete scrollback or a saved screen image;
  truncation can start inside a terminal escape sequence or UTF-8 character.
- Desktop preview needs LAN- or overlay-reachable dev servers. There is no TCP
  tunnel for preview pages. Preview rewriting uses the first saved host address,
  which may differ from an address chosen by transport reconnection.
- Phone and browser clients do not run providers locally. Their compact UI is
  for threads, chat, approvals and starting work in existing projects. It hides
  terminal, preview and diff panels, file pickers, attachment upload and voice
  input. Prepare projects and provider installations on the host. See the
  [mobile design](mobile-design.md) for the phone's scope.
- Mobile release artifacts are development builds: the Android arm64 APK is a
  debug build, and the iOS arm64 IPA is an unsigned debug build. Re-sign the IPA
  with your own signing identity and provisioning profile before device
  installation. For Android, enable USB debugging and authorize your computer,
  then replace `VERSION` with the downloaded release version:

  ```sh
  adb install -r tcode-VERSION-android-arm64-debug.apk
  ```
