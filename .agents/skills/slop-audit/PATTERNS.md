# What the audits met

Cases from five rounds: the provider lifecycle (PR #1051), every crate
outside the UI (#1054), the UI (#1058), what needed a contract changed
(#1060), and the tests (#1062, #1063). Paths are as they were then and may
have moved. Each case is evidence, not a rule; the definitions and the
grounds are in [SKILL.md](SKILL.md), the order of work in
[AUDIT.md](AUDIT.md).

The maintainer's words are quoted from the sessions of 2026-11-10 and
2026-11-11, translated from Chinese. They decided entries then and are the
likeliest to decide entries in other areas.

## By what the defence does

### Hides

- **An emptied todo list** (`claude.rs`, `extract_plan_steps_from_todo`):
  `todos: []` produced no event, so the sidebar kept the list the model had
  just cleared. Now an empty `PlanUpdated`.
- **A question with no header** (`codex.rs`, `parse_codex_user_input`): the
  model asked, the user never saw it, and a request with every question
  filtered out blocked with nothing to answer. Now passed through; only a
  question with no id, which cannot be answered, is dropped.
- **Models behind a version bound when `claude --version` could not be read**
  (`claude_manifest.rs`, `available`): a broken PATH emptied the picker of the
  current models the CLI runs fine. Now offered; the CLI refuses what it
  cannot run.
- **A failure written only to the log**: Codex refusing to stop a background
  terminal, Codex or OpenCode or pi failing a listing at session start, an
  attachment that could not be read, a worktree Git could not repair, a
  thread-activity backfill that failed. Each is now a Warning or an error
  carrying the source's own text, through a path that already existed.

### Alters

- **A turn failed because one tool failed** (`pi.rs`, `tool_event`): a bash
  exit 3 the model read and carried on from marked the whole turn Failed. pi
  said the turn completed.
- **An interrupt guessed from prose** (`claude.rs`, `result_status`): any
  non-success result whose text contained "abort", "cancel" or "interrupt"
  became Interrupted, which also dropped the CLI's error text. Now a
  non-success result is Failed with Claude's text; an interrupt Tcode sent is
  attributed by `interrupt_pending`.
- **A plugin command that exited 0** (`claude_plugins.rs`, `json_outcome`)
  reported as a protocol error because its last line was not JSON.
- **A failed import counted as skipped** (`sessions.rs`,
  `start_external_import`): not yet changed; it needs a count in the protocol.

### Replaces

- **Answering the harness's open prompts**: OpenCode's forms were deleted when
  a turn ended, and at shutdown its pending permissions were rejected with the
  message "Tcode session closed", which OpenCode writes into the session for
  the model to read on resume. pi's extension prompts were cancelled at
  shutdown. All removed.
- **A made-up turn or message id** (`opencode.rs` turn counter, `pi.rs`
  `assistant_id`): each was the fallback left behind by the fix for the very
  bug the fallback reproduces (a rewind hitting the wrong turn; a message
  shown twice).
- **The shared cwd when a subagent's worktree could not be made**
  (`orchestrate.rs`, `resolve_child_worktree`): children asked to be isolated
  ran in the lead's checkout. Now `start_thread` fails with the reason.

### Cuts short

- Removed where the user could already stop the thing: OpenCode's 24-hour SSE
  read deadline (a separate thread reads it; stopping the session closes the
  socket), Grok's 30 seconds waiting for a cancelled turn before a rewind.
- **Left in place, see [Timeouts need a cancel path](#timeouts-need-a-cancel-path).**

### Refuses

- **A path outside the session cwd** (`acp_session.rs`, `resolve_path`): an
  ACP agent could not read `~/.gitconfig` or a sibling directory its native
  CLI reads. The check normalised lexically and stopped no symlink.
- **A check repeated behind the boundary** (`send.rs`, `send_turn_assembled`):
  the protocol boundary already refuses a send to a read-only mirror or while
  a relay awaits confirmation. The repeat could only be reached by Tcode's own
  paths (a scheduled message coming due, a rejected steer sent again), which
  had already taken the message off the queue: it was lost. Now it waits in
  the queue.
- **A permission value checked against the descriptor again** (`codex.rs`,
  `SetOption`): the UI already disables what the descriptor marks unavailable;
  Codex refuses what it will not take.

### Invents

- **Second spellings** of wire fields only one spelling of which is ever sent:
  pi's `input` for `arguments`, `text` for `thinking`; Grok's camelCase and a
  `result` wrapper; Claude's `done`, `succeeded`, `cancelled` task statuses.
  Each was checked against a recorded fixture and the harness's own source.
- **Branches for states the code cannot reach**: Cursor's replay guard, whose
  comment said the requests it guards are never sent then; Grok's
  `session.id()` being `None` after the session is established.
- **A limit nobody chose**: 32 levels of directory copy, after which a deeper
  subtree was silently skipped; 1 MiB of ACP terminal output when the agent
  set no limit.

### Stands in

- **Four tests written during this audit and then deleted from it.** The
  rewrite agents, told to add a test where a behaviour had none, each wrote an
  imitation of the harness and asserted against it: a shell script answering
  as `codex app-server` would when it refuses two listings, a fake OpenCode
  server returning 500 for `/api/mcp`, a branch added to pi's `RPC_STAND_IN`
  script to refuse a steer, a fake `opencode` CLI failing `plugin list`.
  > To test some Claude integration, writing a tiny imitation `claude` binary
  > that takes one CLI argument and returns one data structure, then a test
  > case calling the code against that binary: that is simply absurd.
  > — the maintainer
- **Still in the tree** at the time of writing: some thirty-five shell scripts
  written by tests across `crates/agent`, `crates/runtime`, `crates/services`
  and `crates/ui`, and the `FakeClaude`, `FakeServer`, `FakeCli` types. Not
  yet inventoried; some only need a file to exist, others speak for a harness.
- **Wire lines whose shape nothing backs**: `codex/v2_messages.jsonl`, lines
  9–10 of Claude's `usage_scope.jsonl` name no schema or source. A defence
  that relied on one of these (`result_ids`, de-duplicating a `result` by
  uuid) rests on Tcode's guess.
- **Recordings of real runs, committed.** The opposite fault. The fixtures
  that are real captures carried what the run saw:
  `runtime/src/app/fixtures/monitor_wakeups_child.jsonl` and
  `plugins/claude/real-home-marketplace-list.json` hold the maintainer's
  home directory, toolchain paths and the name of another project, in a
  public repository. No token or address was found.
  > I do not agree to using real data. Oversight slips, I iterate fast; what
  > if something leaks? — the maintainer

  So a fixture takes its shape from the harness's schema or source and its
  content is invented.
- What stood up instead: the interrupt change was shown with the probe
  against the real Claude Code (`--interrupt-after`), which still reported
  the turn interrupted.

## What a stand-in does to production

Removing the stand-ins showed they had not only proved nothing; they had
taught production code what to expect.

- **A shape only the test had.** OpenCode's plugin listing read
  `[specifier, options]`, a spelling the hand-written test input used and
  OpenCode never has. The spelling OpenCode does use, `{"package", "options"}`,
  nobody read, so a plugin configured with options was missing from Tcode's
  list. Found when the fake CLI went and the parser was checked against
  OpenCode 2.2.6's own schema.
- **A list checked against itself.** The test of the hidden ACP agents chose
  its own ids and compared them with the hidden list. The live registry had
  `opencode-v2`, a duplicate of the native integration, and the marketplace
  offered it (Principle 3).
- **A protocol whose sender is gone.** pi's approval parsing read the payload
  of a Tcode extension deleted long before; the test and the parser kept each
  other alive.
- **A frame the harness never sends.** A Claude test removed a notification
  from a trace to build "a terminal update with no notification after it",
  which Claude Code 2.1.322 cannot produce; the code path it justified hid
  the real defect, where the output file named by that notification was
  never read.
- **An order only the synthetic trace had.** The hand-built subagent trace
  asserted Start, PreferPath, Stop; the real one is Start, Stop, PreferPath,
  Stop.

## Why tests stayed slow after the sleeps went

After every lane, 202 tests still ran over two seconds (231 before). Three
causes outside any one test, each found independently by several lanes:

- **Unoptimized dependencies doing the test's real work**: picture decoding,
  PBKDF2 at its real iteration count, zstd dictionaries. Optimizing those
  packages in the dev profile took an interrupted storage pass from 7.8 s to
  3.0 s. Done in this PR.
- **A database per test, with `fullfsync`**: about 0.2 s to open and an fsync
  per commit, queued behind each other when tests run in parallel. A case
  table that opens one host for all its rows instead of one each is the part
  a test can do. Open.
- **`draw_settled` sleeping 16 ms a frame** (577 call sites in `crates/ui`),
  because gpui-base's scrollbar and bounce sample `Instant::now()`. A test
  that calls it after every step sleeps a whole animation each time; one
  slept 6.7 s. Upstream's clock, to be confirmed and carried under Principle
  8. Open.

## Habits the lanes named

Each was found in one lane and is likely elsewhere.

- **The expected value comes from the code under test**: a label computed
  with the function being tested, a hash literal the code produced, a filter
  expression copied into the test.
- **A negative assertion that cannot fail**: `try_recv().is_err()` straight
  after writing to an echo child, before the line could have crossed the
  pipe. A marker written after it, and reading everything before the marker,
  makes it real.
- **A wait that never ends on a regression**: `recv().await` with nothing
  bounding it, ended only by the runner's kill timer.
- **Private state written to set the scene, or read to assert**: a field
  assigned instead of the event that sets it, a private cache cleared to
  reach a state no user path reaches.
- **A production timer shortened through a private field** so the test can
  wait it out on the real clock.
- **A timer as the contract**: a test that passes only once a production
  deadline has run out.
- **Input larger than the contract**: a 2048-pixel picture where the
  assertion is who reads it, megabytes where one line over a chunk size does.
- **One host or store per row of a case table.**
- **A scene replayed across every width or theme** when neither moves what is
  asserted.
- **Motion as a contract**: a frame count, a fraction of an animation reached
  at a given frame, a pixel offset, with no user path depending on the number
  (Principle 6).
- **The test rebuilds the production loop**: aborts the real task, edits its
  private throttle, calls its private step by hand.
- **The driver is unlike the real peer**: a test browser that never closes
  its socket and so waits out the proxy's drain every time.
- **A comment that says "recorded"** over data with no recording and no
  source for its shape.
- **A name that promises more than the assertions deliver.**
- **A test with no fixed binary path** may launch the real CLI found on PATH.

From the rounds over the UI and across the layers:

- **A second gate**: a menu already left an action out by some state, and the
  handler checks the same state again.
- **A shadow of the host's rule**: the UI keeps its own, differing copies of
  a judgment the host owns. Whether a thread could be settled was decided in
  five places, none of them the host's.
- **Rebuilt by kind**: the client fills in, from a table keyed by provider
  kind, something the host already sends.
- **Written, then split again**: one layer joins meaning into a sentence and
  another cuts it apart to translate it. Codex's permission description, a
  profile's display name.
- **An empty id for a missing one**: `unwrap_or_default()` on a thread id,
  sent to the host.
- **Not answered yet, shown as none**: a list drawn empty, or a switch drawn
  off, before the host has said anything.
- **Failed, shown as empty**: a query's error turned into no rows, a count of
  zero, "no matches".
- **The log as the report**: a fallible read drops its error into a log line
  and hands on an empty value, so no layer above can say what happened.
- **A door in place of a permission**: the client holds a command back while
  some state is unknown, in place of the host's own authorization, and the
  user's action vanishes.
- **A layout taken for a capability**: a compact window turned off sending
  while reconnecting, pasting a picture, dropping a file.
- **A guessed layout value written to shared state**: a phone's estimate of
  its terminal height sent to the host as the thread's.
- **A second owner of one timing**: a timer run beside the one that already
  counts the same thing down.
- **A catch for panics nothing showed**, written with the feature.
- **A gap comment whose reason does not hold**: a local primitive justified
  by a cost the dependency does not have.
- **A builder option nothing sets**, and the branch behind its default.
- **A version gate as a capability switch**, with an unread version taken
  for an old one.
- **A limit that outlived its timer**: a number kept from a timeout that was
  removed, on a path where it no longer means anything.
- **Production turning off its own loop for tests** behind `cfg(not(test))`,
  with a hand pump beside it, so the test drives something the app never
  runs.
- **A fixture that contradicts itself**: a notice saying 35 lines over 25.
- **A Debug string of a map compared**: it fails by iteration order.
- **A test clock borrowed from another module's duration**, equal by
  accident.
- **Queued in the order work finished**, not the order the user gave.

From the round that worked through what was carried forward:

- **Production tolerating what only a test lacks**: `try_global` where the
  app always installs the global, kept so that tests which never installed
  it would run. Install it in the tests' mount, then read it plainly.
- **A test build that draws another tree**: a selector or a wrapper element
  under `cfg(test)`, so the test looks at something the app never draws.
- **Cleanup asserted**: a test that requires its scratch directory to be
  gone. A stopped host can still have work in it; removing it is cleanup.
- **A wait on something that tells nobody**: a deletion run off the host
  with no event when it ends. A test that waits on events for it passes
  only when the work finished before the wait began. The finding is the
  missing event, in production.
- **One limit kept twice**: a history window withheld a record too long for
  a network line and a live push sent it whole. A limit has one owner, and
  every path that meets it goes through that owner.
- **A sentence for the model the user never sees**: text added to a message
  because a harness was thought to need some. None did.
- **A branch per platform compiled on one**: a parameter mutated only on
  Unix, a module only its tests kept alive elsewhere. This machine's Clippy
  cannot see either; read a change that touches `cfg` for what the other
  side is left holding.
- **A setting that does not survive a conversion**: stdio set on a `std`
  command that is then turned into an async one, which puts back to inherit
  whatever its own setters did not set. Every plugin listing read nothing
  for a round, and no test could have said so.

From the round over the Wayland preview (the nested compositor):

- **A timer where the peer has a signal**: a view's next frame released by a
  16 ms timer, when the session's compositor sends a frame callback for
  exactly that; GTK's loop polled every 16 ms, when GLib names the
  descriptors and the timeout to wait on. Every hop of an interaction waited
  out a tick.
- **A dependency's conversion undone by constants**: scroll deltas divided
  by the factors GPUI was read to multiply by. The session's own pointer
  events were there to be passed on unchanged.
- **Guessing when the source will ask**: the clipboard copied in before the
  keys that usually paste. A selection that fetches when the client asks
  needs no guess.
- **A misbehaving peer only a test can be**: positions clamped, buffer edges
  capped and handler panics caught for a hand-written client sending what
  the one real client, GTK in the same process, never sends.
- **A window nobody shows**: the client's file chooser was a toplevel the
  compositor kept and never drew, and being modal it took the page's input
  for good. Found by pressing the button in the running app, not by reading.
- **A link fix that held only for optimized builds**: a release binary
  linked, the unoptimized one did not. A change to how a binary links is
  built both ways before it merges.

## What the audit did wrong

- **It added tests by symptom before finding the contract's owner** (#1096).
  Separate account-environment and title-refusal tests repeated setup beside
  existing lifecycle tests. The environment fixture reused one command channel
  across launches, so observing a new launch did not establish which process
  received the message. The title test changed settings before admission, so
  it did not exercise invalidation during an admitted asynchronous start.
  Existing per-launch channels and held-start fixtures reached both decisions
  without a new production seam.
- **It reported in place of verifying.** Four rounds merged with every
  visual change marked "not looked at in the running app". Saying so each
  time did not make it looked at.
- **It took a lane's word.** Twice in one hour a fix was called done before
  its result was read; one edit had not applied, the other fixed the wrong
  thing.
- **It let the tests pass by.** The brief for the UI round said the tests had
  been cleaned already. Two hundred and thirty-three of them were two thirds
  of the suite's time, and a third to a half tested primitives.
- **It did not say what its building cost.** The fourth round came out three
  thousand lines longer. The maintainer's answer was that building is not a
  problem: whatever leaves the codebase better is wanted, whatever harms it
  is not. What was missing was the account. The largest part was adapters
  reading the text layout of harness output, where a guess had moved rather
  than gone, and nobody had checked that a layout an adapter does not
  recognise is still shown as the harness wrote it.
- **Removing a bound made a hang last.** With the in-test limits gone a wait
  that never ends is held only by the runner, and the first one met cost a
  platform five minutes before Principle 10 could be applied to it.
- **It answered a measured question from memory.** "Compilation is most of
  CI" was said before any log was read; test execution was half.
- **It called a pass thorough that was not.** After the first pass over the
  tests, sixty-two frame steps and fourteen pixel assertions were still in
  one file. Nobody had counted.
- **It passed on a lane's doubt as a question.** A lane noted that a deleted
  fade test came in with a commit saying the maintainer had chosen that fade
  out of three, and the main session asked whether to restore it without
  reading the test, which stepped six hundred frames comparing opacities.
- **It deleted by name.** A script removing unused test helpers by function
  name took a production method of the same name in another file.
- **It left the model to default.** A walk-through sent eight prompts on the
  most expensive model to see a reply arrive.
- **It asked leave it already had**, to bring a window to the front, and
  waited a round for the answer.
- **It stopped a lane and not what the lane had started.** Three subagents
  and watchers ran on after their parents were stopped.
- **It broke `main` and did not know.** Making plugin changes cancellable
  left every plugin listing empty. The lane had written that only a real
  CLI could prove the path, and nobody ran one. A later lane found it by
  using the same call. What only the real thing proves is run before the
  merge, not listed after it.
- **It guessed how much was left.** "About seventy percent of what was found
  is dealt with" was said from memory; a count against the code gave 139
  done or void and 151 still there. The carried-forward list is counted
  against the code before anything is said about what remains.

- **It read the code and said how the app behaves.** "Enter during
  dictation already stops and sends" was true and beside the point: the send
  button stayed dark, so the maintainer never tried it, and a phone has no
  Enter. "A regression from round four" was a guess said as a cause; the
  lane found an older commit. What is read is reported as read.
- **It called a non-blocking question unanswerable.** Codex asks without
  waiting and drops the question when the turn ends; the original app does
  the same. A harness's own design is looked up before it is called a
  defect.
- **It kept a round open.** New items were sent to running lanes for a day
  while the maintainer could not restart the app the conversation ran in.
  A round has an end stated at its start: the last merge. What is found
  after that is listed.

- **It tied a state to a signal nothing redraws for.** A page's keyboard
  focus was made to follow the window being active, read during a paint;
  nothing paints when the window comes back, so keys were dropped. Only the
  running app showed it.
- **It wrote a test that waited on its own fixture.** A new control was put
  where an earlier step of the same test sampled the background, and the
  wait never ended. A wait with no bound of its own is looked at running
  before it is pushed.

## Families beyond the defence

Named by the maintainer, not yet inventoried:

> Test functions that sleep for seconds or even tens of seconds, single test
> cases that run for minutes in GitHub Actions, races that deadlock and never
> stop: too many to list. And what is bad is not limited to the kinds named
> so far. — the maintainer

What is known without an inventory:

- 156 `thread::sleep` / `Timer::after` sites across `crates`, among them
  5, 10, 20, 30 and 60 second waits in `crates/runtime/src/app/tests.rs`,
  `crates/ui/src/store`, `crates/agent/src/cursor/tests.rs` and
  `crates/agent/src/grok.rs`.
- `.config/nextest.toml` says of itself that the slowest passing tests on CI
  take under three minutes, and kills a test at five so that one hung test
  does not hold the job: a kill timer standing in for tests that end.
- Tests whose final assertion depends on when a random kill lands
  (`tests-runtime-b` #149, #152) are candidates for Principle 10.

## Kept, and why

- **Stream end without `SessionClosed`** (`fc80e498b`; #75, #104, #161):
  threads stuck on "Working".
- **`settings_unreadable`** (`bf578b3a6`): a 0.3.2 build overwrote a newer
  build's settings with defaults.
- **An unacknowledged steer goes back on the queue** (`7daca9802`): a
  subagent's report was lost.
- **The cwd is checked before spawning** (`4a12aff57`): a deleted worktree was
  reported as a missing binary.
- **Codex's first-start sqlite race is retried once**: an upstream bug,
  reproduced 12 times in 36 starts, carried under Principle 8.
- **ACP `initialize` has a timeout** (`817f7b429`): cline 3.0.39 never
  answers it.
- **Rate limits and the five-failure invitation limit**: Principle 5.
- **An idle provider is stopped after an hour, sixteen residents at most**:
  the maintainer's word.
- **A plugin argument beginning with `-` is refused**: the id comes from
  third-party marketplace data and would be read as an option.
- **`bypassPermissionsModeAccepted` seeded for an API-key account**: Claude
  Code 2.1.322 downgrades bypass to default for background workers without
  recorded consent, and such an account's config directory is one the user
  never opened interactively. Kept; the maintainer was unsure, and a run
  comparing both seeds has not been done.

## Where the inventory was wrong

The rewrite round overturned or corrected these. They are the reason every
entry is verified again before it is changed.

- **"`interrupt_pending` attributes every interrupt Tcode sends."** A
  conversation-only rewind makes the CLI interrupt the running turn without
  setting it. With the prose heuristic gone that turn would have shown as
  failed. The fix set the flag there too, with a test that fails without it.
- **"An unknown task status should be a terminal failure."** Claude Code's own
  schema lists `running` and `paused` for `task_updated`; treating them as
  terminal would have stopped the subagent tail early. `task_updated` keeps
  unknown values in progress; `task_notification`, the one terminal frame,
  decides.
- **"An early close is wrongly judged a failed session transfer"**
  (`events.rs`): not removed. When the copy succeeds but the CLI cannot resume
  it, the launch returns Ok and the failure arrives only as `SessionClosed`.
  Without the judgement the thread copies, resumes and closes on every start
  and can never send. `an_account_switch_relays_when_the_copy_does_not_resume`
  guards exactly that.
- **"`SpawnedHost::shutdown_blocking` has no callers."** It has a dozen, in
  `crates/ui` and `crates/runtime` tests.
- **"A failed `get_entries` should be read again without `since`."** That is a
  retry, a new mechanism. The rewrite reports the failure and leaves the
  cursor alone.

## Timeouts need a cancel path

The maintainer agreed that a timeout becomes "no limit, the user cancels".
The rewrite found that for several the user cannot cancel:

- **OpenCode's start** (`Server::spawn`, `wait_ready`,
  `wait_for_server_connected`): the launch blocks, and stopping the session
  only sets its state back to idle; it does not reach the blocked start.
- **OpenCode's REST calls**: made synchronously inside the actor, which reads
  no commands while one is outstanding, so neither `Shutdown` nor `Interrupt`
  arrives. The 60-second read timeout is what lets a hung server be reaped.
- **OpenCode's wait before a rewind**: same loop, same reason.
- **Claude's live settings change**: no turn is running while it waits, so
  there is no stop button; a queued message then blocks settling the thread.
- **Plugin operations** on every provider: no cancel entry exists.
- **Title generation**: a background task with no user handle.

These stay until stopping a session kills the process directly instead of
sending a `Shutdown` the actor may never read. That is the first entry of the
next round, not a reason to keep the timeouts for good.

## Something else may answer for it

- **pi's extension prompts at turn end** were not left open, though the same
  cancel at shutdown was removed: `crates/core` clears pending requests when
  a turn completes, so pi would wait on a prompt the user can no longer see.
  The fold has to change first.
- **The approval option id check** (`acp_session.rs`, `approval_outcome`) was
  not removed: the UI and control-mcp only ever send an option the agent
  offered, but a client's `RespondApproval` over the protocol is checked for
  its request id only, so this is the one place that path is checked.

## The maintainer's answers

- Harness versions: "Support only the new versions where possible, with no
  promise that old ones run correctly. Build in a minimum recommended version
  for each provider and say so when the installed one is below it."
- Tcode's own old records: "Readable, but they need not display as they did.
  A single generic fallback event can stand for every message type that is
  now obsolete; showing the raw line is enough."
- Codex: "For Codex, ChatGPT.app is the authority on how to drive the
  app-server." Checked against ChatGPT.app 26.1103: it fills model and effort
  from the user's config, then the model's own default, and only then
  `medium`; it retries an overloaded turn on its own only against app-servers
  older than 0.162; it sends an unusable elicitation answer back to the user
  rather than declining.
- Issues and pull requests: "Every PR and every issue is drafted by an agent.
  I cannot say whether they express what I want; that needs talking through."
- Assertion-free `#[ignore]`d manual tools in the test tree: delete.
- Scope: "It should be the whole of Tcode's implementation", tests included,
  not production code alone.
- On test waits: "Another of the most important problems: no wait in any
  test should consume the real clock."
- On the questions a round leaves: after the second round's 122 questions,
  "all of them as you recommend", with later rounds left to the session's
  judgement. The rules that answer stood on are the ones a later round
  applies without asking: cuts that do not say what they left out go, cuts
  that say so and offer the whole stay; a number the feature needs stays, one
  the source's documentation contradicts follows the documentation, one that
  only guards against runaway with no incident goes; security checks stay; a
  timeout the user cannot cancel stays until the cancel path exists; checks
  on Tcode's own data and messages go; a file of Tcode's own that cannot be
  read is an error, never an empty default written back over it; what only a
  real device can judge is left alone.
- On agents following the code they find: "That is what is wrong with the
  codebase as it stands: you carry on its old inertia without telling good
  from bad, and the old bad inertia is exactly what I want gone."

On tests kept because a defect was once fixed, and on a test said to hold the
motion the maintainer chose out of three prototypes:

> A bug is fixed, not fixed and then given a regression test. As long as the
> motion is right, and after several rounds it is, there is no need for a
> test to guard it. I did choose one of three. So what? What has that to do
> with a test? It cannot be there to make sure the animation is the one I
> chose and not one of two that do not exist. — the maintainer

On Stop while a harness is still starting:

> The interrupt is fine. It can wait until the harness has really started.
> We must not kill the process. — the maintainer, and then: it only needs
> to say it is stopping.

On a thread that followed a shell's `cd`:

> The model went there to look something up and did not come back, that is
> all. Why would the conversation move? — the maintainer

On a test added for a display rule the maintainer had just stated (a command
over five rows shows four):

> What is there to add? What is the reason to test a fixed behaviour like
> this? — the maintainer

A constant applied in one place is not a decision that needs proving: the
code that states it is the proof, and a test of it only repeats the number.

On whether the audit may build as well as delete:

> Building is not a problem in my eyes. Everything that benefits this
> codebase rather than harming it is what we are here for. — the maintainer

On running the app to look, and on what it costs:

> When did I say you could not verify? — the maintainer

> For a real run use the cheapest model. Do not carry on with the largest
> one just to test that a conversation works. — the maintainer

On checks before a push:

> Add Clippy to your own checks. — the maintainer, and then: there is no
> need to run all three platforms here; this machine runs its own.

## Defects met on the way

Not defences; fixed or reported as their own entries.

- pi refusing a steer emitted a generic `Error` and left the steer pending
  forever. Fixed: `SteerRejected` by request id.
- Stopping a session sends `Shutdown` to an actor that may be blocked and
  never read it. Open.
- While a relay awaits confirmation, a turn ending on the old process with a
  non-empty queue restarts onto the new provider with the old provider's
  resume cursor and no relay. Open; older than the audit.
- Codex's `wake()` returns early when the background-terminal list is due and
  may swallow a manual "Retry now". Open, unverified.
- A Claude Code `system/init` whose `cwd` differed moved the thread, so a
  shell `cd` left behind moved it. The thread follows a worktree entered or
  left, nothing else.
- A fabricated `Finished` three seconds after a dictation stopped cut off
  the end of long speech and sent without it. Removed; the wait is cancelled
  by the user.
- The runtime test driver `run_until_parked` does not process an input the
  way `pipe.rs::host_loop` does. Open.
