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
  `expect` condition was given.
- **Bounded output.** Model-visible text is capped; oversized results return a preview plus a
  continuation ref for `read_text`.

Deliberate deviations from pi-computer-use (documented so later work can close them): no
Windows/UIAutomation backend, no CDP browser roots (browser automation stays on the
`tcode_preview` server and the embedded WebView), no separate helper app (see below), and a
simplified successor-diff heuristic. The earlier v1 OCR deviation is closed on macOS: sparse
desktop observations now gain Vision-derived `pictureOnly` nodes.

## Architecture

- `crates/computer-use-mcp` — the whole feature:
  - `outline.rs` — platform-neutral UI tree model, folding, search ranking.
  - `state.rs` — bounded immutable state store, `state_id` allocation, staleness checks.
  - `tools.rs` — rmcp `ToolRouter` (same streamable-HTTP + bearer-token shape as
    `preview-mcp` / `orchestrate-mcp`).
  - `backend/` — `Backend` trait; `backend/macos/` implements it with the AX C API
    (`AXUIElement*`), CGEvent input synthesis, `screencapture -l <windowid>` capture, and
    Vision (`VNRecognizeTextRequest`) fallback OCR;
    other platforms get a stub backend whose tools return a clear "unsupported platform" error.
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

## OCR fallback and image mode

On macOS, a non-trivial observed region (at least 20,000 square screen points) is considered
text-sparse when its AX descendants contain fewer than three text-bearing nodes. The root window
title is not counted. When capture is allowed, tcode captures that window once, runs one accurate,
automatic-language `VNRecognizeTextRequest` over the same PNG, and adds each recognized line as a
`static_text` leaf.
The leaf is marked `pictureOnly` in the rendered outline and as `picture_only: true` in
`inspect_ui`; it has a `click` capability but no live AX element. `search_ui`, `expand_ui`,
`inspect_ui`, and `read_text` operate on these cached leaves like other state-owned nodes.

Vision reports normalized boxes from the image's lower-left corner. tcode maps them into the
outline's absolute screen-point coordinates by scaling through the captured window frame and
flipping the y-axis. Window shadows are omitted from capture so the image and window frame share
the same bounds. Clicking a `pictureOnly` ref posts the normal pointer input at the cached box
center; semantic-only actions are rejected for those refs.

Image mode is also the OCR policy:

- `auto` captures and runs OCR only when the outline meets the sparse rule. If Screen Recording
  permission is absent, both are skipped and the observation reports a warning.
- `always` attaches one screenshot to every `observe_ui` result, but runs OCR only for a sparse
  outline.
- `never` disables both screenshots and OCR, including for explicit visual/fused observations.

Thus an adequate AX tree never incurs Vision work, and a captured image is never OCRed more than
once in one observation.

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
  live status, a **Grant** button (fires the TCC prompt and opens the matching
  `x-apple.systempreferences` pane), and **Recheck**.

### Restart continuity

macOS applies some grants (notably Screen Recording) only after the app restarts, and shows its
own "Quit & Reopen" dialog. tcode therefore treats any permission flow as a potential restart:

1. When the user clicks **Grant**, tcode first writes a small `relaunch.json` marker into the
   data dir: `{ reopen_settings: "computer_use", active_session: <id> }`.
2. Session timelines are already continuously persisted (JSONL + resume cursors), so an
   externally-initiated quit loses nothing.
3. On startup, a present marker is consumed: the previous active session is reopened, the
   Settings window is reopened on the recorded page, and permissions are rechecked
   automatically so the user immediately sees the new status.
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
  outline folding and search ranking, OCR fallback decisions and coordinate/node synthesis,
  state-store eviction and staleness, tool schemas, settings serde round-trips, and MCP
  registration wiring for all three provider paths.
