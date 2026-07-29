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

Status: **P0–P3 implemented** (2026-07-29). The desktop UI now uses a real
serialized NDJSON pipe, `WorkspaceStore` contains no `Entity<AppState>`, and
`tcode-runtime` has no gpui dependency. The four deliberately local handle
affordances retained for the in-process transport are listed in §§3–4.

## 1. Starting point (historical)

The layering below `runtime` is already clean and mostly serializable:

- `agent` speaks ACP to providers with a `SessionCommand` → `AgentEvent`
  command/event pair; `AgentEvent` is serde.
- Session history is **event-sourced**: per-session `AgentEvent` JSONL,
  `Timeline` is a pure fold. Parked-session re-adoption already replays JSONL.
  This is the cornerstone: the remote pipeline streams the same events and the
  client folds them locally.
- `core` is pure serde data; `services` is fs/git/settings/store.

The debt was concentrated at the top:

- `runtime/src/app.rs` (~13k lines) held `AppState` as a gpui `Entity`, mixing
  backend authority (stores, provider processes, MCP registries, event pumps)
  with window state (`route`, `palette_open`, `sidebar_collapsed`, right-panel
  tabs, `debug_*` seeds).
- Production UI reached **166 distinct `AppState` methods** through
  `Entity<AppState>::read/update` (~550 call sites in 18 files): 95 mutating
  commands, 4 backend I/O queries, 67 pure selectors. It also reads 22 pub
  fields directly, including the whole folded `active.timeline` as shared
  memory, and in a few places *writes* backend fields directly (composer clears
  terminal contexts via `active.as_mut()`).
- `runtime` could not compile headless: `Context<AppState>` appeared in 147
  production signatures, plus `EventEmitter`, `Task`, `BackgroundExecutor`,
  public `gpui::Rgba`, and one clipboard write (`copy_plan`).
- Side channels bypassed `AppState` entirely: UI held the live `term::Terminal`
  (shared grid + raw PTY byte input); `PreviewPanel` owns the WebViews and
  answers preview-MCP broker requests; composer assembles the final provider
  prompt (slash commands, terminal selections, review comments) and
  base64-encodes attachments on the UI path; settings page performs
  computer-use TCC prompts directly.
- `RuntimeEvent` and several boundary DTOs were not serde yet.

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

Implemented layers:

- **`tcode-protocol`** — the single contract: request/response/event types,
  snapshot DTOs, seq numbering, version + capability negotiation. Depends only
  on `core`/`agent` types (all serde). No gpui anywhere.
- **`tcode-runtime`** (the host; no crate rename was needed) — gpui-free.
  `AppState` is a plain `Send` struct owned by a dedicated thread running the
  smol host loop. Decoded client messages and background completions serialize
  through one mailbox; blocking work is centralized on `smol::unblock`.
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

Style: a correlated, tagged bidirectional protocol over newline-delimited JSON
(not a claim of strict JSON-RPC 2.0 conformance). Symmetry matters:
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
   - active/session status: terminal ids, split layout, selected contexts,
     drawer state, and active tab. Raw terminal bytes are deliberately absent
     from JSON (see §4).

Every subscription carries seq
numbers and can rebuild from snapshot (in-process this is just the resubscribe
path; remotely it becomes lossless reconnect). Tagged protocol enums reject
unknown command payloads as structured decode errors; selected value enums use
`#[serde(other)]` where a safe unknown value exists.

The in-process transport has four explicit non-NDJSON handle affordances:

- the preview broker receiver moves once into `HostHandle`; remote replaces it
  with reverse RPC;
- the orchestrate broker receiver moves once into the host at construction and
  never reaches the UI; remote replaces it with correlated RPC;
- one external-import progress bus is installed at host construction; each
  serialized command gets a client-local receiver routed by its request id,
  and remote replaces the bus with progress events;
- `LocalTerminalRegistry` shares live terminal handles created at construction;
  remote replaces those with the raw byte streams described below.

Commands, queries, subscriptions, snapshots, deltas, and runtime events have no
typed shortcut: they all cross serde as NDJSON lines.

Former consuming reads now have explicit ownership: native-rewind prefills are
one-shot `NativeRewindPrefill` events cached per session by the client until the
composer consumes them; diff-focus is purely client-owned replica state;
pending-relaunch consumption is a correlated command result. None reads or
mutates `AppState` through shared memory.

## 4. The hard parts, decided

- **Terminal.** `term` is split into host `PtyHandle` and client
  `GridEmulator`. For this in-process phase, the compatibility `Terminal`
  objects cross only through `LocalTerminalRegistry`; no terminal byte is
  encoded as JSON. A remote transport will replace that registry with what SSH
  carries: raw output bytes down and input bytes + resize up.
- **Preview WebView.** The WebView is native client UI and stays there. The
  preview-MCP URL/token registry stays host-side; its receiver is the explicit
  local reverse-RPC affordance above and is taken exactly once by the client.
- **Attachments & images.** Paths in messages are host paths. Client-side
  pastes/drops validate/transcode locally, then upload bytes through the query
  plane; the host stores them and returns the stored path.
  Timeline image rendering fetches bytes by path through the query plane with a
  client cache. `save_attachment_to_dir`, `remove_user_file`, `read_file_bytes`
  and the other client file operations are protocol queries. The remaining
  runtime `ui_facade` types are host-internal import-progress DTOs.
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
- **Computer use.** The MCP server and provider registration are host-side.
  Native TCC presentation remains client UI; settings and the relaunch marker
  cross serialized commands, and relaunch consumption is correlated.
- **Colors/clipboard leaks.** `gpui::Rgba` in accent APIs becomes a hex string
  in protocol/core; `copy_plan`'s host-side clipboard write becomes a
  client-side effect event (clipboard is always a client device).
- **Window state.** `route`, `palette_open`, `sidebar_collapsed`, right-panel
  tab/expansion, terminal drawer height, `debug_*` seeds leave `AppState` and
  live client-side (per-window). Per-conversation UI state that today parks in
  `conversation_ui` stays client-side keyed by conversation destination.
- **Multi-client.** This effort intentionally has one client endpoint. Request
  ids, topic snapshots, and sequence numbers preserve the information a later
  fan-out transport needs; multi-client arbitration remains remote-phase work.

## 5. Migration plan — strangler fig, app always shippable

Each phase compiles, passes `cargo test --workspace`, and ships behind no flag.
Verification per phase: existing smoke mode (`--smoke`), plus a protocol
loopback test harness added in P1.

- **P0 — Purify the boundary** ✅ done.
  Window state (route, palette, sidebar collapse, quit confirm, debug seeds)
  lives in a UI-owned `WindowState`; the UI performs no direct backend-field
  writes; the send commands take typed text + attachment paths and the runtime
  assembles terminal contexts, review comments and attachment encoding itself;
  `RuntimeEvent` + boundary DTOs are serde; accents are plain `u32` colors;
  the plan-copy clipboard write is a UI-handled effect.
  Deliberate deviation from the original wording: slash-command *interception*
  (`/plan` `/default` `/model`, and `/orchestrate` prefix detection) stays in
  the composer — it is input UX in the same class as the trigger menus; the
  runtime remains authoritative for everything that reaches a provider.

- **P1 — `tcode-protocol` crate** ✅ done.
  Define Command/Query/Event/Subscription enums covering the inventoried
  surface (commands, explicit I/O queries, per-domain subscriptions), snapshot DTOs,
  seq numbers, hello/version. Move the 67 pure selectors to shared code.
  Exit: round-trip serde tests for every type; a loopback harness exists.

- **P2 — Client store; UI reads only replicas** ✅ done (the bulk; view-by-view,
  delegatable per file).
  Introduce the client store fed by host events over an in-process channel
  (still unserialized at this step). Migrate the 18 UI files off
  `Entity<AppState>` reads onto the store + protocol commands, one view at a
  time (suggested order: sidebar → settings → chat → composer → diff →
  terminal drawer → panels). Timeline folding moves client-side.
  Exit: `crates/ui` has zero `Entity<AppState>` references.

- **P3 — Host off gpui; serialize the pipe** ✅ done.
  Replace `Context`/`Task`/`BackgroundExecutor` with smol + channels; host loop
  owns state on its own thread; the in-process transport becomes real correlated
  NDJSON. Split `term` into host PTY and client grid halves. The in-process
  terminal registry and preview broker receiver are the documented local
  affordances; neither terminal bytes nor ordinary typed messages bypass serde.
  The local terminal and preview affordances above deliberately stop at the
  transport boundary required by this phase; their remote byte-stream/reverse
  RPC replacements remain §7 work.
  Exit — this is the finish line of the whole effort: `tcode-runtime` compiles
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
- The serialized contract and snapshot/sequence model are reusable for N
  clients; connection ownership, arbitration, and fan-out still need explicit
  remote-phase implementation.
