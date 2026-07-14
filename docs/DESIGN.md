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

## Surface anatomy

### Sidebar
1. App row: collapse icon button, "tcode" bold 14px, channel pill ("DEV").
2. Search row: magnifier + "Search" muted + ⌘K kbd chip → opens the palette.
3. "PROJECTS" header: 11px uppercase muted + sort (no-op) + add-project button
   (native directory picker).
4. Project groups: rotating chevron + folder icon + 13px medium name; hover
   shows "+" (new thread in project); collapse state persisted.
   Thread rows: single-line truncated title + relative time (muted 11px); hover = accent bg
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
  typographic step down from the 15px bubble to the 11px uppercase "WORK LOG"
  label that opens the turn's activity.
- Turn = "Work Log" section: 11px uppercase muted label; activity rows (muted ✓
  + one-line summary; command/file/tool/reasoning); >2 rows → last 2 +
  "+N previous log entries" expander. Once expanded, that row becomes "Hide N
  previous log entries" with an upward chevron so the rows can be collapsed
  again. Footer "Worked for XmYYs ›" is collapsed by default when finished and
  expanded live with "••• Working for Ns" ticking.
- Assistant markdown 15px, relaxed line-height, inline code chips (mono 13,
  muted bg, 4px radius). Streaming appends via push_str with
  follow-when-near-bottom.
- User messages: right-aligned bubble, muted bg, radius 12, max-width ~70%.
- **Message actions.** Every message reserves a 24px action row under it (the
  height is always taken, so revealing it never shifts the timeline). It is
  hidden until the message is hovered — except on the newest user and newest
  assistant message, where it stays visible so the actions are reachable without
  hovering. Ghost xsmall buttons, icon + label:
  - user bubble (right-aligned row): **Copy** · **Edit** · **Revert** — Edit and
    Revert only on the message that *opened* the turn (a steered message joins a
    turn already in flight and carries Copy alone), Revert only when the turn has
    a git checkpoint. Both are disabled with an explaining tooltip while a turn
    runs.
  - assistant message (left-aligned row): **Copy**.
  - Copy puts the message's **raw text** (the markdown source, not the rendered
    document) on the clipboard and flips to "Copied!" for 2s.
- **Edit & resend.** Edit replaces the bubble with an inline multi-line editor
  (primary-bordered card, seeded with the original text; Enter resends, Esc
  cancels; explicit Cancel / Resend buttons and a muted hint row). Resending
  rewinds the conversation to the state just before that message — the turn's git
  checkpoint restores the worktree, the JSONL log is truncated at the message and
  the provider session is rolled back to Idle (the same single mechanism Revert
  uses) — and then sends the edited text as a fresh turn. Without a checkpoint
  (e.g. a non-git cwd) the transcript is still truncated and the message resent,
  and a toast says plainly that files on disk were not reverted.
- **Errors are never truncated or folded away.** A provider/app error renders as
  its own block: a danger-tinted card (10px radius, danger border at 35%, danger
  bg at 6%) with an uppercase 11px ERROR label, a Copy button, and the FULL
  message wrapped at 13px/20px. Errors deliberately do not join the Work Log's
  activity rows — those are one-line ellipsized and collapse when the turn ends,
  which is exactly how T3 Code ends up showing "Request was abo…" and then
  nothing. A failed provider start additionally leaves the unsent message in the
  queue strip (typed text is never destroyed by a dead process).
- CHANGED FILES card per turn with file changes: header "CHANGED FILES (N) ·
  +A -D" + "Collapse all" ghost + "View diff" bordered button; body = directory
  tree, file rows with right-aligned per-file +a/-d; paths relative to the
  session cwd.
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
on model change.

Approval panel (above composer): "PENDING APPROVAL" label, summary + count,
expandable detail (command text / file list), actions Deny / Always allow /
Approve (primary).

### Diff panel
Right resizable split (default 560px, min 320px). Sidebar · chat · right panel
are **one** resizable group: nesting a second group inside the chat panel does not
shrink the chat — the right panel is painted over it and the timeline and composer
are clipped mid-word. The chat column reflows; it never clips.

Details: tab strip ("Diff" + "+"
no-op) with expand/close cluster; toolbar "Turn N ⌄" selector + wrap toggle
(+ no-op split/whitespace/¶ icons); body per file: header row (icon, relative
path, "new" badge for creates, +N/-M) then unified diff: dual line-number
gutters (11px mono muted), 12px mono content, syntax highlighting by extension,
add/remove row tints + left accent bars, "N unmodified lines" muted separator
rows between hunks.

### Settings (full-page route)
Left nav (sidebar width): General / Providers + "← Back" pinned bottom. Header:
"Settings" + "Restore defaults" bordered button (confirm). Rows: bold 14px
title + 13px muted description left, control right (dropdown / toggle / text
input), hairline separators. General: Theme (System/Light/Dark, live), Word
wrap in diffs, Delete confirmation. Providers: claude / codex binary paths.

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

## Verification protocol

For any visual change: `cargo build` (zero warnings) + `cargo test --workspace`
+ a headless smoke (`--smoke "claude|<tmpdir>|..."`), then launch with
`--open-latest` (optionally `--open-diff` / `--open-settings` /
`--open-palette`), capture the window (`tools/windowid.c` helper +
`screencapture -x -l<id>`), and review both themes against this spec.
