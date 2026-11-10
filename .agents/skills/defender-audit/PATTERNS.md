# Defences met so far

Cases from the first audit: the provider lifecycle (`crates/agent`,
`crates/runtime/src/app`), PR #1051. Paths are as they were at `0eebc05cf` and
may have moved. Each case is evidence, not a rule; the definition, the grounds
and the rewrite rules are in [SKILL.md](SKILL.md).

The maintainer's words are quoted from the session of 2026-11-10, translated
from Chinese. They decided entries then and are the likeliest to decide
entries in other areas.

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
- **Hand-typed wire lines** called fixtures: `codex/v2_messages.jsonl`, lines
  9–10 of Claude's `usage_scope.jsonl`. A defence that relied on one of these
  (`result_ids`, de-duplicating a `result` by uuid) has no recording behind
  it.
- What stood up instead: the interrupt change was shown with the probe
  against the real Claude Code (`--interrupt-after`), which still reported
  the turn interrupted.

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
- On agents following the code they find: "That is what is wrong with the
  codebase as it stands: you carry on its old inertia without telling good
  from bad, and the old bad inertia is exactly what I want gone."

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
- The runtime test driver `run_until_parked` does not process an input the
  way `pipe.rs::host_loop` does. Open.
