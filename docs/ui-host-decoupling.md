# tcode UI/host decoupling plan

Goal of **this effort**: split the UI from the backend behind one serializable
protocol pipeline *inside the desktop app*. Every interaction between views and
backend state crosses a single typed, serde-serializable contract; the desktop
app becomes "client #1 of its own backend".

Why: the motivating future is remote coding — a tcode on a laptop connecting to
the tcode running on the home Mac, with everything actually happening on the
Mac. That future is **explicitly out of scope here** (no listener, no pairing,
no network transport in this effort); it is what the pipeline enables. §7
sketches it only so the pipeline is not designed into a corner.

Status: plan only. Nothing here is implemented yet. Coupling inventory that
grounds this plan was swept 2026-07-29 (all of `crates/ui`, `crates/runtime`).

## 1. Where we are

The layering below `runtime` is already clean and mostly serializable:

- `agent` speaks ACP to providers with a `SessionCommand` → `AgentEvent`
  command/event pair; `AgentEvent` is serde.
- Session history is **event-sourced**: per-session `AgentEvent` JSONL,
  `Timeline` is a pure fold. Parked-session re-adoption already replays JSONL.
  This is the cornerstone: the remote pipeline streams the same events and the
  client folds them locally.
- `core` is pure serde data; `services` is fs/git/settings/store.

The debt is concentrated at the top:

- `runtime/src/app.rs` (~13k lines) holds `AppState`, a gpui `Entity` mixing
  backend authority (stores, provider processes, MCP registries, event pumps)
  with window state (`route`, `palette_open`, `sidebar_collapsed`, right-panel
  tabs, `debug_*` seeds).
- Production UI reaches **166 distinct `AppState` methods** through
  `Entity<AppState>::read/update` (~550 call sites in 18 files): 95 mutating
  commands, 4 backend I/O queries, 67 pure selectors. It also reads 22 pub
  fields directly, including the whole folded `active.timeline` as shared
  memory, and in a few places *writes* backend fields directly (composer clears
  terminal contexts via `active.as_mut()`).
- `runtime` cannot compile headless: `Context<AppState>` appears in 147
  production signatures, plus `EventEmitter`, `Task`, `BackgroundExecutor`,
  public `gpui::Rgba`, and one clipboard write (`copy_plan`).
- Side channels bypass `AppState` entirely: UI holds the live `term::Terminal`
  (shared grid + raw PTY byte input); `PreviewPanel` owns the WebViews and
  answers preview-MCP broker requests; composer assembles the final provider
  prompt (slash commands, terminal selections, review comments) and
  base64-encodes attachments on the UI path; settings page performs
  computer-use TCC prompts directly.
- `RuntimeEvent` and several boundary DTOs are not serde yet.

## 2. Target architecture

```
┌────────────── client (any machine) ──────────────┐
│ gpui views · client store (replicated state)     │
│ local emulation: alacritty grid, syntax, diff    │
│ native: WebView, clipboard, drag-drop, dialogs   │
└───────────────┬──────────────────────────────────┘
                │  tcode-protocol: bidirectional JSON-RPC / NDJSON
                │  transport now: in-process duplex · future: network (§8)
┌───────────────┴───────────── host ───────────────┐
│ tcode-host: sessions, providers (ACP), JSONL     │
│ store, settings, git, PTYs, attachments,         │
│ preview/orchestrate/computer-use MCP servers     │
└──────────────────────────────────────────────────┘
```

New crates:

- **`tcode-protocol`** — the single contract: request/response/event types,
  snapshot DTOs, seq numbering, version + capability negotiation. Depends only
  on `core`/`agent` types (all serde). No gpui anywhere.
- **`tcode-host`** (evolves from `runtime`) — gpui-free. `AppState` becomes a
  plain struct owned by one async task (the "host loop") on smol: consumes a
  command channel, emits an event channel, does blocking work on a thread pool.
  Multi-client aware from day one.
- **client store** (in `ui` or a new `tcode-client`) — replicated projections
  fed by protocol events; gpui views read only this store plus purely local
  window state. The 67 pure selectors move into `core`/`protocol` so both sides
  share them; no RPC for formatting.

The host runs **in-process with the desktop app**; in this effort the only
transport is an in-process duplex channel carrying the serialized protocol.
That single seam is the whole point: once the app's own UI drives its backend
exclusively through it, attaching a second (later: remote) client is a
transport problem, not an architecture problem.

## 3. Protocol design

Style: bidirectional JSON-RPC 2.0 over newline-delimited JSON — the same idiom
as ACP, which tcode already speaks downward to providers. Symmetry matters:
client→host for commands/queries/subscriptions, host→client for events and
reverse requests (preview automation). One serialized code path everywhere,
including local in-process transport (traffic is UI-scale; honesty beats the
micro-optimization of passing structs).

Three planes:

1. **Commands** (the ~95 mutations, named and versioned): `send_turn`, `steer`,
   `interrupt`, `respond_approval`, `run_git_action`, `create_project`,
   `update_settings`, terminal lifecycle, ACP marketplace ops, … Fire-and-ack
   with a request id; results that today arrive as toasts become
   request-correlated completions *plus* broadcast state deltas.
2. **Queries** (few, async): the 4 I/O queries (`list_active_workspace`,
   `scan_external_history`, `generate_commit_message`, secret presence) plus
   the file-shaped access the UI does ad hoc today, made explicit: file bytes
   fetch (diff sides, attachment/image bytes), git diff load, fs listing. These
   go through the pipeline even in-process so the seam stays honest.
3. **Subscriptions** (host→client push, per domain, snapshot + ordered deltas
   with seq numbers):
   - `session/<id>/events`: snapshot = JSONL replay, delta = live
     `AgentEvent`s. The client folds `Timeline` itself — identical to today's
     re-adoption path.
   - `index`: `SessionMeta`/`Project` upserts (sidebar).
   - `settings`: whole-document replace (small).
   - `runtime-events`: errors/notices/toasts, made serde; command-triggered
     toasts target the issuing client, state changes broadcast.
   - `terminal/<id>`: raw output bytes + exit/title events (see §4).

Two design rules paid for now, cashed in later: every subscription carries seq
numbers and can rebuild from snapshot (in-process this is just the resubscribe
path; remotely it becomes lossless reconnect), and protocol enums are
forward-tolerant (`#[serde(other)]` / value passthrough) so version skew
between two tcodes degrades instead of failing. Neither costs meaningful
complexity today; both are nearly impossible to retrofit.

## 4. The hard parts, decided

- **Terminal.** PTY stays host-side; the *emulation* moves client-side. Split
  `term` into pty ownership (host) and alacritty grid (client). The wire
  carries what SSH carries: output bytes down, input bytes + resize up. No grid
  diffing protocol, no shared memory.
- **Preview WebView.** The WebView is native client UI and stays there. The
  preview-MCP broker stays host-side (providers connect to it on the host);
  its `BrokerRequest`s — which already cross an async channel with a reply
  slot — become reverse RPCs over the pipeline to the client owning the panel.
  In-process this is nearly a rename of the existing flow.
- **Attachments & images.** Paths in messages are host paths. Client-side
  pastes/drops upload bytes via protocol; host validates/transcodes/stores
  (today's composer logic moves host-side) and returns the stored path.
  Timeline image rendering fetches bytes by path through the query plane with a
  client cache. `save_attachment_to_dir`, `remove_user_file`, `read_file_bytes`
  and the rest of `ui_facade` become protocol queries; `ui_facade` is deleted.
- **Prompt assembly & slash commands.** Composer currently builds the final
  provider prompt (terminal selections, review comments, `/plan`, `/model`,
  `/orchestrate` routing) and mutates backend drafts directly. This is business
  logic; it moves into the host command handler (`send_turn` takes the typed
  text + attachment ids + flags; the host composes). The client keeps only
  trigger-menu UX.
- **Native pickers & paths.** Workspace paths are host paths; the UI treats
  them as opaque strings. The native directory picker stays (client and host
  share a filesystem in this effort), but its *result* enters the backend only
  through a command, and fs listing for `@` completion is a protocol query.
- **Computer use.** Entirely host-side (screen/AX). The TCC prompt/relaunch
  flow currently in `settings_page.rs` moves behind commands so the UI only
  toggles and observes status.
- **Colors/clipboard leaks.** `gpui::Rgba` in accent APIs becomes a hex string
  in protocol/core; `copy_plan`'s host-side clipboard write becomes a
  client-side effect event (clipboard is always a client device).
- **Window state.** `route`, `palette_open`, `sidebar_collapsed`, right-panel
  tab/expansion, terminal drawer height, `debug_*` seeds leave `AppState` and
  live client-side (per-window). Per-conversation UI state that today parks in
  `conversation_ui` stays client-side keyed by conversation destination.
- **Multi-client.** The protocol never assumes exclusivity: events broadcast,
  commands serialize through the host loop, command-triggered toasts target
  the issuing client. In this effort there is exactly one client (the app's
  own UI), but the contract is written as if there were N.

## 5. Migration plan — strangler fig, app always shippable

Each phase compiles, passes `cargo test --workspace`, and ships behind no flag.
Verification per phase: existing smoke mode (`--smoke`), plus a protocol
loopback test harness added in P1.

- **P0 — Purify the boundary** (mechanical, delegatable).
  Split window state out of `AppState` into a client-side struct. Kill direct
  field writes from UI (composer's `active.as_mut()`, one-shot debug consumes)
  by adding commands. Move prompt assembly + slash-command routing from
  composer into runtime. Make `RuntimeEvent` + boundary DTOs serde; replace
  public `Rgba` with hex strings; move `copy_plan` clipboard to a UI effect.
  Exit: UI performs no direct field mutation; runtime's public surface is
  serde-serializable in principle.

- **P1 — `tcode-protocol` crate.**
  Define Command/Query/Event/Subscription enums covering the inventoried
  surface (95 commands, 4 queries, per-domain subscriptions), snapshot DTOs,
  seq numbers, hello/version. Move the 67 pure selectors to shared code.
  Exit: round-trip serde tests for every type; a loopback harness exists.

- **P2 — Client store; UI reads only replicas** (the bulk; view-by-view,
  delegatable per file).
  Introduce the client store fed by host events over an in-process channel
  (still unserialized at this step). Migrate the 18 UI files off
  `Entity<AppState>` reads onto the store + protocol commands, one view at a
  time (suggested order: sidebar → settings → chat → composer → diff →
  terminal drawer → panels). Timeline folding moves client-side.
  Exit: `crates/ui` has zero `Entity<AppState>` references.

- **P3 — Host off gpui; serialize the pipe.**
  Replace `Context`/`Task`/`BackgroundExecutor` with smol + channels; host loop
  owns state on its own thread; the in-process transport becomes real NDJSON
  JSON-RPC. Split `term` (pty host-side, grid client-side; the boundary
  carries output bytes down, input bytes + resize up). Preview broker requests
  ride the pipeline as reverse RPCs.
  Exit — this is the finish line of the whole effort: `tcode-host` compiles
  with no gpui dependency; the desktop app runs fully through the serialized
  in-process pipe; `crates/ui` has zero `Entity<AppState>` references; the
  smoke suite passes end-to-end over the pipeline.

Rough effort: P0 and P1 are days each; P2 is the long tail (weeks,
parallelizable per view once the store lands); P3 is a week of concentrated
runtime surgery.

## 6. Open questions

- Diff compute placement: client folds diffs today from full file texts; over a
  slow link, host-side hunk computation may be worth a capability flag later.
- i18n stays client-side (events are localization-free already — keep it that
  way as a protocol invariant).

## 7. Future (out of scope here): remote tcode-to-tcode

Kept only as design guardrails, so the pipeline built above needs no rework:

- The Mac's running tcode would open its own listener (off by default) and the
  traveling laptop's tcode would connect directly — tcode-native pairing
  (short-lived code shown on the host, then mutual per-device key pinning,
  revocable), TLS on the wire, never an unauthenticated port, no ssh involved.
  Reachability (NAT) stays the user's network layer; no tcode relay service.
- Remote-only work at that point: connection UI, fs-browser Add Project,
  attachment byte upload, image fetch caching, preview port forwarding (a
  multiplexed TCP proxy stream in the same connection), reconnect via the seq
  numbers §3 already mandates, version-skew handling via the hello exchange.
- Everything in §§2–5 is deliberately already compatible with N clients and a
  serialized wire, so none of it should need revisiting.
