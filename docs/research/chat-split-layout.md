# Splitting the chat column into tiled thread panes

Date: 2026-10-26

Question: the ui-v2 shell should let the chat column be split like a
terminal — side by side, a 2×2 grid, one tall pane beside two stacked ones —
with one thread per pane. What does the current code own that stands in the
way, what does gpui-base already provide, and in what order is the capability
built so that every step ships on its own?

Everything below is derived from the code as of `ui-v2` at this date and from
gpui-base 0.8.0 as vendored in the registry; nothing here is a requirement
until it is in [CONTRIBUTING.md](../../CONTRIBUTING.md) or in the code.

## What the shell is today

The wide layout ([`render_wide`](../../crates/ui/src/shell.rs)) is one flat
`h_resizable` group: sidebar · chat · right panel. The chat slot holds the
attachment's single `Entity<ChatView>`, which owns its composer, terminal
drawer, pull-request panel and timeline list. The right panel (diff, plan,
agents, preview, pull requests) is a sibling, with its own floating mode when
the column is too narrow for both.

The thread on screen is a property of the **store**, not of the view.
[`WorkspaceStore`](../../crates/ui/src/store/mod.rs) holds one
`selected_session_id`, one `session_replica` (timeline), one
`session_status_replica`, one `git_status_replica`, the selected thread's
`command_outputs`, history paging, pending anchors, and the
`conversation_on_screen` flag. Switching threads is
[`select_session`](../../crates/ui/src/store/intents.rs): it leaves the old
thread (its replica is parked in `threads`, bounded by `KEPT_THREADS = 4`),
unsubscribes, subscribes the four session topics for the new one, and
rebuilds the replica. Every view that says "the active thread" reads
`active_session_id()`; the callers today are:

| File | Reads of `active_session_id` |
| --- | --- |
| `store/intents.rs` | 36 (every session command defaults to it) |
| `shell.rs` | 22 |
| `pull_requests/mod.rs` | 11 |
| `chat/mod.rs` | 11 |
| `sidebar.rs` | 9 |
| `diff/view.rs`, `preview_panel*`, `composer/*` | 2–3 each |

Two facts settle the shape of the work:

- **The host already serves many live sessions to one client.** Subscriptions
  are a set of topics ([`subscribe`](../../crates/runtime/src/app/sessions.rs));
  each session topic makes its session resident (`residents.live` is a map),
  and a session is parked only when its last topic goes. Nothing on the host
  or on the wire changes for N panes. The protocol version is untouched.
- **One store per pane is not an option.** `HostLink::events()` hands out
  clones of one `async_channel` receiver, so two stores on one link would
  split the stream between them rather than both see it. The workspace-level
  replicas (index, projects, settings, providers, terminals, preferences,
  connection state) also have one owner and must keep one. The thread-level
  state has to move out of the store into something there can be several of.

## What gpui-base provides

gpui-base 0.8.0 ships `gpui_base::dock`
(`~/.cargo/registry/src/*/gpui-base-0.8.0/src/dock`), which Tcode does not use
yet. It is the layout engine this feature needs, and Principle 6 says to use
it before writing a tree of our own:

- `PaneTree` is pure data: `Split` and `Tabs` nodes with stable `NodeId`s,
  panels by `PanelId` (the entity id). Every edit — `split`, `insert_panel`,
  `move_panel`, `remove_panel`, `set_active`, `set_sizes` — normalizes before
  it returns, and the algebra is unit-testable without a window.
- `DockArea` reconciles the tree into `TabGroup` entities, attaches drag
  sources, drop hit-testing, split handles, focus and keyboard handling, and
  emits `DockEvent::LayoutChanged`. `split_at(node, panel, Placement, …)`
  is the terminal-style "split this pane" operation; `set_zoomed_in/out` is
  the pane zoom; `set_center(DockLayout)` installs a whole layout, which is
  how presets (two columns, 2×2, one-beside-two) are expressed.
- `dump` / `load` round-trip the area through `DockAreaState`, rebuilding
  panels through a `PanelRegistry` keyed by `panel_name`, with each panel's
  own `serde_json` info — this is the persistence seam.
- Appearance is entirely the host's through `DockAreaRenderer` (frames,
  split handle) and `TabGroupRenderer` (tab bar, active-panel placement,
  drop indicator, empty group). Nothing is painted by base, so Tcode's
  material and motion stay Tcode's.
- A panel implements `Panel` (`panel_name`, `closable`, `zoomable`,
  `set_active`, `on_removed`, `dump`) on top of `Render + Focusable +
  EventEmitter<PanelEvent>`.

There is no leaf node: a panel always lives in a `Tabs` group. For a
terminal-like feel the renderer draws the **chat header as the group's tab
bar** when the group has one panel, and real tabs only when a second thread
is dropped onto the same pane. Stacking threads as tabs in a pane therefore
comes for free rather than being designed around.

One known hazard: the comment above `chat_panel` in `render_wide` records
that nesting a second resizable group inside the chat panel once failed to
shrink the chat (the right panel painted over it). `DockArea` builds on the
same `ResizablePanelGroup`. Whether that still happens with the dock inside
the chat slot is the first thing to establish, before anything else is built
on it (see Phase 1).

## Design

### Ownership

```
WorkspaceStore            one per attachment: index, projects, settings,
  │                       providers, terminals, preferences, connection,
  │                       focused_session_id, and the routing of session
  │                       topics to the thread that owns them
  └── ThreadStore ×N      one per open thread (pane or tab): session replica,
        │                 status, plan, git status, history paging, command
        │                 outputs, pending turn/anchor, composer appends,
        │                 fallback block/review, conversation UI state
        └── ThreadPanel   the dock panel: ChatView + Composer + TerminalDrawer
                          + PullRequestsPanel, bound to one ThreadStore
```

- `ThreadStore` is the promotion of today's private `ThreadReplica` into an
  entity views can observe. The store keeps `threads: HashMap<String,
  Entity<ThreadStore>>`; the `KEPT_THREADS` bound applies to threads no pane
  shows, exactly as it applies to left threads today.
- Incoming session-topic envelopes route by `session_id` to the owning
  `ThreadStore`; today they are checked against `selected_session_id`. A
  thread is subscribed while any pane shows it and unsubscribed when the last
  one closes (the host parks it then, as it does now on `leave_session`).
- `focused_session_id` replaces `selected_session_id` for everything that
  means "the thread the user is acting on": the sidebar highlight, the
  window title, palette commands on the active thread, the git button and
  pull-request pill, the diff/plan/preview/agents right panel's target, read
  acknowledgement, and the `Command`s in `intents.rs` that default their
  `session_id`. Those commands take the session explicitly where the caller
  is a pane; the "active" default remains for the palette and the sidebar.
- The right panel stays a sibling of the chat column and follows the focused
  pane. The preview's native WebView is one instance, so it must follow
  focus rather than be per pane. The terminal drawer is inside `ChatView`,
  so it is per pane without further work.
- The compact (phone/narrow) shell keeps one thread on screen: it renders
  the focused pane's `ThreadPanel` and never reads the layout tree. The
  tree is desktop-wide state.

### Layout

- The chat slot of the outer workspace group hosts one `DockArea` whose
  center region is the pane tree; no side or bottom docks in this feature
  (the sidebar and right panel keep their current, tested behaviour).
- `ThreadPanel` is the only panel kind, `panel_name = "thread"`, with its
  `dump` info `{ "session_id" }` or `{ "project_draft" }`.
- Per-pane minimum width is `CHAT_MIN_WIDTH` (the header's compact minimum,
  532 px). The right-panel floating decision uses the tree's minimum width
  (sum of the widest row's pane minimums) instead of one chat's minimum.
- Pane count is capped at six (the same bound as
  `MAX_TERMINALS_PER_SESSION`); each open pane keeps a host session resident,
  which is the real cost. The split actions disable at the cap.
- Focus is the dock's: the focused group gets a ring from gpui-base's focus
  tokens; no second focus notion is kept.

### Interaction

Terminal conventions, bound in `shortcut.rs` (none of these keys are taken
today):

| Action | Key | Effect |
| --- | --- | --- |
| `SplitRight` | `secondary-d` | New pane right of the focused one |
| `SplitDown` | `secondary-shift-d` | New pane below the focused one |
| `ClosePane` | `secondary-shift-w` | Close the focused pane (its thread stays in the sidebar; `secondary-w` keeps closing the window) |
| `FocusPane(Left/Right/Up/Down)` | `secondary-alt-arrow` | Move focus spatially |
| `FocusPane(Next/Prev)` | `secondary-]` / `secondary-[` | Move focus in layout order |
| `ZoomPane` | `secondary-shift-enter` | Toggle the focused pane to fill the column |
| `ApplyLayout(Columns2 / Grid2x2 / OneBesideTwo / Single)` | palette only | Install a preset; existing panes fill slots in layout order, empty slots open project drafts |

What a new pane opens: the **New thread draft of the focused pane's
project** (`ConversationDestination::ProjectDraft`), the chat equivalent of
a new shell. A thread already open in another pane is not opened twice:
selecting it from the sidebar or the palette focuses the pane that has it.
Alt-click and the sidebar's context menu offer "Open in new pane"; plain
click replaces the focused pane's thread, as it does today.

Surfaces: the same actions appear in the command palette and in a split
menu on the chat header beside the existing panel controls. Drag and drop
between panes, and dropping one pane onto another to stack them as tabs,
come from the dock with no extra behaviour code — only the drop indicator's
appearance in the renderer.

### Persistence

Layout is a screen's concern (Principle 2), so it is client-local: a new
`layout: Option<serde_json::Value>` beside `navigation` in
`ClientPreferences`, keyed by attachment target, holding `DockAreaState`
from `DockArea::dump`. Restore goes through `PanelRegistry`: a `"thread"`
panel whose session is no longer in the index is dropped the way
`discard_restored_thread` drops a vanished thread today, and a layout that
loses all its panes falls back to a single draft pane. The desktop's
existing "fresh path on launch" policy (`restore_navigation = false`) also
decides whether the layout is restored; the phone never restores it.

## Phases

Each phase is a mergeable series that leaves the app working; every phase
runs fmt, clippy, nextest, and a look at the surface in both themes at a
wide and a narrow width.

**Phase 0 — thread state gets an owner (no visible change).**
Move the per-thread fields out of `WorkspaceStore` into
`crates/ui/src/store/thread.rs` (`ThreadStore`), route session envelopes by
id, and give `ChatView`, `Composer`, `TerminalDrawer`, `PullRequestsPanel`,
`DiffPanel` and `PreviewPanel` an `Entity<ThreadStore>` next to the
workspace store. `selected_session_id` becomes `focused_session_id` with the
same single-thread behaviour. Store tests that assert replica contents move
to the thread store; the ones asserting routing stay on the workspace store.
This is the largest phase and is pure refactor; it is judged by the shell
tests in `shell.rs` and the store tests passing unchanged in contract.

**Phase 1 — the dock, with one pane.**
Add `crates/ui/src/panes/` with `ThreadPanel: Panel`, the two renderers
(the chat header as the tab bar of a one-panel group; Tcode's split handle
and focus ring), and put a `DockArea` with a single center pane in the chat
slot. First deliverable is the spike for the nesting hazard above: if the
dock inside the chat slot cannot reflow against the right panel, the
alternative is one `DockArea` owning the whole workspace row (sidebar as the
left dock, right panel as the right dock) and the floating right panel is
rebuilt on `DockPlacement::Right`; that decision is taken here, not later.
Visual parity with today is the acceptance bar.

**Phase 2 — splitting.**
Actions, key bindings, palette commands, header split menu, presets, the
pane cap, per-pane minimum width in the floating math, focus-follows-pane
for the right panel, sidebar highlight and title, "open in new pane", and
focusing an already-open thread instead of opening it twice. Locale keys
go to both `en.yml` and `zh-CN.yml`. Tests: tree-level cases through
`DockArea` for split/close/zoom/focus order, a shell test that a split
leaves the right panel docked and the chat headers clear, and a store test
that two panes receive their own session events.

**Phase 3 — persistence.**
`ClientPreferences.layout`, dump on `LayoutChanged` (debounced like the
navigation checkpoint), load on attach, prune vanished threads, compact
ignores it. Tests: a cold start restores two panes, and a layout naming an
archived thread restores the surviving pane.

**Phase 4 — direct manipulation.**
Drop indicator and drag preview appearance, dragging a sidebar thread onto a
pane edge (an `AnyDrag` payload the dock resolves into `split_at`), and the
tabbed pane's tab strip when a group holds more than one thread.

## Open questions for the maintainer

- Whether a new pane should open a project draft (this plan) or duplicate
  the focused thread. Duplicating means two views over one `ThreadStore`
  with one composer draft; it is possible but deferred.
- Whether the cap of six panes should instead follow `KEPT_THREADS`, or be a
  setting. Six is the terminal's existing bound and needs no new setting.
- Whether the layout should be remembered per host (this plan) or per
  project.
