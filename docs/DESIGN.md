# tcode design spec

The visual and interaction contract for tcode. Update it deliberately when a
product decision changes; historical design drafts are not additional rules.
Phone and browser adaptations are in [mobile design](mobile-design.md).

## Design tokens

The embedded [theme](../themes/tcode.json) owns colors and font choices;
[material.rs](../crates/ui/src/material.rs) owns surface treatments, shared
geometry and radii. Use those definitions rather than maintaining a second
palette in documentation.

DM Sans is bundled for UI text. Desktop monospace text uses the configured
system family; mobile font registration is described in [mobile design](mobile-design.md).
The centered chat/composer column is 720px wide at most. Desktop prose and
composer text use 13.5px type with a 21px line height; metadata is smaller and
muted, with monospace for paths, command text and numeric evidence.

The material layers are:

| Layer | Use | Treatment |
| --- | --- | --- |
| T0 | Sidebar and window edges | Translucent theme canvas over the native window material |
| T1 | Chat, right panel and Settings reading surfaces | Near-opaque warm paper in light mode, blue carbon in dark mode |
| T2 | Inline fields, hover and selection | Theme-derived tints |
| T3 | Composer, popovers, dialogs, menus and toasts | Opaque popover fill, hairline border and soft shadow |

Keep the same material composition when navigating between Chat and Settings.
Separate reading regions with space and material contrast; use faded or inset
hairlines where a rule is needed. Hover and focus must not change geometry.
Diff additions and deletions use the success and danger colors consistently.

## Window material

The persistent main window uses native backdrop material: macOS keeps its
existing blurred vibrancy, while Windows deliberately uses GPUI's
`WindowBackgroundAppearance::Blurred`, which the locked Windows backend maps to
Acrylic Accent state 4. Mica was rejected because it did not provide the
perceptible live background-through blur required in the exposed T0 sidebar and
window-edge regions. Both native materials retain the embedded theme's
translucent canvas so the system backdrop can show through.
`TCODE_NO_VIBRANCY=1` keeps its macOS-only
diagnostic behavior: an opaque window with a flattened canvas. Linux and other
platforms remain opaque and flatten that canvas to its solid RGB base. In-app
T3 child surfaces (popovers, menus, dialogs, drawers and toasts) use the fully
opaque `popover.background` token so lower layers never show through; they do
not receive native Acrylic.

## Layout metrics (at 1440×900)

- Sidebar is resizable. Collapsed it occupies **0px** —
  no icon strip, no layout node at all: the chat (and right panel) run to the
  window's left edge. Collapsed, entering the first 12px at the window's left
  edge reveals the sidebar as an **overlay** (see Sidebar below).
- Window top is seamless: no app titlebar — the sidebar's first row (traffic
  lights inset 74px, wordmark + channel pill) and the chat header (52px) form
  the top strip; both are window-drag areas.
- Chat content column: max-width 720px, centered, ≥24px horizontal padding
  (must reflow, never clip, when the diff panel narrows the chat region).
- Composer: floating opaque card with the shared composer radius, a hairline
  border and subtle shadow. Focus changes the border color without resizing it.
- Sidebar thread rows ≈30px, 13px text, 4px-radius hover bg.
- Parent thread rows always show a disclosure chevron and total-child badge;
  when children are active, the badge reads active/total in the success color.

## Scrolling contract

Potentially unbounded content always has its own resolved-height viewport and a
separate, non-shrinking content column. Headers, search fields, footers and
actions stay outside that viewport. This applies to the sidebar project list,
Settings content, command-palette results, Add Project recents (capped at
390px), ACP/model catalogs, model traits, branch and diff-scope selectors,
queued messages, user-input options, approval details and expanded toast
details. A bounded flex column with `overflow` on the same node is not an
acceptable substitute: flexbox can shrink its rows until no scrollable overflow
remains.

## Surface anatomy

### Sidebar

Expanded, it is the first panel of the workspace resizable group (220–380px,
dragged width remembered across collapse/expand and window resizes). Collapsed,
it is **not in the layout at all**. A fixed, invisible 12px-wide element-level
hover trigger sits at the left edge and only opens the sidebar. The revealed
sidebar is a separate absolute sibling at its remembered width, rendered as a
shadowed overlay
*painted on top of* the chat: the content columns never reflow when it appears
or disappears. The trigger ignores hover-exit; the overlay's own occluding
hitbox and hover listener keep it open while occupied and close it when the
pointer leaves. It is strictly transient state, never persisted, and command
palette / dialogs / toasts still layer above it. Its own contents are identical
in both states.

1. App row: "tcode" bold 14px, channel pill ("DEV"). No collapse button — the
   toggle lives in the chat header, since a collapsed sidebar has no width to
   host it.
2. Search row: magnifier + "Search" muted + ⌘K (macOS) / Ctrl+K
   (Windows/Linux) kbd chip → opens the palette.
3. Project/thread header: sort, grouped/flat layout and add-project controls.
   Sorting and layout choices are persisted.
4. Project groups: rotating chevron + folder icon + 13px medium name; hover
   shows "+" (new thread in project); collapse state persisted.
   Thread rows: single-line truncated AI-generated title (first-message fallback
   while naming) + relative time (muted 11px); hover = accent bg. Inline rename
   commits on Enter and cancels on blur or any click outside the input.
   On hover, time swaps to the archive icon; active = persistent accent bg; a running
   session shows "● Working" (green, 11px) left of the title; >6 threads →
   "Show more" / "Show less" toggle row (the row remains available after
   expansion so the list can be collapsed again).
5. Footer: gear + "Settings" → settings route.

### Chat header

52px. The first control is the **sidebar toggle**, immediately left of the
title: `PanelLeft` + "Collapse sidebar" while expanded, `PanelLeftOpen` +
"Expand sidebar" while collapsed. Then the thread title 16px medium ("No active
thread" muted when empty); right: the git/Open actions and the terminal · plan ·
preview · diff panel toggles. The title stretch is the window-drag handle; the
toggle is a real button and never arms a drag. Collapsed on macOS (windowed) the
row is inset 80px so the toggle clears the native traffic lights; no other
platform pays that inset.

### Timeline

- Turns are separated by 32px; smaller gaps group blocks within a turn. There
  is no divider under the user bubble: space and typography separate prose from
  the muted activity summary.
- Subagent capsules use a spinner while active, then a compact lifecycle chip:
  green for completed, amber for interrupted, and red for failed or declined.
- Turn activity uses collapsible "Work Log" sections. A compact disclosure
  header reveals transparent activity rows
  (muted status icon + one-line command/tool/subagent/reasoning summary).
  Details sit beneath their row; execution traces do not need enclosing cards.
  While a turn is running,
  the latest five activities remain directly visible. Once a sixth arrives,
  only the older prefix is summarized by a collapsed Work Log row (with a
  working spinner on its right); those five visible activities are excluded
  from that row's counts. The newest command output or file-edit diff stays
  automatically expanded until a newer activity appears. A superseded detail
  folds immediately if it has already been visible for 500ms; otherwise it
  stays open only for the remainder of that minimum visibility window, unless
  another activity supersedes it first. A detail with two newer activities ahead
  of it folds immediately regardless of that window. Opening a running thread is
  a current-state snapshot, not a replay: among activities that arrived while
  the thread was away, only the newest detail opens, and its 500ms visibility
  window starts when the thread becomes visible.
  Assistant prose settles the run, folding every
  activity in it under one summary row. A completed section's toggle summarizes
  only its real, nonzero events
  (commands, unique edited files, tool calls, subagents, and compactions). Each
  section counts only the activities folded into it.
  Empty activity sections are omitted; unclassified activity still has a Work Log
  disclosure rather than disappearing.
- Assistant Markdown follows the prose typography above. Streaming follows the
  latest output only while the reader remains near the bottom.
- User messages: right-aligned bubble, muted bg, radius 12, max-width 75%.
- A confirmed provider handoff inserts a subtle centered divider chip before
  the next user bubble: “Relayed from X to Y”. The injected handoff transcript
  is provider-only context and never renders as a message or disclosure row.
- **Disclosure rows** fold injected, non-conversational context out of the
  bubbles into a reusable centered control: a collapsed-by-default row of 12px
  muted `label ›` whose chevron rotates and whose background lifts to accent on
  hover. Clicking toggles a per-entry expansion (state lives on the chat view,
  keyed by entry id — not global), revealing the injected text verbatim as 13px
  muted preformatted prompt source inside a bordered muted card. Because that
  text can be long (orchestrate guidance), the card is a resolved-height,
  capped-at-320px scroll viewport of its own rather than growing the turn. Two
  things render as disclosure rows today: an `/orchestrate` turn shows an
  "Orchestrate Skill ›" row above a bubble that now holds only the user's own
  words (the injected guidance + configuration prefix is the disclosure; the
  provider still receives the whole composed text); and a child-thread callback
  renders as a single "`{title, ≤24 chars…} {state} ›`" row **instead of** a
  bubble. A disclosure row sits where the turn's user bubble would start and
  keeps the surrounding turn rhythm. Message actions follow the split: the
  orchestrate bubble's Copy copies only the visible user text; callback rows are
  not bubbles and carry **no** action row. Messages logged before the split
  annotation existed lack it
  and render as an ordinary full bubble, exactly as before.
- **Message actions.** Every message reserves a 24px action row under it (the
  height is always taken, so revealing it never shifts the timeline). It is
  hidden until the message is hovered — except on the newest user and newest
  assistant message, where it stays visible so the actions are reachable without
  hovering. Ghost xsmall buttons, icon + label:
  - user bubble (right-aligned row): **Copy**, plus a provider-native rewind
    menu when that provider supplied a checkpoint for the turn. Claude Code
    offers **Restore code and conversation**, **Restore conversation**, and
    **Restore code**; conversation options are unavailable on the first turn
    because there is no preceding assistant state. Rewind is disabled while a
    turn or another rewind is active. Steered messages carry Copy alone.
  - assistant message (left-aligned row): **Copy**.
  - Copy puts the message's **raw text** (the markdown source, not the rendered
    document) on the clipboard and flips to "Copied!" for 2s.
- **Provider-native rewind.** Tcode owns no checkpoint store and never truncates
  its event log. For supported Claude Code versions, replayed user-message UUIDs
  become opaque turn checkpoints and the menu forwards Claude's native file and
  conversation rewind controls. Only after the provider confirms the operation
  does Tcode append a rewind event; the folded timeline then hides the rewound
  turns. Claude's conversation prefill is placed in the normal composer. File
  coverage follows Claude Code's own checkpoint semantics (direct file-edit
  tools, not arbitrary external filesystem writes). Codex currently exposes
  only a deprecated conversation-only `thread/rollback`, so Tcode intentionally
  offers no Codex rewind action until a stable native capability can express the
  requested semantics.
- **Errors are never truncated or folded away.** A provider/app error renders as
  its own block: a danger-tinted card (10px radius, danger border at 35%, danger
  bg at 6%) with an uppercase 11px ERROR label, a Copy button, and the FULL
  message wrapped at 13px/20px. Errors deliberately do not join the Work Log's
  activity rows, which are ellipsized and collapse when the turn ends.
  A failed provider start additionally leaves the unsent message in the
  queue strip (typed text is never destroyed by a dead process).
  When a Claude usage window is exhausted, the card adds a resume row: either a
  live reset countdown with Cancel, or a button to schedule the resume manually.
- Changed-file evidence sits in the flow as a quiet summary and clickable file
  chips, showing three files initially with a Show more/Show fewer control.
  Codex uses its replacement `turn/diff/updated` net snapshot; providers without
  that capability fold only successfully completed structured file edits and
  label the result **PARTIAL**. Neither path compares ambient workspace state,
  so external edits are never claimed by the turn. The evidence remains visible
  when activity details fold; desktop chips and View diff open the diff panel.
- Finished turn's bottom row keeps the muted local completion clock; when the
  turn has a trustworthy timestamped breakdown, it appends "Total", "AI
  thinking & response", and "Tool calls" durations via the row's existing
  middle-dot grammar, rolling hour-scale spans up to `Hh MMm SSs` so they stay
  readable. Turns with legacy or untrustworthy timestamps show just the bare
  clock.
- Floating "⌄ Scroll to end" pill when not at bottom.

### Composer

The composer holds the draft plus removable attachment, terminal-context and
review-comment chips. Its controls select the provider/model, model parameters,
approval mode and Build/Plan mode, subject to provider capabilities. Context
usage comes from the live session. Sending during a turn queues the message;
the secondary send action steers when the provider supports it. Stop interrupts
the current turn. Queue/steer guidance belongs in the send tooltip.

The checkout row below the desktop composer shows the working directory and
Git branch. Voice input is available on supported macOS 26 builds: live partial
text replaces its provisional range at the insertion anchor, final text commits
it, and stopping keeps the transcript without sending. Escape, submit and
thread changes also stop dictation. Compact clients hide this entry point.

A finalized, unresolved proposed plan adds a "Plan Ready" header with a dismiss
button and changes the empty primary action to Implement. In Plan mode any
sendable draft (text, images, terminal context, or review comments) uses Refine
and the refine placeholder; in Build mode the composer keeps its ordinary Send
affordance, because a typed message there is an ordinary build turn.
At compact widths the overflow Build/Plan row is interactive (it toggles like the
full-width chip and closes the popover); the permission row stays display-only,
since its full-width counterpart is an explicit picker.

Model picker popover: left rail = favorites star + provider
glyphs; search input; rows = model name (✓ current) + provider subtitle,
⌘1…⌘9 (macOS) / Ctrl+1…Ctrl+9 (Windows/Linux) chips, favorite star; footer note
when a live session will restart (via resume) on model change. Picking a
different provider on a thread with at least one
completed turn defers the switch until send. Send opens a “Conversation relay”
confirmation; confirming starts that provider fresh and sends a canonical
timeline transcript (project, original provider/model, turn messages, compact
work outcomes, and plan/todo state, capped at roughly 60k characters) plus the
new message. Later messages use the new provider's native cursor. Empty or
incomplete threads switch silently without a transcript.

Approval and user-input panels sit above the composer. Preserve the provider
request, available decisions, free-text answers and editor prefill; show the
full actionable detail. Approval actions include deny, allow, allow for the
thread and cancel the turn where supported.

Pi extension select, input, and editor dialogs surface through the native
user-input panel, including editor prefill in its free-text field.

### Diff panel

Right resizable split (default 560px, min 320px). Sidebar · chat · right panel
are **one** resizable group: nesting a second group inside the chat panel does not
shrink the chat — the right panel is painted over it and the timeline and composer
are clipped mid-word. The chat column reflows; it never clips.

The panel has expand/close controls, a diff-scope selector, unified/split
layout, wrap, whitespace-insensitive and invisibles toggles.

The body is a variable-height virtual GPUI list backed by a Zed-inspired
pipeline: full old/new texts use imara-diff histogram hunks, with patch parsing
as fallback. Word-level changed-token highlights layer over syntax runs;
collapsed gaps expand without re-diffing, and split rows pair by content.
Loading, highlighting, and row construction run on background executors, while
the render path constructs only visible file headers and rows.
Unified and split rows share syntax highlights and line-number drag selection.
Unified rows use two 44px gutters and a 2px change-color rail; each split cell
uses a 42px gutter without the rail. Both retain an 18px minimum row height.

Right-panel state (open/closed, Diff/Plan/Preview tab, expansion and selected
turn), each Preview WebView, and the bottom terminal workspace all belong to the
conversation destination rather than the shared window. Stored threads key by
session id; unsent drafts use their own session ids so two drafts in one
project keep separate state. Switching conversations moves
the live terminal workspace with its PTYs, scrollback, tabs, splits and attached
context. Because WebViews are native child overlays rather than GPUI scene
nodes, their visibility is synchronized directly from app state: closing
Preview, selecting Diff/Plan, switching conversations, opening the command
palette, or leaving Chat hides every WebView that no longer owns the panel.

### Settings (full-page route)

Settings uses a left navigation column and independently scrolling content.
Groups share the composer's opaque floating-card treatment. Rows pair a title
and description with a control; sparse groups use space, dense lists use inset
hairlines. Restore defaults requires confirmation.

Provider profiles expose only applicable options. Pi defaults to no tcode
permission extension; its Native approvals toggle enables the gate for
supervised and auto-accept-edits sessions. Without it those stored modes take
effect as Full access, while Read only uses pi's native tool filter. Trust
project extensions adds `--approve` at launch. Pi has no MCP client; explicitly
enabled orchestration or computer-use registrations produce an unavailable-tools
warning. Remote setup is documented in [remote work mode](remote.md), and
permissions in [computer use](computer-use.md).

Orchestrate uses one provider-neutral workflow, refreshed on each explicit
`/orchestrate` message. The main thread frames and decides, routes concrete work
to execution models across the enabled provider fleet, and independently accepts
or rejects the actual integrated result. Small tasks reduce coordination overhead,
not execution ownership. Work that can advance concurrently is routed according
to task dependencies; the main thread integrates parallel deliverables, resolves
conflicts, and retains final acceptance of the integrated result. Optional peer
discussion remains separate from execution. A child report informs the main
thread's judgment but is not itself acceptance; the main thread retains discretion
over proportionate verification.

Settings show two model lists: **Collaboration models**, bundled
with GPT-6 Astra and Claude Fable 5.1, and **Execution models**, bundled with
GPT-5.6 Sol and Claude Opus 5. Other models may still initiate `/orchestrate`.
`collaborate` opens a read-only peer discussion, continued through `send`;
`dispatch` assigns concrete work to execution models. Model selection considers
the whole cross-provider fleet, preferring tcode Orchestrate to native subagents.
Each collaboration model can be switched on or off independently. Its switch
controls whether it can be invited through `collaborate`, never whether it may
serve as the main decision model. Turning every peer off still permits the main
thread to use `/orchestrate` and dispatch execution work. Status chips and switch
tooltips explicitly name collaboration to make this distinction visible.

Each provider/model ID occurs once across both lists, regardless of endpoint.
Add pickers exclude configured models and settings patches enforce uniqueness.
Each row has an editable description, enable switch, restore/delete actions,
a read-only list of available reasoning efforts, and a Fast switch when supported
(or when a stored value needs to remain visible). Effort is selected per tool call
from the live provider catalog, with bundled startup fallbacks. There is no saved
fixed-effort field. Collaboration is limited to medium/high; omitted effort uses
medium when available. Sol's description recommends medium for routine execution,
high/xhigh as difficulty grows, and max for the hardest well-defined problems.

The main workflow has no self-concept. Peer descriptions contain their collaboration
self-concepts: the main thread sees only other peers, and a consulted peer receives
its own description with the discussion brief. These texts emphasize complementary
perspectives, useful initiative within scope, and proportionate verification.

Both add-model popovers reuse the provider/model picker with fixed tabs and a
300px scrollable model list.

### Command palette (⌘K on macOS, Ctrl+K on Windows/Linux)

Centered top-anchored modal over a dim backdrop: search input; grouped results
— Actions (new thread per project, open settings, toggle theme, toggle diff
panel) and Threads (fuzzy over titles); footer key hints (↑↓ Navigate · Enter
Select · Esc Close).

### Session lifetime

Navigating away from a thread must not cancel its running turn, queued messages
or provider background tasks. Events continue to reach its stored timeline and
sidebar status; returning adopts the resident session. Idle providers may be
retained briefly and reclaimed by the runtime's idle grace period and LRU bound.
That resource policy must not reap sessions with work still in flight.

Dedicated worktree sessions normally live under `~/.tcode/worktrees/<session-id>`;
`TCODE_WORKTREES_DIR` overrides that root for isolated runs. Startup cleanup
removes registered orphan worktrees only after their minimum age, preserving
fresh entries, unknown directories and paths it cannot safely inspect. Projects
may place a `.worktreeinclude` at their repository root to copy required ignored files or directories into each
new worktree. Entries are relative paths, one per line; blank lines and `#`
comments are ignored. Copies never overwrite files Git materialized and stop at
an aggregate 512 MiB limit. The list is entirely user-controlled: including
`.env` files or other credentials copies those secrets into
the worktree directory, where they remain until the worktree is removed.

A clean worktree session can merge its committed branch back into the clean,
branch-attached original checkout. Descendants fast-forward; divergent history
uses a merge commit, with conflicts aborted for manual resolution. Worktrees are
never auto-committed or removed by merge-back. Orchestration can opt children
into the same worktree/session-metadata path globally or per dispatch; non-Git
cwd and creation failures fall back to the resolved cwd and are reported in the
dispatch response.

### Empty state

Centered "Pick a thread to continue" (20px semibold) over "Select an existing
thread or create a new one to get started." (14px muted). No composer rendered.

## Accessibility

Keyboard focus uses one quiet, keyboard-only outline across raw controls: a
2px outer ring derived from the theme ring token, with theme-specific opacity
so it remains legible over both paper and carbon surfaces without shifting
layout. Component-library controls retain their native focus treatment. Hidden
row actions must enter the normal tab order and reveal themselves when focused,
not depend on pointer hover.

Interactive surfaces expose the semantic role that matches their behavior
(button, tab, switch, menu item, option, or terminal) and a localized accessible
name. Selection, expansion, and toggle state are reported on the owning control.
Composite menus and listboxes keep keyboard focus in their input or container,
use menu-item/option descendants, and report the highlighted descendant rather
than adding every result to the global tab sequence.

## Verification protocol

Use the validation commands in [CONTRIBUTING.md](../CONTRIBUTING.md) and its
linked CI workflow. For visual changes, also launch the affected surface and
review both themes at its normal and narrow widths. Exercise keyboard focus,
scrolling and the changed interaction. Capture the relevant states for the PR;
unit or compile checks alone do not establish visual correctness.
