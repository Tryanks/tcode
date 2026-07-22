# tcode design spec

The visual contract for tcode's UI, closely modeled on
[T3 Code](https://t3.gg)'s design (see the acknowledgment in the README). When
code and this doc disagree, fix one of them — deliberately.

## Design tokens

Fonts:
- UI: **DM Sans** (bundled, OFL) → fallback -apple-system, system-ui
- Mono: SF Mono → SFMono-Regular, JetBrains Mono, Menlo (system; not bundled)

Radius base: 10px. Buttons/chips ~8px, cards ~10-12px, composer 16px, circular
send button fully round.

Light theme:
| token | value |
|---|---|
| background | #ffffff |
| foreground | #262626 |
| primary | #1447e6 |
| primary-fg | #ffffff |
| muted bg | rgba(0,0,0,0.04) |
| muted-fg | #686868 |
| accent (hover) | rgba(0,0,0,0.04) |
| border | rgba(0,0,0,0.08) |
| destructive | #ef4444 / fg #b91c1c |
| success | #10b981 / fg #047857 |

Dark theme:
| token | value |
|---|---|
| background | #161616 |
| foreground | #f5f5f5 |
| primary | #155dfc |
| muted bg | rgba(255,255,255,0.04) |
| muted-fg | #818181 |
| accent (hover) | rgba(255,255,255,0.04) |
| border | rgba(255,255,255,0.06) |
| destructive | #fb414a / fg #f87171 |
| success | #10b981 / fg #34d399 |

Diff colors: added rows get a low-alpha success tint with a solid success left
accent bar; removed rows the destructive equivalent; +N / -M counts render in
success-fg / destructive-fg.

Canonical values live in `themes/tcode.json` (embedded at build time).

## Window material

The persistent main window uses the native long-lived-window material: macOS
uses its existing blurred vibrancy, while Windows 11 uses base Mica
(`WindowBackgroundAppearance::MicaBackdrop`, `DWMSBT_MAINWINDOW`). Acrylic is
reserved for transient surfaces such as menus and popovers, not the main
window. Both native materials retain the embedded theme's translucent canvas so
the system backdrop can show through. `TCODE_NO_VIBRANCY=1` keeps its macOS-only
diagnostic behavior: an opaque window with a flattened canvas. Linux and other
platforms remain opaque and flatten that canvas to its solid RGB base.

## Layout metrics (at 1440×900)

- Sidebar 255px, resizable; 1px right border; collapses to a 48px icon strip.
- Window top is seamless: no app titlebar — the sidebar's first row (traffic
  lights inset 74px, collapse button, wordmark + channel pill) and the chat
  header (52px) form the top strip; both are window-drag areas.
- Chat content column: max-width 768px, centered, ≥24px horizontal padding
  (must reflow, never clip, when the diff panel narrows the chat region).
- Composer: floating card, radius 16, 1px border, subtle shadow; bottom control
  row ≈44px.
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
1. App row: collapse icon button, "tcode" bold 14px, channel pill ("DEV").
2. Search row: magnifier + "Search" muted + ⌘K kbd chip → opens the palette.
3. "PROJECTS" header: 11px uppercase muted + sort (no-op) + add-project button
   (native directory picker).
4. Project groups: rotating chevron + folder icon + 13px medium name; hover
   shows "+" (new thread in project); collapse state persisted.
   Thread rows: single-line truncated AI-generated title (first-message fallback
   while naming) + relative time (muted 11px); hover = accent bg. Inline rename
   commits on Enter and cancels on blur or any click outside the input.
   and time swaps to archive icon; active = persistent accent bg; a running
   session shows "● Working" (green, 11px) left of the title; >6 threads →
   "Show more" / "Show less" toggle row (the row remains available after
   expansion so the list can be collapsed again).
5. Footer: gear + "Settings" → settings route.

### Chat header
52px; thread title 16px medium left ("No active thread" muted when empty);
right: two icon buttons (layout placeholder · diff-panel toggle).

### Timeline
- **Turn separation is rhythm, not rules.** Turns are 44px apart; inside a turn
  blocks sit 16px apart. There is deliberately **no divider/hairline under the
  user bubble** — the eye separates turns by the space around them and by the
  typographic step down from the 15px bubble to the muted activity summary.
- Turn activity = collapsible "Work Log" sections: an expanded section starts
  with an 11px uppercase muted label, followed by activity rows (muted ✓ +
  one-line summary; command/tool/subagent/reasoning); >2 rows → last 2 +
  "+N previous log entries" expander. Once expanded, that row becomes "Hide N
  previous log entries" with an upward chevron so the rows can be collapsed
  again. A completed section's toggle summarizes only its real, nonzero events
  (commands, unique edited files, tool calls, subagents, and compactions); an
  earlier section uses its own counts and the final section uses turn-wide
  counts, prefixed once with Chinese “共” to make the aggregate scope explicit.
  A zero-event summary is omitted. The active section stays expanded with "•••
  Working for Ns" ticking.
- Assistant markdown 15px, relaxed line-height, inline code chips (mono 13,
  muted bg, 4px radius). Streaming appends via push_str with
  follow-when-near-bottom.
- User messages: right-aligned bubble, muted bg, radius 12, max-width ~70%.
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
  keeps the 44px/16px turn rhythm. Message actions follow the split: the
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
  activity rows — those are one-line ellipsized and collapse when the turn ends,
  which is exactly how T3 Code ends up showing "Request was abo…" and then
  nothing. A failed provider start additionally leaves the unsent message in the
  queue strip (typed text is never destroyed by a dead process).
- CHANGED FILES card per turn with provider-attributed file changes: Codex uses
  its replacement `turn/diff/updated` net snapshot; providers without that
  capability fold only successfully completed structured file-edit operations
  and label the result **PARTIAL**. Neither path compares ambient workspace
  state, so external edits are never claimed by the turn. Header "CHANGED FILES
  (N) · +A -D" + "Collapse all" ghost + "View diff" bordered button; body =
  directory tree, file rows with right-aligned per-file +a/-d; paths relative to
  the session cwd.
- Small muted local-time row after each finished turn.
- Floating "⌄ Scroll to end" pill when not at bottom.

### Composer
Floating card; placeholder "Ask anything, @tag files/folders, $use skills, or
/ for commands". Control row: provider glyph + model name + chevron (model
picker popover) · divider · context chip ("42k / 200k" from live token usage)
· lock + "Ask to edit" (static; permission profiles not yet a feature) · box +
"Build" (static) · spacer · running: blue spinner + circular stop button; idle:
circular send button (primary bg when input non-empty). Below the card: folder
icon + "Local checkout" left, branch icon + current git branch right (hidden
outside a git repo).

Model picker popover (~360px, radius 12): left rail = favorites star + provider
glyphs; search input; rows = model name (✓ current) + provider subtitle, ⌘1…⌘9
chips, favorite star; footer note when a live session will restart (via resume)
on model change. Picking a different provider on a thread with at least one
completed turn defers the switch until send. Send opens a “Conversation relay”
confirmation; confirming starts that provider fresh and sends a canonical
timeline transcript (project, original provider/model, turn messages, compact
work outcomes, and plan/todo state, capped at roughly 60k characters) plus the
new message. Later messages use the new provider's native cursor. Empty or
incomplete threads switch silently without a transcript.

Approval panel (above composer): "PENDING APPROVAL" label, summary + count,
expandable detail (command text / file list), actions Deny / Always allow /
Approve (primary).

### Diff panel
Right resizable split (default 560px, min 320px). Sidebar · chat · right panel
are **one** resizable group: nesting a second group inside the chat panel does not
shrink the chat — the right panel is painted over it and the timeline and composer
are clipped mid-word. The chat column reflows; it never clips.

Details: tab strip ("Diff" + "+" no-op) with expand/close cluster; toolbar
"Turn N ⌄" selector, unified/split layout, and wrap toggle (+ no-op
whitespace/¶ icons).

The body is a variable-height virtual GPUI list. Git loading, patch parsing,
syntax highlighting, and row model construction run on background executors;
the render path constructs only
the visible file headers and rows. Each file header shows icon, relative path,
operation badge, and +N/-M totals. Rows retain dual line-number gutters, mono
content, syntax/novel-span highlighting, add/remove tints and accent bars, soft
wrapping, split pairing, selection, and inline review comments.

Right-panel state (open/closed, Diff/Plan/Preview tab, expansion and selected
turn), each Preview WebView, and the bottom terminal workspace all belong to the
conversation destination rather than the shared window. Stored threads key by
session id; an unsent New thread surface keys by project, matching the composer
draft cache despite its transient session ids. Switching conversations moves
the live terminal workspace with its PTYs, scrollback, tabs, splits and attached
context. Because WebViews are native child overlays rather than GPUI scene
nodes, their visibility is synchronized directly from app state: closing
Preview, selecting Diff/Plan, switching conversations, opening the command
palette, or leaving Chat hides every WebView that no longer owns the panel.

### Settings (full-page route)
Left nav (sidebar width): General / Providers / Orchestrate + "← Back" pinned
bottom. Header: "Settings" + "Restore defaults" bordered button (confirm).
Groups are **floating cards** in chat's composer-console idiom, not flat
System-Settings boxes (`material::floating_card`: popover fill, hairline border,
radius 12, subtle `shadow_md`); the 768px content column and window material
(T0 blur + translucent paper) are identical to chat, so navigating in/out never
flips the material. Rows: bold 15px title + 13px muted description left, control
right (dropdown / toggle / text input). Sparse surfaces separate rows with
breathing room and no rules; only dense lists (Providers, Orchestrate) carry the
faintest inset hairline. General: Language, Theme
(System/Light/Dark, live), thread-title provider/model, Word wrap in diffs,
Delete confirmation, task-panel behavior, and provider update checks. The title
model defaults to Codex `gpt-5.6-luna`; its isolated background request always
uses `low` reasoning effort. Providers: Claude / Codex / pi / OpenCode
configuration.

Orchestrate begins with an explicit built-in `/orchestrate` explanation. Every
main model is eligible: the page exposes one multiline generic identity plus
optional per-model multiline identity overrides, and models without an override
inherit the generic text. Each editor has a compact "Restore default" action.
Allowed child models are retained as provider/model profiles with one multiline
routing-definition editor, an independent dispatch switch, restore and delete
actions. Built-in ratings and recommended effort live inside the default text,
not separate controls. Add-model popovers keep provider tabs fixed above a
300px scrollable model list so large catalogs never grow past the viewport.
That provider/model picker is one shared component also used by the General
page's thread-title setting, so catalog resolution and provider switching stay
identical across both settings surfaces.

### Command palette (⌘K)
Centered top-anchored modal over a dim backdrop: search input; grouped results
— Actions (new thread per project, open settings, toggle theme, toggle diff
panel) and Threads (fuzzy over titles); footer key hints (↑↓ Navigate · Enter
Select · Esc Close).

### Session lifetime
A working session survives everything except an explicit stop. Switching
threads (or opening a draft) parks a session that still has work — running turn
or queued messages — instead of killing its provider: the process, event pump
and queue stay alive in the background, its events keep landing in the JSONL,
queued messages keep dispatching as turns complete, and its sidebar row keeps
the green "● Working" dot. Selecting the thread re-adopts the live session
seamlessly (timeline replayed from the JSONL, which stayed current). When a
parked session runs out of work it shuts down for real. There is **no idle
reaper and no timer** — T3 Code hard-kills provider processes after 30 minutes
without a user message, which silently destroys autonomous overnight sessions;
tcode's rule is "finish what you were given, then rest".

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

For any visual change: `cargo build` (zero warnings) + `cargo test --workspace`
+ a headless smoke (`--smoke "claude|<tmpdir>|..."`), then launch with
`--open-latest` (optionally `--open-diff` / `--open-settings` /
`--open-palette`), capture the window (`tools/windowid.c` helper +
`screencapture -x -l<id>`), and review both themes against this spec.
