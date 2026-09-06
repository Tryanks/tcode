# Computer use

tcode gives MCP-capable providers (Claude Code, Codex, OpenCode, and ACP agents that
advertise `mcpCapabilities.http`) a set of desktop computer-use tools, served by the
in-process `tcode_computer_use` MCP server. pi has no MCP client, so its provider card
and model-picker rows identify computer use, preview, and orchestrate as unavailable
before a session starts. Tools observe an accessibility tree, then apply actions
against the returned state. Native backends are available on macOS and Windows;
other platforms return an unsupported-platform error.

The tool design was informed by
[pi-computer-use](https://github.com/injaneity/pi-computer-use).

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

Tool contract:

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

Browser automation belongs to the separate `tcode_preview` server and embedded
WebView; this server exposes desktop windows, not CDP browser roots.

## Implementation boundary

[crates/computer-use-mcp](../crates/computer-use-mcp/src/lib.rs) owns the outline,
immutable state store, tool router, platform backends and permission facade.
Providers receive its streamable-HTTP MCP registration when computer use is
enabled. Claude, Codex and OpenCode use their native MCP configuration; ACP
registration is gated on the agent's HTTP MCP capability. The Settings UI
consumes the same permission facade.

The server runs inside tcode, so macOS permissions apply to the running app;
there is no separately installed helper app.

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

The macOS backend tries AX actions before synthesizing input. Pointer actions
and keyboard fallbacks normally post events to the target process. A background
activation guard suppresses focus changes while delivering them; the system
cursor stays in place. The implementation uses optional private routing APIs,
so callers must handle failed or unknown outcomes rather than assuming delivery
means the application changed.

`allow_foreground_fallback` defaults to `false`. When enabled, only `type_text`
and `keypress` may retry through foreground activation and HID delivery after a
background failure. Pointer actions do not take that fallback. `show_agent_cursor`
defaults to `true` and controls a separate macOS action overlay; it is visible
only while the target app is frontmost and does not move the system cursor.

Before walking Chromium/Electron windows, the backend best-effort enables their
accessibility exposure. It also retries activation for processes whose earlier
trees were text-sparse, accommodating branded Electron apps. This may make a
later observation richer than the first; it does not guarantee every app exposes
a complete tree.

## Windows backend

The Windows backend uses the `uiautomation` crate. The crate initializes COM in a multithreaded apartment and owns the UIA client, Control
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

The relevant Settings pages are:

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
2. Session events and resume cursors are persisted continuously. The marker
   records a navigation destination; it is not a backup of in-flight provider work.
3. On startup, a present marker is consumed and validated against the current Screen Recording
   status. After a real grant, the previous active session is reopened, Settings is
   reopened on the recorded page, and permissions are rechecked automatically. A denied or stale
   marker is discarded without changing the launch route.
4. The Computer Use page also offers an explicit **Relaunch tcode** button (shown when a grant
   was detected as pending-restart) that writes the same marker and relaunches via
   `open -n <bundle>`.

## Validation

Use [CONTRIBUTING.md](../CONTRIBUTING.md) and CI for build and test commands.
The platform-neutral tests exercise outline/state handling and tool behavior;
compilation does not establish native permission or input-delivery behavior.
For backend changes, exercise observation, the affected action and its successor
state on the target platform. For macOS permission changes, also check denial,
return from System Settings, grant, restart continuity and revocation. Use an
isolated test environment when changing permission state would disrupt the
working desktop; keep evidence in the PR rather than a machine-specific run log.
