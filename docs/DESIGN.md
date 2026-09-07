# tcode design spec

The visual and interaction contract for tcode. Update it deliberately when a
product decision changes; historical design drafts are not additional rules.
There is one shell, described here; **Compact layout** below covers the narrow
end of it.

## One shell, one layout rule

Desktop, iOS, Android and the browser run the same shell. It has exactly one
layout rule:

> **Compact iff the available logical content width — the viewport width minus
> whatever the system occludes on its left and right — is under 900px.** At
> exactly 900 the layout is wide.

Nothing else decides it. Not the operating system, not the input device, not a
saved preference: a desktop window dragged narrow is compact, an iPad in
landscape is wide, and rotating a phone changes the layout the same way
dragging a window edge does. The rule is never persisted, because a window
width is not a setting.

Input-device behavior is a separate question with a separate answer. Whether
Enter submits follows the *keyboard*, not the width: a wide tablet still types
on glass, and a narrow desktop window still has a hardware Enter key.

Crossing the breakpoint is a layout change and nothing else. It never detaches
from the host, never reconnects, and never rebuilds a view that holds user
state. The selected thread, the composer draft and its selection, pending
attachments, approvals, scroll position and focus all survive in both
directions, and the sidebar and right-panel widths come back as they were left
when the window widens again.

## Capability-appropriate UI

Every product view is built by every client. A view is never compiled away
because of the platform it happens to run on: the difference between clients is
which *operations* they can perform, not which screens exist.

An operation is gated only when it is genuinely native — a file dialog, an
embedded webview, macOS permission grants, dictation, the AppKit pasteboard,
launching an editor — and then by two independent things: whether this build has
the capability at all, and whether it applies to the current attachment. A
picker that browses this machine is meaningless while the workspace runs on
another one, so it is withheld even where the build has it.

Where an operation is unavailable, the view says which machine can perform it
and offers what it can (open externally, copy, type the value) instead of a
control that would do nothing. A client never infers the host's state from its
own operating system, and never reports a success it did not perform.

## Design tokens

The embedded [theme](../themes/tcode.json) owns colors and font choices;
[material.rs](../crates/ui/src/material.rs) owns surface treatments, shared
geometry and radii. Use those definitions rather than maintaining a second
palette in documentation.

DM Sans is bundled for UI text and comes from the same font resource on every
client. Desktop monospace text uses the configured system family; Android
registers the bundled Lilex in its place and adds the packaged Noto Color Emoji
fallback. Native debug builds embed fonts and SVGs in the package rather than
reading a development machine's asset path.
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

Those heights are desktop design sizes, and the same views open at every width.
Each is capped to what the window can actually show, so a short window scrolls
the list instead of pushing the footer off-screen. Dialogs are capped the same
way — width and height — because a dialog wider than the viewport is also
positioned off-centre.

## Compact layout

### Destinations

Compact replaces the split with a navigation stack over one history:

**Machines → Threads → Thread → Panel**, plus **Add a machine** over Machines and
**Settings → Settings section** over wherever they were opened from, pushed and
popped with a 200ms lateral transition.

- **Machines** — which machine this window talks to, and nothing else: **This
  machine** where the device has one, the added machines, one **Add a machine**
  button and the machines found nearby. It is the root of a window with no
  attachment, and it can also be *visited* from an attached workspace through
  the sidebar's feature area (see **Sidebar**) without leaving that machine.
  Letting other devices connect to this machine is a setting of this machine
  and lives in **Settings → Other devices**, never here.
- **Add a machine** — the connection form, pushed by **Add a machine**, by a
  **Nearby machines** row that prefilled an endpoint, or by a
  certificate-changed row's **Add again**.
  Labels sit above full-width fields, the primary action is pinned at the foot
  of the page above the keyboard, and adding ends on an explicit security ID
  comparison before anything connects.
- **Threads** — the shared sidebar under a nav bar titled **Threads**, with the
  attached machine's name as its subtitle and new-thread and settings actions. New thread starts a draft directly when
  the machine has one project and otherwise opens the command palette, which
  already owns "new thread in ‹project›" and can search.
- **Thread** — the shared chat view. Its desktop header is replaced by the nav
  bar; the timeline, composer, approvals and user-input panels are the same
  entities the wide layout uses.
- **Panel** — the terminal, diff, plan and preview at full width, chosen from a
  segmented control. They are the same entities the wide layout puts in the
  split: a compact window has less room, not less product. Export and the
  per-thread actions stay on the thread row's context menu.
  The segmented control is the page's **only** selector, so a panel does not
  also draw its own tab row, and it drops the right column's expand / split /
  close controls — Back is how you leave the page. What remains is one toolbar
  row on the page inset with 44pt touch targets: the diff's source and base
  pickers, the terminal's tab strip and a new-terminal target, the preview's
  address field. Everything else moves into that row's overflow menu.
- **Settings** and **Settings section** — the section list, then one section's
  detail (see **Settings** below).

**One navigation bar.** Every compact page — Settings included — is composed as
the same 52pt nav bar: Back at the top left, a centered title that truncates
rather than colliding with its controls, an optional muted subtitle under it,
and at most two trailing actions. No page draws a header of its own, and
nothing anywhere puts a Back control at the bottom of a list.

**Back is labelled with a place, not a title.** Each destination owns one short
fixed label — Machines, Threads, Thread, Panels, Settings — and a Back control
carries the label of the destination it returns to (Add a machine answers with
its caller's, Machines; a settings section returns to Settings). Dynamic titles
never reach a Back control, so it never truncates and never changes width when
a machine or thread is renamed. The nav bar reserves the same room on both
sides at every page, so the centered title does not shift between pages.

Back — the Android system gesture and every Back control — unwinds in one order:
the software keyboard or composition, then the topmost dismissible overlay
(dialog, menu, palette), then one entry of the window's history. It reports "not
consumed" only at the root, where the platform closes the app. Non-dismissible
dialogs, such as import progress and approval prompts, keep refusing dismissal.

**Navigating never detaches.** Walking back from Threads to Machines, or
visiting Machines from an open thread, keeps the link, the selected thread and
every view over that workspace. Only two things detach: connecting to a
*different* machine, which clears the previous machine's history and lands on
the new machine's threads, and an explicit **Disconnect** in a machine row's
own menu. **Remove** removes the saved credential and leaves a live attachment
running. Resizing never detaches and keeps the page the window is on.

### The window seam

A window is not always the rectangle it reports: a status bar, a notch, a home
indicator or a software keyboard can cover part of it. Those edges belong to the
*window*, not to the host the workspace is attached to.

There is one safe content rectangle, shared by pages, the palette, dialogs and
toasts. Bottom avoidance is **max(safe area, keyboard)**, never their sum — a
keyboard that already covers the home indicator does not need it counted twice.
Backgrounds paint edge to edge; only interactive content is constrained, and
only once. The browser canvas is already resized around its on-screen keyboard,
so the client adds no inset of its own there and takes its size from the actual
canvas.

Insets change without a timer: the platform schedules a frame when they move, so
the layout follows the keyboard immediately.

### Touch and typography

- Pages inset 16pt left and right; nav bars are 52pt plus the top safe area.
  Icon buttons have a 44×44pt touch target and use the shared stroke icons in
  the foreground color.
- The compact composer's radius is 16pt. Other shared components keep their own
  material radii.
- Empty states are an icon, a title, a short explanation and any necessary
  primary action, centered and width-limited; they show no desktop shortcuts.
- Nothing depends on hover. Long text, model lists and variable-length option
  sets wrap or scroll rather than truncating a choice away.
- Compact approval cards default to expanded, with deny and allow on one row and
  every other available action on its own; user questions keep their options,
  free text and editor prefill.

### The compact inset rule

One inset, applied once, at the page:

- **Page inset 16pt** left and right. Every compact page's content — the diff,
  plan/tasks and preview bodies included — starts and ends there. A view that
  the wide layout draws in the right column does not get to keep the column's
  denser padding when it becomes a page.
- **Card inset 12pt.** A card, notice, chip or row *inside* that content pads a
  further 12pt; it never re-applies the page inset.
- **Terminal exception.** The terminal grid stays edge to edge horizontally —
  it is measured in columns, and narrowing it drops columns — but it still sits
  inside the window's safe rect and keeps 8pt of air below the segmented control.
  On iOS and Android, focusing the grid raises the software keyboard and adds one
  44pt special-key row at the bottom of that rect, directly above the keyboard.
  The row reserves its height before the grid is measured, so it never covers the
  last terminal row. It contains Esc, Tab, sticky Ctrl and Alt, four arrows, then
  a horizontally scrolling `- / | ~` tail; sticky modifiers highlight until the
  next terminal key or committed character consumes them. Desktop and browser
  terminals never draw the row.

Prose inside the content (errors, notices, file headers) wraps against the page
inset rather than running past it. Code and diff lines do not wrap: they scroll
horizontally *inside* the body, so the page edge stays where it is.

A row that pairs a label and description with a control follows the same rule as
Settings: on a compact page the control moves to its own full-width line under
the text, so the description is never squeezed into a column a word wide. A
fixed 44pt affordance — a switch — is the exception and stays beside its label.

### One list style

Two kinds of list, one rule for which is which:

- **Navigable content lists** — threads, machines, nearby machines, projects —
  are **plain rows**: no card, no border, no radius. Each row is at least 56pt
  tall at the 16pt page inset, separated from the next by a hairline indented
  to that inset, with a hover fill on a pointer and a pressed tint everywhere.
  Sections carry a caption in one shared style; a project header is that
  caption plus its collapse affordance. A row's own overflow trigger (the
  machine row's "…") keeps its 44×44 hit region *inside* the row, so the row's
  hover fill covers the whole row rather than stopping short of a seam.
- **Settings-like forms** — the Settings sections, **Other devices**, **Add a
  machine** — keep the grouped floating card with inset hairlines between rows.

So Machines and Threads read as one family of pages, and neither reads as a
settings page.

### Connection states

| State | Presentation |
| --- | --- |
| Connecting | The initial index has not arrived; the thread list shows a loading skeleton |
| Connected | Index ready and the link healthy; no status banner |
| Reconnecting | Content is kept; the banner names the retry attempt |
| Offline | Shown after 30 seconds disconnected; transient failures keep retrying quietly |
| Certificate changed | An explicit error and an **Add again** entry point outrank every other status; native clients stop retrying that identity |

Offline keeps the last received replica readable and disables writes; visited
threads keep their events, and unvisited ones may have only their list summary.
Reconnecting resubscribes to the current thread. A native app returning to the
foreground, or a browser page becoming visible again, interrupts the backoff and
retries at once.

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
3. Feature area: the window's persistent entries, directly under the search
   field at both widths, `flex_none` and outside the thread list's scrolling and
   filtering. Each entry is one sidebar-sized row — leading stroke icon, label,
   a muted trailing value and, where it has one, a status glyph — and it takes
   the selected surface while its destination is showing. Compact rows are 44pt
   for touch. Today it holds one entry, **Machines**, whose trailing value is the
   attached machine's name (or "Not connected") with the connection glyph; it
   navigates to `Destination::Hosts` without disturbing the attachment. Later
   persistent features are rows here, not new controls elsewhere.
4. Project/thread header: sort, grouped/flat layout and add-project controls.
   Sorting and layout choices are persisted.
5. Project groups: rotating chevron + folder icon + 13px medium name; hover
   shows "+" (new thread in project); collapse state persisted.
   Thread rows: single-line truncated AI-generated title (first-message fallback
   while naming) + relative time (muted 11px); hover = accent bg. Inline rename
   commits on Enter and cancels on blur or any click outside the input.
   On hover, time swaps to the archive icon; active = persistent accent bg; a running
   session shows "● Working" (green, 11px) left of the title; >6 threads →
   "Show more" / "Show less" toggle row (the row remains available after
   expansion so the list can be collapsed again).
6. Footer: gear + "Settings" → settings route. Like a feature-area entry, it
   takes the selected surface while that route is showing.

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

The terminal is host state, not a client's own emulator. The host owns the
grid and publishes it — cells, cursor, modes and scrollback — so every attached
client renders the same screen, and a client that attaches mid-session sees
exactly what the host sees. Scrolling and selection are local to each viewer:
two clients read different parts of the same terminal without disturbing each
other, and only the keys and mouse reports a client sends reach the shell.
Keyboard input is encoded from the replicated modes, so bracketed paste,
application cursor keys and mouse reporting behave the same everywhere. In-grid
images are not supported: the terminal is a coding tool's terminal, and it
renders text.

A stored command's output in the chat timeline is the same grid, rendered on
demand. The client measures the width it can show and asks the host for that
item at that many columns; the host replays the captured bytes through its own
emulator and answers with one finished screen. The panel shows the plain text
until the answer arrives, keeps the answer per width, and asks again once a
width change settles.

The Preview tab exists on every client. Its URL field, open-in-system-browser
and copy-URL work everywhere; the embedded browser, history, JS automation and
screenshots need a system webview, which only macOS and Windows desktop builds
have. Without one the tab explains that and offers the portable actions rather
than dead back/reload/screenshot controls, agent automation requests are
answered with an explicit "unsupported" instead of timing out, and such a client
does not subscribe as an owner of the session's preview at all. Localhost port
discovery scans the client, so it is offered only for a local workspace; a host
URL typed into the field still works.

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

### Settings (wide route)

Settings replaces the **content column**, never the window: the workspace
sidebar — and the persistent feature area in it — stays beside it, exactly as it
does on the Machines route, and the sidebar's Settings entry takes the selected
surface while the route is showing. The way out is a Back control at the left of
the content header, labelled with the destination it returns to and consistent
with the Machines route's header (caption buttons included, where the platform
has them). The rail carries no back row of its own; nothing puts a Back control
at the foot of a list.

Settings uses a left navigation column and independently scrolling content.
Groups share the composer's opaque floating-card treatment. Rows pair a title
and description with a control; sparse groups use space, dense lists use inset
hairlines. Restore defaults requires confirmation. Compact clients have no room
for the rail beside the content: the same sections become a full-width list that
pushes to one section at a time, and both pages wear the shell's one nav bar —
Back to whatever Settings was opened from, then Back to the section list. The
compact page draws no header of its own.

**Compact rows stack.** Where a wide row puts its label left and its control
right, a compact row puts the label and description above a full-width control:
no fixed-width label column, prose that wraps rather than overflowing, and no
horizontal clipping. A row whose control is a 44pt switch keeps it beside the
label at both widths, since a switch never squeezes the text.

Every section is present on every client, including over a remote link. Computer
Use configuration is host settings and stays editable; only its **System
permissions** group is local — it shows live status and Grant/Recheck when this
build can read them *and* the workspace is this machine's, and otherwise says to
manage system permissions on the named host. A client never reports the host's
permission state from its own OS.

Editable fields are seeded from the host's settings the first time a real
snapshot exists, not from local defaults, and a field the user has since edited
is never rewritten by a later snapshot.

Provider profiles expose only applicable options. Pi defaults to no tcode
permission extension; its Native approvals toggle enables the gate for
supervised and auto-accept-edits sessions. Without it those stored modes take
effect as Full access, while Read only uses pi's native tool filter. Trust
project extensions adds `--approve` at launch. Pi has no MCP client; explicitly
enabled orchestration or computer-use registrations produce an unavailable-tools
warning. Using tcode from other devices is documented in
[Use tcode from other devices](remote.md), and
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
GPT-6 Astra and Claude Opus 5. The two Astra rows are separate role-specific
profiles with different descriptions. Other models may still initiate `/orchestrate`.
`collaborate` opens a read-only peer discussion, continued through `send`;
`dispatch` assigns concrete work to execution models. Model selection considers
the whole cross-provider fleet, preferring tcode Orchestrate to native subagents.
Each collaboration model can be switched on or off independently. Its switch
controls whether it can be invited through `collaborate`, never whether it may
serve as the main decision model. Turning every peer off still permits the main
thread to use `/orchestrate` and dispatch execution work. Status chips and switch
tooltips explicitly name collaboration to make this distinction visible.

Each provider/model ID occurs once per list, regardless of endpoint. The same ID
may have separate collaboration and execution profiles. Each add picker excludes
models configured in its own list, and settings patches enforce within-list uniqueness.
Each row has an editable description, enable switch, restore/delete actions,
a read-only list of available reasoning efforts, and a Fast switch when supported
(or when a stored value needs to remain visible). Effort is selected per tool call
from the live provider catalog, with bundled startup fallbacks. There is no saved
fixed-effort field. Collaboration is limited to medium/high; omitted effort uses
medium when available. The GPT-6 executor is dispatched at low only; higher efforts
are never used for it. Its description names computer use as a headline strength.
Fast mode remains independent.
Once a provider catalog is loaded, a configured model absent from it is rendered
unavailable with the catalog mismatch and dispatch or collaboration is rejected;
an empty pre-discovery catalog continues to use bundled fallbacks.

The main workflow has no self-concept. Peer descriptions contain their collaboration
self-concepts: the main thread sees only other peers, and a consulted peer receives
its own description with the discussion brief. These texts emphasize complementary
perspectives, useful initiative within scope, and proportionate verification.
Astra may use Computer Use in a collaboration thread to gather focused decision
evidence by observing and reading the app UI. It reports visible state, state ids,
read text, and discrepancies rather than treating observation as implementation.
It operates the UI only when the lead's brief explicitly requests it and the
thread's access mode permits it. Bulk UI sweeps and code changes remain execution
work for `dispatch`. Enabled Computer Use registrations are attached to child
threads, including collaboration children.

Both add-model popovers reuse the provider/model picker with fixed tabs and a
300px scrollable model list.

Theme, language and device name belong to the client. An explicit client choice
overrides the attached host's replicated setting; restoring that row reveals the
host setting again. Changing hosts replaces the workspace store, shell and all
descendant views in the same window. The local kernel and remote hosting controls
remain alive independently, so connecting to a host and returning to **This
machine** never relaunch the process and never interrupt other attached
clients.

**Settings → Other devices** is **Let other devices connect to this machine**
and nothing else: the listener, **Let nearby devices find this machine**,
connection codes and **Connected devices**. It needs a listener and a beacon,
so the section only exists where the client can host — a phone or a browser has
no such setting. Choosing which machine to talk to is a product surface, not a
setting: it lives in **Machines**, reached from the sidebar's feature area at
both widths. Adding a machine follows the same rules everywhere: a security ID
pinned by an invite or a discovery result applies
only to the endpoint it came from and is dropped if either field is edited; an
answer from a superseded attempt is discarded rather than applied; and a client
that can only reach the origin that served it (a browser) fixes the address and
port, hides discovery, and hides the camera scan unless it has one. A machine
whose certificate no longer matches the pinned one cannot be connected to —
the row offers **Add again** instead.

### Command palette (⌘K on macOS, Ctrl+K on Windows/Linux)

Centered top-anchored modal over a dim backdrop: search input; grouped results
— Actions (new thread per project, open settings, toggle theme, toggle diff
panel), Threads (fuzzy over titles) and Messages; footer key hints (↑↓ Navigate ·
Enter Select · Esc Close). A leading `>` restricts results to Actions.

Messages are full-text hits inside stored conversations, shown as the thread
title over the matching snippet; selecting one opens that thread at the hit's
turn. The search itself belongs to the host: it indexes its own session logs, in
its own index order, and clients send only the query text. Every client gets the
group, including compact ones — there is no local session store to reopen. The
client owns presentation only: a 150ms debounce and discarding an answer that a
newer keystroke has already superseded.

### Exporting a thread

The host renders the artifact — it owns the event log and flushes pending writes
first — and writes nothing. The client owns delivery, because "where does this
file go" is a question about the machine the user is at, which over a remote link
is not the host. The export dialog names the file and its size, and offers only
what this client can actually do: **Save** through the platform save panel,
**Download** where the platform has one (a browser Blob), and **Copy** to the
clipboard everywhere. A dismissed save panel is a decision, not a failure, and
reports nothing. An export too large for one response frame is refused with an
explicit size error rather than truncated.

### Importing external history

The project root in **Add project** belongs to the host: whether a path is
absolute and whether it exists are facts about the host's filesystem, so the host
decides and the dialog shows the host's own reason for refusing one. The native
directory picker browses *this* machine, so it appears only for a local
workspace; a remote one types a host path, with the recents list and the import
run also coming from the host. Failures — a refused path, an unreadable recents
scan, a refused import — are shown, never swallowed.

Choosing a directory in **Add project** starts an import and opens a modal,
non-dismissible progress dialog with a bar, the "n of N" line naming the tool
being read, and — once finished — an imported/skipped summary and an OK button.

Import progress is host state, not a client-side job. The host keeps the latest
run per project and publishes it, so closing the window, disconnecting, or
attaching a second client never abandons the run or loses its outcome: a client
that attaches after a fast completion still sees the summary. The imported
threads appear in the sidebar before the dialog reports the run finished. A
second import of the same project while one is running is refused rather than
queued.

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

The workspace does not sit on a blank page. When no conversation is open — at
launch, or because the thread on screen was archived or deleted — it opens the
new-thread draft of the project the user last interacted with, composer focused
and ready. "Last interacted" is set by user navigation only (opening a thread,
starting a draft); background model activity and archive timestamps never move
it, and it is persisted, so a launch lands where the user left off. A remembered
project that no longer exists falls back to the first project in the sidebar.
That project keeps a single standing draft, so returning to it preserves its
composer attachments.

A thread that is archived while on screen — an Orchestrate child auto-archived
on completion is the common case — hands the workspace to its parent when the
parent is still visible, with the parent's scroll position and panels intact.
Archiving a thread the user is not viewing changes nothing.

Only a workspace with no projects at all reaches the empty page: centered
"Add a project to get started" (15px semibold) over "tcode works inside a
project folder. Add one to open its first thread." (13px muted) and an
**Add project** button. No composer is rendered. The same page, titled "Pick a
thread to continue" over a list of recent projects, covers the moment before a
draft opens.

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
