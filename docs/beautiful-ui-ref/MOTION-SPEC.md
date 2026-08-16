# Beautiful UI Motion & Behavior Spec

Source of truth: the 19 sibling `.tsx` files plus `tokens-light.json` / `tokens-dark.json`. Values below describe the code as written, including places where comments and runtime behavior differ.

## Common patterns

- Time units: Tailwind `duration-100/150/200/250/300/400/500` = `100/150/200/250/300/400/500ms`; absent explicit easing uses `--default-transition-timing-function = cubic-bezier(.4,0,.2,1)`.
- Shared easings: `--ease-out = cubic-bezier(0,0,.2,1)`; `--ease-in-out = cubic-bezier(.4,0,.2,1)`; `--ease-out-strong = cubic-bezier(.23,1,.32,1)`. Literal alternatives used below: `cubic-bezier(0.16,1,0.3,1)`, `cubic-bezier(0.22,0.61,0.25,1)`, and `cubic-bezier(0.77,0,0.175,1)`. CSS `ease` = `cubic-bezier(.25,.1,.25,1)`.
- Shared named animations referenced by TSX: `fade-in` (opacity entrance), `fade-up` (opacity + upward-translate entrance), `pop-in` (opacity + scale entrance), `shimmer-text` (moving gradient background), `spin` (continuous rotation), `stream-in` (blur + opacity resolution), `pixel-on` (cell intensity pulse), and `eq-bounce` (bar-height pulse). Their keyframe bodies/endpoints are not included in these 21 reference files; do not invent endpoint distances/blur radii when porting. Durations/easings/delays are fully recorded below.
- Expand/collapse idiom: outer `display:grid`; animate `grid-template-rows: 0fr↔1fr` plus `opacity:0↔1`; inner child `overflow:hidden`. Most use `300–400ms` and strong-out easing. This avoids precomputing height.
- Measurement idiom: when geometry must follow content, `useLayoutEffect` reads `offsetHeight`, `offsetTop`, `getBoundingClientRect()`, or `scrollHeight` before paint; `ResizeObserver`/`requestAnimationFrame` are used where streaming reflows continuously.
- Palette semantics (both themes): `canvas` = app background; `surface` = raised cards/controls; `inset` = recessed subpanels; `field` = inputs/selected-neutral; `hover`/`hover-2` = interaction fills; `ink`, `ink-2`, `ink-3` = primary/secondary/muted text; `line`, `line-strong` = separators/control outlines; `accent`/`accent-ink`/`accent-tint` = primary action/link/soft selection; `green`, `orange`, `red` plus `*-tint` = success/warning/error. Code should use semantic tokens, not light-theme literals.
- Light/dark core colors: canvas `#f1f2f3/#1c1d1f`, surface `#fff/#232427`, field `#f2f2f3/#2b2c2f`, ink `#1f2124/#f2f3f4`, ink-2 `#62656b/#a5a8ad`, ink-3 `#9a9da3/#6c6f75`, line `#ecedef/#2e3033`, accent `#0285ff/#3d9aff`.
- Shared geometry: `rounded-chip=6px`, `rounded-control=8px`, `rounded-card=10px`; generic scale press is `0.96` (some composer controls use `0.94`, allocation segment `0.98`). Default font is Inter/UI sans; `font-mono` is configured mono/SF Mono. `--spacing=.25rem` (4px), so e.g. `size-7=28px`.
- Source limitation: custom classes such as `primitive-card-pad/bar/footer/table-cell`, `records-*`, chart tooltip classes, and named keyframe definitions have no CSS declarations in this reference directory. This spec records only exact facts present in the supplied sources.

### GPUI translation rules

- Treat React state changes as discrete transition triggers; a CSS transition interpolates only properties listed in `transition-*`, while unlisted properties snap on the same frame.
- Tailwind `transition-colors` covers color, background-color, border-color, text-decoration-color, fill, and stroke; it does not imply opacity, geometry, or transform animation.
- `transition-all` is used only by the approval pager; port its width, height, border, and background together for the same visual continuity.
- CSS animation fill `both` applies the first keyframe during delay and retains the last keyframe afterward. This matters for all delayed card/row staggers.
- A React `key` change remounts the element and restarts its entrance animation. Keyed cases: approval question, recommendation body, sidebar badge, and streamed/code tokens by index.
- Conditional mount has no exit animation in these sources unless a persistent grid wrapper performs collapse. Menus and badges therefore disappear immediately when unmounted.
- Grid `0fr↔1fr` requires the inner child to have `overflow:hidden` (and occasionally `min-height:0`); interpolate available track size, not a guessed pixel height.
- Default transition easing is the shared in-out curve even when the motion is an entrance-like change; only use strong-out where an inline `transitionTimingFunction` overrides it.
- `ease-out` on named animations resolves through the token curve `cubic-bezier(0,0,.2,1)`; literal `ease-out` on WA/CSS transitions uses the same CSS keyword shape.
- All stagger delays are relative to the moment the element mounts/state flips, not absolute page time, unless an effective cumulative time is explicitly stated.
- Hover and active transitions must remain interruptible: reversing before completion begins from the current interpolated value, matching CSS transition behavior.
- Active scaling is centered unless a source sets `transformOrigin`; explicit origins occur on reply Sections (top-left) and popup entrances (bottom center/right).
- Opacity-hidden controls often also set `pointerEvents:none`; preserve this in streaming/chat selection states so invisible controls cannot receive input.
- For continuously spinning elements, phase does not persist across conditional remount: failed retry, working spinner, badges, and dictation bars restart when mounted.
- `font-mono` is semantically important for timers, code, paths, domains, and numeric deltas; use tabular figures wherever `tabular-nums` appears to prevent width jitter.
- Radius interpolation in CSS is numeric; Task Rows’ `22→14px` and Prompt Bar’s full/pill→24px should not switch as discrete enum shapes.
- The TSX does not supply reduced-motion fallbacks except Prompt Bar’s Glimm guard. Any global CSS fallback is outside scope; GPUI should define its own policy explicitly.
- Liveline is an external renderer. This spec records passed flags and pointer behavior, but not undocumented internal stroke-drawing or tooltip CSS motion.
- Imported `Shimmer` and `StreamText` in Selection Actions are external atoms; only their caller timing/callback contract is knowable from this source set.
- Color transitions should interpolate resolved RGBA token values in the current theme; theme switching itself has no specified transition.
- Shadows are multi-layer tokens. Where `box-shadow` is animated, interpolate compatible shadow layers; otherwise snap token changes rather than approximating elevation with scale.
- Measurements read post-layout browser pixels. In GPUI, perform them after text shaping/layout and before paint, then commit state only when geometry actually differs.
- Rounding: Selection Actions explicitly rounds anchor x/y to whole pixels; other measured offsets/heights remain browser layout values and may be fractional.
- Scrolling regions suppress visible scrollbars in Filter Table and Prompt Bar; scrolling behavior itself has no easing or programmatic animation.

## approval-card.tsx

- State: `qi=0`, per-question `answers`, `custom`, `sent=false`, `open=true`. No autonomous demo loop. Radio choice updates immediately, clears custom input, then auto-advances or sends after exactly `480ms`; checkbox choice never auto-advances.
- Question transition: keyed body (`key=qi`) runs `fade-up 350ms cubic-bezier(.23,1,.32,1) both` on every page.
- Sent state: green 24px check runs `pop-in 300ms cubic-bezier(.23,1,.32,1) both`; “Answers sent” runs `fade-up 350ms cubic-bezier(.23,1,.32,1) 100ms both`.
- Selection: option row hover background `100ms`; 16px radio/checkbox fill color `200ms`; radio inner 6px dot `scale(0↔1)` in `200ms`; label color `200ms`. Pager dot animates all changed properties (`7↔9px`, fill/border) `300ms`.
- Buttons: dismiss/pager hover colors `100ms`; send background/color/transform `200ms`, active scale `0.96`; collapsed “Open approval” hover `150ms`. Disabled previous/next opacity `0.35` is static.
- Layout: card `max-width:320px`, minimum host height `196px`, `rounded-card=10px`; sent body `148px` high; text mainly `13px`, helper/footer `12px`; icons `12–14px`; custom checkbox radius `5px`, send radius `8px`.
- Interaction: previous/next/dot direct paging; radio delayed advance; final arrow sends only with answer; dismiss replaces card with reopen button; Start over resets all state.

## chat-composer.tsx

- State machine `idle→sent→reply1→reply2→done`; component initializes at `done`. A valid send sets `sent`; timers: `sent→reply1 500ms`, `reply1→reply2 1400ms`, `reply2→done 1200ms` (cumulative `0/500/1900/3100ms`).
- User bubble: opacity `0↔1` + `translateY(10px↔0)` over `300ms`, strong-out.
- Each mounted reply Section enters with `fade-up 400ms` strong-out. Section also transitions opacity/filter/transform `400ms` strong-out: resolving = `opacity .55`, `blur(.5px)`, `scale(.985)` about top-left; settled = `1`, `blur(0)`, `scale(1)`.
- Tabs animate background + opacity `100ms`; inactive `.5`, hover `.75`. Header actions hover color/fill `100ms`. Composer focus border/shadow `150ms`; send color/fill/transform `200ms`, active scale `.96`.
- Layout: fixed `380×288px`, radius `14px`; scrollable conversation; bubble radius `12px`, `13px` type; field radius `8px`, padding `10px`; send `28px`, radius `8px`.
- Interaction: tabs switch immediately; Enter or click sends if trimmed draft nonempty; reply timers restart from send.

## code-block.tsx

- Loop: `count=0`; blank hold `400ms`; reveal one of 6 lines every `LINE_MS=240ms`; after line 6, hold `HOLD_MS=3200ms`, reset to 0, repeat. First-to-final reveal time is `400+5×240=1600ms`; cycle is `4800ms` including final hold.
- Each newly mounted line runs `fade-up 250ms cubic-bezier(.23,1,.32,1) both`. Current-line caret is static (no blink) and disappears at `done`.
- Copy writes the full raw code; “Copied”/green state lasts `1500ms`; copy hover colors `100ms`.
- Layout: `max-width:380px`, radius `10px`; inset code area min-height `137px`, padding `12px 10px`; mono `11.5px`, line-height `1.7`; line numbers `10.5px`; filename mono `12px`.

## context-cards.tsx

- One-shot mount: `chipsShown=false`; a single `700ms` timeout sets true; no reset/repeat.
- Heading `fade-in 400ms ease-out both`. Two cards `fade-up 400ms` strong-out, delays `0/100ms`.
- Source chips transition opacity `0→1`, scale `.95→1`, and hover background over `300ms` strong-out; per-card delays `0/80ms` after the 700ms state change (effective starts `700/780ms`).
- Layout: `max-width:380px`, 8px card gap, 10px radius; header/body text `13/12.5px`; source pill height `24px`, full radius, badge `14px` with 4px radius.

## diff-table.tsx

- `useStage([800,1000,1000])`: stage 0 at mount, stage 1 at `800ms`, stage 2 at `1800ms`, stage 3 at `2800ms`, then stop. Actual tint begins only at stage 2 (`1800ms`), despite comment “1 red tint”; added row begins stage 3.
- Removed rows: background, primary/supplier colors, category opacity (`1→.55`) all `400ms` default easing; supplier line-through switches discretely with the style update.
- Added row: `grid-template-rows 0fr→1fr` + opacity `0→1`, `400ms` strong-out; inner clips overflow and uses green tint.
- Row hover background `400ms` because it shares row color transition.
- Layout: `max-width:380px`, radius 10px; columns `34/30/36%`; table text `12–13px`; status pills height `22px`.

## filter-table.tsx

- Manual state only: `filter=all`; clicking a filter recomputes `shown` for every row.
- Filter pills animate background, shadow, color `200ms`; inactive hover fill participates. Row visibility uses `grid-template-rows 1fr↔0fr` + opacity `1↔0`, `300ms` strong-out with clipped inner content.
- Table row hover background `100ms`; count-chip color/fill changes are not assigned a transition.
- Layout: `max-width:420px`; horizontally scrollable inner minimum width `420px`; columns `1.3fr/.6fr/.95fr/.9fr`; card radius 10px; filter height `26px`; table type `11–12px`.

## fine-tune-card.tsx

- No timed demo. `done` becomes true after any deviation from defaults: segment 0, W 324, H 96, radius 28, opacity 100, type “Select type”. “Edited” mounts with `pop-in 250ms` strong-out; otherwise “Adjust” shimmers `1.4s linear infinite`.
- Scrub fields transition background + ring shadow `200ms`; active uses accent tint + 1px accent ring. Segment thumb translates by `seg×100%` over `300ms` strong-out; segment icon colors `200ms`.
- Type button ring shadow `200ms`; chevron rotates `0↔180°` in `200ms`; menu mounts `pop-in 200ms` strong-out from bottom-right; menu hover fill `150ms`.
- Scrubbing: pointer delta is `(clientX-startX)/2×step`, rounded and clamped. Arrow keys change one step; Shift changes ×10. Direct numeric input strips non-digits except `-` and clamps.
- Layout: card max-width `240px`, radius 10px; 3-way segment track; controls `26px` high; menu width `120px`, radius `10px`; main type `12–13px`.

## insight-cards.tsx

- Manual circular carousel only: page starts 0; previous/next wraps modulo 3. Source comment says autoplay “yields”, but no autoplay/timer or user-yield state exists.
- Page wrapper declares opacity/filter transition `250ms`, but its inline style is always `opacity:1; blur(0)` and it is not keyed; page switches therefore have **no implemented crossfade/blur motion**.
- Pager buttons hover background/color/transform `100ms`, active scale `.96`; suggestion pill hover fill `100ms`.
- Compare/Anomaly charts are paused Liveline snapshots: no pulse/momentum animation. Pointer x is clamped, rounded to nearest data index; cursor/tooltip appears while down/moving and clears on leave/cancel/up. Anomaly Spend/Usage pill transition `150ms`, active `.96`.
- Allocation selection: each segment opacity `.58↔1`, inset ring, transform over `300ms cubic-bezier(.16,1,.3,1)`, active `.98`; inner highlight width `0%↔calc(100%-8px)` + opacity over `500ms` same easing; legend pill colors/transform `150ms`, active `.96`.
- Layout: host `max-width:344px`, min-height `408px`; cards min-height `278px`, radius 10px, padding 12px; chart stage `166px`; hero values `17/20px`, most labels `10.5–12.5px`, monetary deltas mono `11.5px`.

## loading-state.tsx

- Elapsed timer: `setInterval(100ms)`, increments deciseconds; format `0.0s…59.9s`, then `Nm S.s`. It is independent of visual cycle.
- 3×3 4px grid, gap `1.5px`. Drive/Dots chevron delays by cell index `[90,180,270,0,90,180,90,180,270]ms`; `pixel-on 650ms ease-in-out <delay> infinite`. Drive square radius 1px; Dots circular.
- Orbit perimeter order `[0,1,2,5,8,7,6,3]` gives delays `[0,110,220,770,null,330,660,550,440]ms`; `pixel-on 950ms ease-in-out`; center has no animation and opacity `.07`, animated cells base opacity `.15`.
- Label gradient stops ink-3 35% → ink 50% → ink-3 65%, background-size `200% 100%`, `shimmer-text 1.4s linear infinite`. Label `13px`; timer mono tabular `12px`; overall gap `10px`.
- Source comment promises reduced-motion freezing, but TSX has no media-query check; that behavior can only come from absent global CSS.

## prompt-bar.tsx

- Autoplay starts enabled; any captured pointer-down or key-down stops it permanently; targeting textarea while auto is active also clears demo draft. Steps/timeline (effect at listed cumulative time): `0 blank+vanilla` hold1100; `1100 @ row0` 900; `2000 @ row1` 620; `2620 @ row4` 620; `3240 @ row6` 700; `3940 @ row6 connected` 1000; `4940 blank` 700; `5640 / row0` 900; `6540 / row1` 620; `7160 / row3` 1000; `8160 blank` 800; `8960 model menu` 1200; `10160 flagship` 2400; `12560 blank` 900; loop length `13460ms`.
- Selecting flagship triggers Glimm rainbow left-to-right sweep: `sweepMs=950`, `outroMs=130`, `peakAlpha=1.3`, `bandTight=10`, `brightness=1.4`, `swellAmount=1`, `waveSpeed=1.3`, easing `easeOutExpo`; palette red→orange→yellow→green→cyan→blue→purple. Recreates shader with deterministic hue seed; ignores overlapping sweep and reduced-motion.
- Dictation: listening for exactly `2200ms`, then appends fixed transcript, stops, focuses input. Equalizer has 3 bars, `eq-bounce 900ms ease-in-out infinite`, delays `0/150/300ms`.
- @/slash and model menus mount `pop-in 180ms` strong-out; origins bottom-center / bottom-right. Shared hover highlight measures active row `offsetTop/offsetHeight` in `useLayoutEffect`; top+height glide `220ms` strong-out, opacity `150ms ease`.
- Attach chip mounts `pop-in 200ms` strong-out. Composer border/radius `150ms`; +/dictation active scale `.94` and colors `150ms`; send `200ms`, active `.94`; model/attachment hover colors `100–150ms`.
- Textarea measurement: hidden same-font span estimates one-line width. If newline or `measure.offsetWidth+8 > controlsWidth-(28×3+modelButtonWidth)-16`, controls move to second row. Set height to 0, read `scrollHeight`, clamp to `28…100px`; overflow-y becomes auto above 100px. Grid reflow itself has no explicit animation.
- Layout: host max-width `420px`, min-height `384px`; composer padding `6px`, gap `6px`, Rounded radius `14px`; Pill is full radius until attachment/wrap, then `24px`; menus radius `10px`, model menu `176px`; textarea `13px/18px`; controls `28px`.
- Interaction: `@`/`/` token opens filtered menu; arrows wrap selection, Enter/Tab picks, Escape dismisses; Shift+Enter newline, plain Enter sends. Plus opens sources; model selection closes; attach cycles 3 filenames; send clears draft/attachments/menus.

## recommendation-card.tsx

- Manual state only: selected option 0, drawer closed, not accepted. Switching option remounts body keyed by option and runs `fade-in 180ms ease-out both`.
- Alternatives drawer: grid rows `0fr↔1fr` + opacity over `300ms cubic-bezier(.16,1,.3,1)`; inner clipped. Confidence meter bars transition color `300ms`.
- Alternative rows hover `100ms`. Alternatives button background/transform `100ms`, active `.96`; primary button background/transform `150ms`, active `.96`; accepting turns it green and changes label.
- Layout: max-width `380px`, radius 10px; body min-height `48px`, `13px`; inline code mono `12px`; footer inset; buttons height `28px`, radius 8px.
- Interaction: Alternatives toggles drawer; choosing other option promotes it, resets accepted, closes drawer; primary confirms; no reset from accepted except choosing another option.

## records-table.tsx

- **No animation or timed state machine in this source.** No `transition-*`, animation, timeout, interval, or layout-effect appears. Do not infer motion from unresolved `records-*` class names.
- Interactions are immediate: row/all checkbox selection with mixed state; sortable name/last/strength headers toggle ascending/descending; sort arrow gets inline `rotate(180deg)` for descending with no declared transition; links open where present.
- Layout is entirely delegated to unresolved `records-*` CSS. Source establishes five columns, sticky first column intent, horizontal/vertical scroll region, 26 records, colored tags, strength dots, and a calculation footer; text/icon pixel values live in those missing class definitions except icons (`12–15px`).

## search.tsx

- Manual live filtering; empty state when `query.length>2` and no match. Empty query shows first 5 of 7 items.
- Clear button mounts `fade-in 150ms ease-out`; empty panel `fade-in 250ms ease-out`; each result remount runs `fade-in 200ms ease-out` (same keyed item may be retained by React if still present).
- Input-row and result hover background `100ms`; clear hover background/color `100ms`.
- Layout: max-width `288px`, min-height `248px`, card radius 10px; input row `40px`; result rows `32px`, radius 6px; text `12–13px`; empty icon box `32px`, radius 8px.
- Interaction: typing filters case-insensitively; clicking result replaces query with full item; clear resets.

## selection-actions.tsx

- State: bar hidden until `280ms`; placement must also succeed. Action sets `thinking`; after `700ms` goes `streaming`; imported `StreamText` calls `onProgress=place` per update and `onDone→result`. Exact StreamText character cadence/keyframes are outside these supplied files.
- Placement: in `requestAnimationFrame`, center x on complete selected bounds and y `8px` below last client rect. Round coordinates; update on mode/layout, host `ResizeObserver`, and window resize. Anchor transform animates `320ms cubic-bezier(.77,0,.175,1)`; visibility opacity `180ms ease-out`.
- Bar entrance `pop-in 220ms` strong-out. Spinner `spin 700ms linear infinite`; thinking label uses imported Shimmer (timing outside source).
- On mode change, `useLayoutEffect` measures content width + 8px, then Web Animations API interpolates old→new width `320ms` strong-out. A ResizeObserver stores intrinsic width when no width animation runs; prior animation is cancelled before a new one.
- Idle morphs: prompt region max-width/opacity/translateX and width `400ms` strong-out; preset region max-width `224↔462↔0`, opacity, translateX `400ms`; extra actions max-width `0↔262`, opacity, margin `400ms`; send slot max-width `0↔30`, opacity, scale `.88↔1`, `400ms`; chevron rotate `0↔180°`, `400ms`. Send button itself presses to `.94` over `200ms`.
- Common pills hover color/fill and active `.96` over `150ms`; more button `200ms`; retry `150ms`. Streaming text physically reflows the selection and therefore continuously repositions the bar.
- Layout: host max-width `460px`; selected text highlight radius `3px`; bar `36px` high, 4px padding, fully round, max width `viewport-48px`; controls `28px` high/full radius; body `13px`; controls `12–12.5px` sans.
- Interaction: Improve/Shorten/Tone/Grammar run pipeline; free prompt captures starting width on first nonblank char and shows send; more expands presets; result Keep/Discard both reset, retry reruns current action.

## sidebar-nav.tsx

- No auto sequence. Active starts `tasks`, badge 4. `useLayoutEffect` measures target relative to nav for `hovered ?? active`.
- Single highlight pill glides top and height `220ms` strong-out; opacity `150ms ease`. Mouse leave restores active target. Active item text/icon color `150ms`; rows active scale `.96` over `150ms`.
- Workspace and New task hover/fill + press scale `.96` `100ms`. Supplier plus fades on group hover and changes colors `100ms`. Task badge remounts on count change (`key=badge`) with `pop-in 250ms` strong-out.
- Layout: width `240px`, card radius 10px/padding 8px; workspace logo `32px`; search `32px`; nav row radius `7px`; main type `13px`, section heading `10.5px` uppercase tracking `.08em`, icons `13px`.
- Interaction: hover/focus moves highlight; click selects; New task increments badge and selects tasks; search stores text but does not filter; workspace/plus have no attached action.

## streaming-text.tsx

- Loop has 28 tokens (19 words + citation + 8 words). Start `count=0`; add one token every `WORD_MS=55ms`, reaching done at `1540ms`; hold `HOLD_MS=3400ms`; reset to blank and repeat (`4940ms` cycle). No extra initial delay.
- Every word mounts `stream-in 420ms cubic-bezier(.22,.61,.25,1) both`, declared will-change filter+opacity. Because cadence is 55ms, about 8 trailing tokens overlap in blur/fade resolution; older words are settled while the tail remains soft. Citation mounts `pop-in 250ms` strong-out.
- Caret mounts per count with `fade-in 150ms ease-out`, but does not blink. Actions and follow-ups transition opacity `400ms` only when done; pointer events remain none beforehand.
- Follow-ups additionally run `fade-up 350ms` strong-out at delays `0/90ms`. Sources drawer grid rows + opacity `300ms` strong-out. Action hover `100ms`; source-chip/list hover `150ms`.
- Layout: host max-width `380px`, min-height `248px`; prose `13px` relaxed; citation `18px` high, mono `10.5px`, radius 5px; icons `15px` in 24px controls; sources panel radius 10px; follow-ups `12.5px`.
- Interaction: after done, source summary toggles measured-free drawer; links open externally. Other action/follow-up buttons have no handlers in source.

## task-rows.tsx

- `TICKS=[600,900,2400,1400,2400,600]`, but hook stops at index `length-1`; actual stages: tick1 `600ms`, tick2 `1500ms`, tick3 `3900ms`, tick4 `5300ms`, tick5 `7700ms`, then stop. Final array value `600` is unused.
- Row 2 (`draft`) state: pending ticks 0–2; failed at tick3 (`3900ms`); done ticks 4–5 (`5300ms+`). Middle `index` auto-opens only at tick2 (`1500–3900ms`). Source comment’s “600ms ring sweeps 0→66%” is not implemented: active ring spins from first render and has fixed 28% dash.
- Rows enter `fade-up 450ms` strong-out delays `0/80/160ms`. Active 24px SVG ring spins `1.1s linear infinite`; badge mounts `pop-in 300ms` strong-out. Failed/Completed pill `fade-in 200ms ease-out`; failed retry icon spins `1.2s linear`.
- Capsule radius animates closed `22px`↔open `14px` over `300ms`. Chevron rotates `300ms`. Detail grid rows+opacity `300ms` strong-out; detail lines `fade-up 300ms`, delays `120/220ms`. Row hover fill `100ms`.
- Layout: max-width `440px`; Capsule gap 8px/min-height 196px, List is one 10px card; row height `44px`; labels `13px`, detail `12px`, metadata mono `11.5px`.
- Interaction: every row manually toggles; manual value overrides auto-open thereafter. Variants Capsules/List affect container/radii only.

## thinking-state.tsx

- `STAGES=[800,600,1800,2600,1600]`; hook stops at `length-1`: stage1 `800ms`, stage2 `1400ms`, stage3 `3200ms`, stage4 `5800ms`, stop; final `1600` unused.
- Auto-expanded stages 1–3 (`800–5800ms`), collapsed stages 0/4. Working through stage2 (`<3`); done at `3200ms`. Visible rows: 0 at stages 0/1, first min(2,N) at stage2, all at stages 3/4. Manual expand boolean overrides auto state.
- Working label `shimmer-text 1.4s linear infinite`; done label `fade-in 350ms ease-out`. Header chevron rotates `300ms`; trace grid rows+opacity `400ms` strong-out.
- `useLayoutEffect` reads trace `offsetHeight` after visibility/expand/variant/stage changes. Vertical line animates height to `measuredHeight-2px` over `500ms` strong-out, top `-8px`.
- Query entrance `fade-up 300ms` strong-out. Rows `fade-up 320ms` strong-out stagger `120ms×index`. Steps’ active spinner `700ms linear`; Search “+7 more” `fade-in 300ms ease-out`. Row hover `150ms`.
- Layout: max-width `380px`, min-height `176px`; header `13px`; trace margin-left 5px/padding-left 16px; rows min-height `28px`, radius 6px, `12.5px`; secondary/mono `11–11.5px`.
- Interaction: header expands/collapses; Search rows are links; Coding rows toggle selected inset fill; Steps/Reasoning rows inert. Variants share one timing machine.

## tool-chips.tsx

- One-shot `STEP_MS=700`; `total=ROWS.length+1=5`. Step1/2/3/4 at `700/1400/2100/2800ms` reveals rows sequentially; step5 at `3500ms` reveals diff chips; then stops.
- Tool row mount `fade-up 300ms` strong-out. Run collapse: grid rows+opacity `300ms` default easing; header chevron rotates `0↔-90°` `200ms`.
- Row hover swaps normal icon opacity out `100ms` and chevron opacity/rotation in `150ms`; row background/chip hover `100ms`. Detail grid rows+opacity `300ms` strong-out.
- Three diff chips `pop-in 250ms` strong-out delays `0/80/160ms`; “+2 more” `fade-in 300ms ease-out 240ms`.
- Layout: max-width `320px`, min-height `220px`; header `12.5px`; tool row `28px`; chip height `22px`, radius 6px, `11.5px` (mono for files/commands); details `11.5px`, optional mono; diff chips `28px`.
- Interaction: header collapses whole run; each row independently expands details; row hover reveals chevron; diff chips and “+2 more” are pointer-styled but have no handlers.
