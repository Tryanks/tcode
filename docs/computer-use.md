# Computer use

tcode gives MCP-capable providers (Claude Code, Codex, OpenCode, and ACP agents that
advertise `mcpCapabilities.http`) a set of desktop computer-use tools, served by the
in-process `tcode_computer_use` MCP server. pi has no MCP client, so its provider card
and model-picker rows identify computer use, preview, and orchestrate as unavailable
before a session starts. The design follows
[pi-computer-use](https://github.com/injaneity/pi-computer-use): accessibility-tree-first,
state-scoped observation, transactional actions — not blind pixel clicking.

## Tool surface

| Tool | Purpose |
| --- | --- |
| `find_roots` | Ranked list of desktop window roots (`@rN`) with app name, bundle id, pid, title. |
| `observe_ui` | Observe one root (or the frontmost window). Returns a folded accessibility outline with element refs (`@eN`), a `state_id`, and (per image mode) a screenshot. |
| `search_ui` | Ranked text/role search over the full cached outline of a `state_id`. |
| `expand_ui` | Local outline context around one ref, to a given depth. |
| `inspect_ui` | Full attributes, frame, and supported actions for one ref. |
| `act_ui` | Run a transaction of actions (`press`, `click`, `set_text`, `type_text`, `keypress`, `scroll`, `drag`, `move_mouse`) against a `state_id`, optionally with an `expect` postcondition; returns the successor state as a diff or full view. |
| `read_text` | Page through long text owned by a state ref. |
| `wait_for` | Wait for a text/role condition to become present or absent. |

Core contract, inherited from pi-computer-use:

- **State-scoped refs.** Every `@e` ref belongs to the `state_id` that produced it. Observations
  are immutable and stored in a bounded LRU (default 8). Acting from an evicted or stale state is
  rejected with a clear error; the model must observe again.
- **Progressive disclosure.** The first outline is folded; `search_ui` / `expand_ui` /
  `inspect_ui` query the full stored tree without touching the live UI.
- **Honest outcomes.** `act_ui` reports `worked` / `didnt` / `unknown` per step, stops at the
  first failure (`stopped_at`), and never treats event delivery alone as semantic success when an
  `expect` condition was given. Each step also reports `delivery` (`ax`, `background_pid`,
  `foreground_hid`, or `none`), while the transaction reports `activation` (`none`, `background`,
  or `foreground`). AX-only transactions therefore have `activation: "none"`.
- **Bounded output.** Model-visible text is capped; oversized results return a preview plus a
  continuation ref for `read_text`.

The sole remaining tool-surface deviation from pi-computer-use is the absence of CDP browser
roots: browser automation stays on the `tcode_preview` server and the embedded WebView. The
earlier Windows/UIAutomation gap is resolved by the Windows backend, and text-sparse
accessibility trees use raw-image pass-through. By maintainer decision the image fallback does
not run OCR or synthesize `pictureOnly` nodes; the model reads the attached pixels directly.

## Architecture

- `crates/computer-use-mcp` — the whole feature:
  - `outline.rs` — platform-neutral UI tree model, folding, search ranking.
  - `state.rs` — bounded immutable state store, `state_id` allocation, staleness checks.
  - `tools.rs` — rmcp `ToolRouter` (same streamable-HTTP + bearer-token shape as
    `preview-mcp` / `orchestrate-mcp`).
  - `backend/` — platform dispatch plus shared contracts. `backend/macos/` uses the AX C API
    (`AXUIElement*`), per-process CGEvent input synthesis, and `screencapture -l <windowid>`
    capture.
    `backend/windows/` is a thin adapter over the `uiautomation` crate for COM setup, Control View
    traversal, patterns, input, and GDI-backed screenshot capture. Other platforms get a stub
    backend whose tools return a clear "unsupported platform" error.
  - `permissions.rs` — TCC checks/requests (see below), public API also consumed by the
    settings UI.
- Registration: `SessionOptions.computer_use_server: Option<McpRegistration>` threaded exactly
  like `orchestrate_server` — Claude via `--mcp-config`, Codex via `-c mcp_servers.*`, OpenCode
  via its server config, and ACP via `session/new` `mcpServers` (HTTP-capability-gated).
  Enabled/disabled per
  `Settings.computer_use.enabled`.

Unlike pi-computer-use, tcode needs **no helper app**: tcode is itself a signed `.app`, so
Accessibility and Screen Recording grants attach directly to tcode. That removes helper
install/signing/attribution handling entirely.

## Text-sparse image fallback

An observed window of at least 20,000 square screen points is considered text-sparse when fewer
than three accessibility descendants expose a title, value, or description. The root window
title is excluded. A sparse observation includes `text_sparse: true` so the agent knows the
accessibility outline does not adequately describe the window.

Image mode controls the raw screenshot attachment through the same capture path as other
observations:

- `auto` attaches one window screenshot only when the sparse rule triggers and Screen Recording
  permission is available. Without permission, it returns the plain sparse tree and a warning.
- `always` attaches one screenshot to every observation; sparse observations still include the
  marker.
- `never` never captures or attaches an image; sparse observations still include the marker.

The window is captured at most once per observation. On macOS, `screencapture`'s PNG is decoded,
downscaled while preserving aspect ratio (long edge at most 1568 pixels and area at most about
629,145 pixels), and returned as JPEG at quality 80 with MIME `image/jpeg`. Windows capture stays
PNG and is labeled `image/png`. The fallback is intentionally OCR-free and does not add
`pictureOnly` or other synthesized nodes.

## macOS background input delivery

AX-first actions remain background-safe: `AXPress`, setting `AXValue`, and setting `AXFocused`
are attempted before synthesized input. Coordinate, pointer, scroll, drag, and keyboard fallbacks
use `CGEventPostToPid` instead of the global HID tap. Every event is stamped with the target pid,
window number, and private window-routing field. Mouse events additionally carry click state,
pressure, and both window-under-pointer fields. When SkyLight's optional
`CGEventSetWindowLocation` symbol is available, events also receive a window-local point computed
directly from the AX top-left screen coordinates. Missing private symbols are treated as an
optional capability, not a crash condition. Because these events are never posted globally, the
system cursor does not move.

When the target is not already frontmost, a `BackgroundActivation` guard installs one per-pid
event tap for the current app and one for the target on a dedicated CFRunLoop thread. While armed,
the current-app tap drops only focus-message event types 13, 19, and 20; other events and all
target-tap events pass through. The guard then sends the target window an AppKit-defined
application-activated event (subtype 1) and a PID-directed down/up click at the window center as a
readiness primer. On teardown it sends application-deactivated subtype 2 when the target is still
backgrounded, invalidates the taps, stops the run loop, joins its thread, and only then releases
the callback contexts. A nil NSEvent or unavailable private API is logged and skipped; inability
to establish safe focus suppression makes the action return `didnt` unless an eligible foreground
keyboard fallback is enabled.

`allow_foreground_fallback` defaults to `false`. When enabled, only `type_text` and `keypress` may
retry through the legacy activate/raise foreground HID path, and only after background setup or
delivery fails. Pointer action kinds never activate or raise an app. `show_agent_cursor` defaults
to `true` and is persisted/plumbed through the computer-use configuration as the seam for the
later overlay work; this part does not render an overlay.

## Chromium accessibility activation

Before walking a Chromium/Electron AX tree (bundle id contains `chrome`, `chromium`, or `electron`,
case-insensitively), the backend best-effort sets `AXManualAccessibility` and
`AXEnhancedUserInterface` on the application element. A pid whose earlier tree was text-sparse is
also activated on its next observation, covering branded Electron apps whose bundle id does not
advertise the runtime. The first activation per pid creates a persistent AX observer, attaches its
source to a dedicated process-lifetime run loop, and subscribes to focus, application visibility,
window create/move/resize, value/title/selection, and layout notifications. It prefers the optional
remote-check registration symbol and falls back to public `AXObserverAddNotification`, then waits
about 300 ms before the first walk so the renderer can publish its complete tree.

## Windows backend

The Windows backend uses `uiautomation` 0.25 rather than the earlier hand-rolled UIAutomation COM
attempt. The crate initializes COM in a multithreaded apartment and owns the UIA client, Control
View walker, pattern wrappers, and input/screenshot plumbing. The backend enumerates visible
top-level elements in the stable sibling order returned by the crate (the crate does not expose
Win32 z-order). It walks each root into the same platform-neutral `UiNode` tree used on macOS,
including the same role vocabulary, ref assignment, search, sparsity, and diff behavior.
`bundle_id` has no Windows equivalent: it contains the process executable filename (for example,
`notepad.exe`) when the process image can be queried, and is empty for protected or inaccessible
processes. `app_name` is the executable stem, with the UIA class name as a fallback.

Native patterns map onto the shared action contract as follows: Invoke drives `press` and
ref-targeted `click`; writable Value and numeric RangeValue drive `set_text`; Toggle,
ExpandCollapse, SelectionItem, and a non-empty LegacyIAccessible default action drive `press`;
Scroll drives targeted `scroll`. The tree also reports native `toggle`, `expand`, `collapse`,
`select`, `set_value`, and `scroll_to_visible` capabilities for inspection. Grid, Table, Text,
Transform, and Window patterns currently contribute their normal properties and children but
have no additional direct `act_ui` operation. Physical fallback and coordinate actions use the
crate's `Mouse` and `Keyboard` wrappers, including Unicode text entry and Windows virtual-key
chords.

Window screenshots use the crate's element screenshot API, which captures the element's bounding
rectangle through its GDI path and encodes it with the crate's PNG feature. Windows has no
macOS-style TCC gate, so the shared permission facade reports accessibility and capture as
available.

## macOS permissions

| Permission | Needed for | Check | Request |
| --- | --- | --- | --- |
| Accessibility | reading AX trees, posting CGEvents | `AXIsProcessTrusted` | `AXIsProcessTrustedWithOptions(prompt)` |
| Screen Recording | computer-use screenshots | `CGPreflightScreenCaptureAccess` | `CGRequestScreenCaptureAccess` |

Settings gains two pages:

- **Browser** — enable/disable the embedded preview browser, default home URL, and
  allow-JS-evaluate toggle. Its in-process WKWebView snapshot tool needs no TCC permission.
- **Computer Use** — master enable toggle, image mode (`auto` / `always` / `never`),
  allow-input-actions toggle (off = observe-only), and one permission row per TCC kind:
  live status, a primary action, and **Recheck**. The primary action starts as
  **Request Access** and fires only the native TCC request. If the permission is still missing,
  the next explicit action becomes **Open System Settings** and deep-links the matching
  `x-apple.systempreferences` pane. Returning to tcode also triggers a recheck.

The persisted computer-use block additionally accepts `allow_foreground_fallback` (default
`false`) and `show_agent_cursor` (default `true`). Both use serde defaults, so settings files from
before background delivery continue to load without migration.

### Restart continuity

macOS applies some grants (notably Screen Recording) only after the app restarts, and shows its
own "Quit & Reopen" dialog. tcode therefore preserves Screen Recording flows across a restart:

1. Before a Screen Recording request, tcode writes a temporary `relaunch.json` marker into the
   data dir: `{ reopen_settings: "computer_use", active_session: <id> }`. Accessibility does not
   need this marker. Returning without a grant clears it.
2. Session timelines are already continuously persisted (JSONL + resume cursors), so an
   externally-initiated quit loses nothing.
3. On startup, a present marker is consumed and validated against the current Screen Recording
   status. After a real grant, the previous active session is reopened, the Settings window is
   reopened on the recorded page, and permissions are rechecked automatically. A denied or stale
   marker is discarded without changing the launch route.
4. The Computer Use page also offers an explicit **Relaunch tcode** button (shown when a grant
   was detected as pending-restart) that writes the same marker and relaunches via
   `open -n <bundle>`.

## Dev & testing

- `tcode --cu-permissions` prints the permission status as JSON and exits.
- Because developing computer use on the dev machine would require the very permissions being
  developed (and granting them mid-development churns TCC state), end-to-end testing runs in a
  **tart VM**: build on the host, copy the binary in, drive the VM's screen/keyboard over VNC,
  grant permissions inside the VM, then inspect permission status via SSH.
- CI (macOS/Linux/Windows) builds the platform fallback paths and runs the platform-neutral unit
  tests:
  outline folding, search ranking, text-sparse fallback decisions and observation shape,
  state-store eviction and staleness, tool schemas, settings serde round-trips, and MCP
  registration wiring for all three provider paths.
