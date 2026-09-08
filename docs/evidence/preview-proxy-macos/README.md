# macOS attached preview proxy acceptance, 2026-09-08

Baseline: `d1b7569aa`, macOS 26.6.2 (25G83), lb-wry 0.53.3.
The desktop was paired through Machines to an isolated headless profile on
`127.0.0.1:47610`. A Python ThreadingHTTPServer bound `0.0.0.0:5181`, sent
HTTP response headers and a short HTML body, then held the connection open for
300 seconds. Its intentionally incomplete body keeps socket ownership visible;
blank/stalled rendering is not the pass/fail signal.

## Baseline evidence

Desktop PID 75953, headless PID 67543, HTTP server PID 67810.
Computer Use states S172–S195 cover pairing, opening a project draft and preview,
and entering the URLs. Accessibility initially exposed only window controls;
after osascript activation, coordinate clicks and AXPress drove the UI.

`lsof -nP -iTCP:5181` after `http://localhost:5181`:

```text
Python    67810 tryanks    3u IPv4 TCP *:5181 (LISTEN)
Python    67810 tryanks    6u IPv4 TCP 127.0.0.1:5181->127.0.0.1:55106 (ESTABLISHED)
com.apple 78548 tryanks    5u IPv4 TCP 127.0.0.1:55106->127.0.0.1:5181 (ESTABLISHED)
```

After `http://192.168.31.148:5181`:

```text
Python    67810 tryanks    7u IPv4 TCP 192.168.31.148:5181->192.168.31.148:55585 (ESTABLISHED)
com.apple 78548 tryanks    6u IPv4 TCP 192.168.31.148:55585->192.168.31.148:5181 (ESTABLISHED)
```

No `preview proxy` host log appeared for either destination. The headless PID
owned no upstream socket. WebKit's system log identified `parentPID=75953` and
`Connected Path: ... interface: lo0` for the LAN-IP load: this Mac routes its own
LAN IP locally. A repeat localhost navigation again used PID 78548, source port
57575; see [raw repeat output](before-localhost-lsof.txt),
[localhost screenshot](before-localhost.png), and [LAN screenshot](before-lan.png).

## Configuration experiments

A minimal Objective-C WKWebView harness isolated the public APIs from GPUI/wry.
All variants used the same host/probe. Credentials were read from the isolated
paired profile without printing them. No URL rewriting was used.

| Variant | Result |
| --- | --- |
| Proxy installed on nonpersistent store before assigning it to configuration and creating WebView | localhost direct, WebKit PID 86508 |
| Clear excluded domains, add empty match domain, disable failover, before creation | localhost direct, PID 86624 |
| Credentials before creation; explicit `localhost` match, cleared exclusions, failover false | localhost direct, PID 88477 |
| Credentials before creation; `*` match and same options | own LAN IP direct, PID 88540 |
| HTTPS localhost with wildcard/options | TLS failure at the plain HTTP server, no host proxy log |
| Fresh persistent store, direct property setter instead of KVC, credentials before creation | localhost direct, PID 92885 |
| Cleared exclusions, empty match, added excluded domain `<-loopback>`, failover false | localhost direct, PID 93041 |
| Public `http://example.com`, authenticated configuration before creation | host received authenticated CONNECT; configuration is applied to nonlocal destinations |

The public control produced these host logs (timestamps UTC):

```text
[2026-09-08T07:06:44Z WARN  tcode_remote::proxy] preview proxy authentication rejected from 127.0.0.1:56610
[2026-09-08T07:06:44Z INFO  tcode_remote::proxy] preview proxy CONNECT example.com:80 from 127.0.0.1:56611
[2026-09-08T07:06:44Z WARN  tcode_remote::proxy] preview proxy authentication rejected from 127.0.0.1:56613
[2026-09-08T07:06:44Z INFO  tcode_remote::proxy] preview proxy CONNECT example.com:80 from 127.0.0.1:56614
```

The initial unauthenticated request receives the proxy challenge; subsequent
requests authenticate. macOS's API uses CONNECT even for an HTTP destination,
so a successful macOS path need not emit `preview proxy HTTP` specifically.

The installed Apple SDK `Network/proxy_config.h` documents that failover is
already disabled by default; match/excluded APIs control domain suffixes. It
exposes no documented switch to force local-interface destinations through the
proxy. `WKWebsiteDataStore.h` recommends configuring before page loads.
[WebKit's setter](https://github.com/WebKit/WebKit/blob/main/Source/WebKit/UIProcess/API/Cocoa/WKWebsiteDataStore.mm)
serializes the configuration into the network process; wry uses the same private
store throughout construction. The early-installation probes rule out late
credential installation as a sufficient explanation of the local bypass.

## Containment decision

The observed failure is broader than a literal `localhost` URL. Refusing only
loopback top-level navigations leaves own-interface addresses, subresources and
names resolving to local interfaces able to connect directly. The containment
therefore rejects macOS attached WebView creation at the production builder
entry point, through the existing unavailable surface. Local macOS previews
remain enabled. Windows/Android routing is unchanged.

This is a fail-closed containment, not a working macOS remote proxy. It is broader
than the originally proposed loopback-only fallback. No claim is made that the
Apple behavior occurs on every macOS release; the containment avoids relying on
unverified routing until the backend can enforce the complete boundary.

The unchanged desktop client itself also passed the public control after its
post-creation `authenticate` call (Computer Use S196):

```text
[2026-09-08T07:12:41Z WARN  tcode_remote::proxy] preview proxy authentication rejected from 127.0.0.1:57751
[2026-09-08T07:12:41Z INFO  tcode_remote::proxy] preview proxy CONNECT example.com:80 from 127.0.0.1:57752
```

Thus the production proxy is neither wholly missing nor universally ignored
because of late authentication; the reproduced bypass is destination-dependent.

## Automated validation

Commands used `CARGO_TARGET_DIR=/Users/tryanks/RustroverProjects/tcode/target`.

- Baseline `cargo build --release -p tcode --locked`: passed (5m41s).
- Initial containment release build: passed (3m55s).
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed before
  and after rebase.
- `cargo test -p tcode-ui --locked attached_preview_fails_closed_before_webview_creation`:
  passed. This exercises the production builder without creating a native window,
  asserting remote rejection and local acceptance. It was not run against the
  unmodified baseline; the live baseline is the demonstrated regression failure.
- Initial full UI suite: 382 passed, one unrelated failure in
  `chat::tests::compact_composer_shows_context_usage` (meter right edge 337px,
  Send right edge 359px).
- After `git fetch origin unify-client-shell` and rebase onto `a3246e9e0`,
  `cargo test -p tcode-ui --locked`: all 385 unit tests and one i18n integration
  test passed. The upstream model-picker width change resolves the prior failure.

Cargo prints an existing future-incompatibility notice for `block v0.1.6`.
No Windows/Android live routing or mobile/Web cross-target checks were run.

## Final manual result and cleanup

The final rebased release build passed in 4m23s. Desktop PID 19628 was launched
with the isolated paired profile and the saved host ID. S207–S230 cover the final
preview, localhost/LAN navigations, settings-based theme changes, and the compact
panel. The URL remains unchanged, and the existing unavailable surface displays:

> The machine proxy could not be applied safely on macOS. WebKit bypasses it for
> local destinations, so the attached preview is unavailable.

For both `http://localhost:5181` and `http://192.168.31.148:5181`,
`lsof -nP -iTCP:5181` returned only:

```text
COMMAND   PID    USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
Python  67810 tryanks    3u  IPv4 0x4a9e0d70b9279e39      0t0  TCP *:5181 (LISTEN)
```

No new headless `preview proxy` line appeared. This is the expected rejection
result, not evidence of successful forwarding. There is no headless upstream
socket because the request is never issued. Raw evidence:
[localhost](after-localhost-lsof.txt), [LAN IP](after-lan-lsof.txt).

Final screenshots:

- [Localhost, light](after-localhost-light.png)
- [LAN IP, light](after-lan-light.png)
- [LAN IP, dark](after-lan-dark.png)
- [Compact panel, dark](after-lan-dark-narrow.png)
- [Compact panel, light](after-lan-light-narrow.png)

The error copy wraps without clipping at the normal 1200×800 window and compact
800×800 window. Both themes were inspected. The initial containment build showed
only the generic missing-WebView hint; the final change makes the existing
unavailable view render its recorded creation error instead.

All temporary native probes were terminated. The desktop was terminated, the
headless host received SIGINT, and the Python server was terminated. The
`/tmp/tcode-px-host`, `/tmp/tcode-px-client`, `/tmp/tcode-px-project`, and
`/tmp/tcode-px-evidence` directories were removed, along with the persistent
probe's `~/Library/WebKit/persistent` store. Final `lsof -nP -iTCP:5181
-iTCP:47610` returned no sockets. No push was performed.
