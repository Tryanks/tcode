---
name: defender-audit
description: "Remove defences from one area of Tcode the maintainer describes, code and tests alike: inventory every defence, keep the ones with grounds, rewrite the rest so the source speaks for itself. Argument: a description of the area, e.g. \"persistence and migration\"."
disable-model-invocation: true
---

# Defender audit

Much of Tcode's implementation was written by agents following the code
they found, not anything the maintainer asked for. One run takes the area the
maintainer describes, in the whole implementation (production code, tests,
fixtures, CI and the seams between them), finds every **habit** in it that
has no grounds, and removes it. The **defence** is the largest family of such
habits and the one this skill defines closely; the families are an open list,
and [Habit](#habit) says what to do with what fits none.

The authority is the **Principles** section of
[CONTRIBUTING.md](../../../CONTRIBUTING.md); read it first. This skill and
[PATTERNS.md](PATTERNS.md) are notes derived from one audit (PR #1051) and
bind no one: where they disagree with the Principles or with what the
maintainer asks, those win. Read PATTERNS.md before starting; it holds the
real cases, including the ones where the inventory was wrong.

## Defence

Tcode is a proxy. Each thing it shows or does has a **source**: the harness
says what happened, the user says what they want, Tcode's own code and stored
data say what Tcode did. A **defence** is anything in the implementation
that puts Tcode's own belief about a source where the source should be:
production code that acts on distrust of the source instead of passing it
through, and a test that proves the code against Tcode's imitation of the
source instead of the source. Nothing has failed where a defence stands; it
is there in case.

Recognise one by what it does to the source. Principle 1 names the first
three:

| It | Meaning | Cases from PR #1051 |
|---|---|---|
| **hides** | drops something the source said | an emptied todo list, a question without a header, a model when the CLI version is unknown, a failure written only to the log |
| **alters** | passes it on changed | a turn failed because one tool failed, an interrupt guessed from prose, a report cut to 1200 characters, a failed import counted as skipped |
| **replaces** | answers in the source's place | cancelling the harness's open question, rejecting its permission request, filling in a model or effort the user did not pick, a fresh session where a resume failed |
| **cuts short** | ends or fails it for being slow | a timeout, a deadline, a watchdog |
| **refuses** | will not do what the source allows | a path outside the cwd, a compact during a turn, a check repeated behind the boundary that already made it |
| **invents** | adds what no source said | a second spelling of a wire field, a made-up turn id, a branch for a state the code cannot reach, a capability the harness does not have |
| **stands in** | speaks for the source in a test | a shell script written to answer as `claude` or `codex` would, a fake server returning what the test expects, wire data in a shape no schema or source of the harness backs, a mock that implements the behaviour asserted |

And by the source it distrusts, which is where to look:

- **the harness's result**: verified, re-judged, or overridden after the
  harness reported it;
- **the harness's speed**: every `Duration` constant that ends in an error;
- **the harness's version**: branches for older CLIs, fallbacks when a request
  is refused, capabilities withheld because Tcode cannot read them back;
- **the harness's conversation with the user**: prompts, permissions and
  consents Tcode answers, drops or pre-accepts;
- **Tcode's own data**: validation, second spellings and migrations for what
  Tcode itself wrote;
- **Tcode's own code**: a check already made upstream, a branch for an
  impossible state, a panic caught and carried on from;
- **the user's patience**: a failure kept from them, an output shortened for
  them;
- **the real harness, in tests**: every `#!/bin/sh` written by a test, every
  `Fake`, `Stub` or stand-in type, every fixture that does not say where its
  shape comes from.

A stand-in proves what Tcode believes the harness does. The harness speaks
for itself in its own schema, types, documentation and source, which give a
fixture its shape, and in a **probe** against the real CLI (CONTRIBUTING's
**Verifying real behaviour**). A file that only has to exist, or a scripted provider at
Tcode's own `AgentEvent` boundary, imitates no harness and is not one.

Not a defence: reporting a real failure as it happened; a branch the
harness's wire protocol requires; translating a provider into the one
`AgentEvent` stream (Principle 4); a feature the maintainer asked for.

## Proving a harness integration

To prove that Tcode calls a harness binary with the right arguments and
understands its reply, the proof is cut where Tcode's own code is cut, and no
test spawns anything that imitates the harness:

1. **What Tcode sends.** Arguments, environment and request bodies are built
   by a pure function production itself calls; the test asserts its return
   value. No exec is injected to inspect them: an injection parameter is a
   production seam only the test uses.
2. **What Tcode understands.** A fixture goes into the parser or mapper and
   the test asserts the events. The fixture's **shape** is the harness's:
   taken from its schema, types, documentation or source, and the fixture
   says which (harness version, file). Its **content** is invented. Captured
   output of a real run is never committed: it carries paths, account data
   and conversation content, and this repository is public; recordings
   already in it held the maintainer's home directory and the names of other
   projects. A shape with nothing of the harness behind it is a stand-in in
   the form of data.
3. **The conversation.** Handshake order, request and response pairing, and
   cancellation are the actor's state machine: feed it lines, read the
   requests it emits. No process.
4. **Tcode's own process handling.** A child that exits, its stderr tail, a
   kill: use a real trivial process that claims to be no harness
   (`sh -c 'exit 3'`).
5. **That the halves meet**, and that the real binary accepts what Tcode
   sends: only the real harness shows it. That is the probe, run on purpose
   and reported, not a unit test.

## Grounds

A defence stays only on one of three grounds, named with its source:

1. **Fact**: without it the product does not work, or a Principle states the
   behaviour (the five-failure invitation limit and rate limits of Principle
   5, compatibility between releases of Principle 9).
2. **Incident**: a specific issue, pull request or fix commit whose symptom
   this defence prevents. A regression test guarding it is a lead to that
   incident, judged by the incident.
3. **The maintainer's word**: in the Principles, or said by the maintainer in
   the session. Every issue and pull request in this repository is drafted by
   an agent, so their text alone is not this ground; ask.

"A comment says so", "a document says so" and "it looks safer" are not
grounds. Neither is "the code around it does the same": see
[Habit](#habit). Principle 1 is a second test for every defence: a check that hides,
alters or replaces what the harness would do in its native CLI is itself the
problem.

## Habit

A defence is rarely alone. An agent that met one in the area it was working
in wrote the next one the same way, and the codebase now carries each as a
**habit**: the same shape in every provider, every test module, every start
path. What this audit removes is the habit, and the habit is also the main
risk to the audit itself, because the agent doing the work reads the same
code and reaches for the same shape.

- Existing code is the thing being judged, never the model for new code and
  never grounds. "Every other adapter has this timeout" and "the tests here
  all use a fake binary" are findings.
- Judge what you write by the definition of a defence, not by its neighbours.
  In PR #1051 the rewrite agents, asked for a test, each copied the fake
  harness already in their file; all four tests were deleted.
- When the inventory finds a habit, record it once, with every place it
  occurs, and ask the maintainer about the habit rather than each instance.
- A habit the maintainer has ruled on goes into [PATTERNS.md](PATTERNS.md),
  so the next area starts from the ruling instead of the precedent.

Defences are not the only family. Others the maintainer has named:

- **waiting on the clock**: a test that sleeps for seconds, or waits out a
  deadline, where it should wait for the state it is about;
- **slow by construction**: a single test that takes minutes in CI;
- **able to hang**: a race that, lost, never ends, held in check only by a
  runner's kill timer.

The list is open, and the worst of the codebase is not confined to it. In
the inventory, report anything in the lane that the maintainer would not have
asked for and that has no ground, whether or not it has a name here: say what
it is, where else the same shape occurs, and what it costs. Fit it to a
family afterwards, or name a new one.

## Steps

Inventory and rewrite are separate rounds with the maintainer between them.

1. **Scope.** The maintainer gives an area, not paths. Follow it through the
   code from its entry points to every layer it touches and list the files,
   then cut the list into **lanes**: sets of files with no file in two lanes,
   each small enough for one agent to read in full (about 10 000 lines).
   An ambiguous description gets the wider reading, stated.
   Done when every file the area touches is in exactly one lane or is named as
   out of scope with the reason.

2. **Inventory**, read-only, one agent per lane ([Dispatch](#dispatch)).
   Each defence is recorded with: location; the source it distrusts and what
   it does to it; the real user path on which that shows; where it came from
   (`git log -S`, the commit and the reason given then); a proposed verdict of
   remove, keep or ask, with its ground; and for a removal, what the user sees
   afterwards and who recovers. The area's tests are in the same inventory: each
   stand-in, and each test that asserts a defence. Their other faults go
   through the [`test-audit` skill](../test-audit/SKILL.md) in the same round.
   Done when every place in the lane where Tcode hides, alters, replaces, cuts
   short, refuses, invents or stands in has been judged, every other habit
   the agent met is reported, and each report states what was read and what
   was not.

3. **Check the inventory.** Open the code at each proposed removal and confirm
   the report describes it. Move to "ask" whatever is a product or security
   choice rather than a defensive reflex. Merge the same defence found in
   several lanes into one entry.
   Done when every proposed removal has been read in the code by the main
   session or is marked unchecked.

4. **Ask the maintainer.** Present the removals grouped by pattern, the keeps
   with their grounds, and the open questions as yes/no questions, the ones
   that decide the most entries first. Wait for the answers.
   Done when every "ask" entry has an answer or is deferred by the maintainer.

5. **Rewrite**, one agent per lane in its own worktree, following
   [Rewrite rules](#rewrite-rules). Each entry is verified again in the code
   before it is changed; an entry that does not hold is left alone and
   reported with the evidence.
   Done when each entry is done, or not done with a reason, and nothing the
   removed defence existed for (constant, field, helper, error variant, comment,
   test, fixture) remains.

6. **Review and land.** Read each lane's diff before taking it. Leave out any
   commit whose own stated precondition failed. Take the rest in order onto
   one branch, and run the lane's crates after each lane and the checks in
   CONTRIBUTING's **Before you open a pull request** once all are in. Where a
   change alters what a real harness run shows, run the probe in
   **Verifying real behaviour**.
   Done when the checks pass on the final commit and the pull request names
   each entry left out and why.

7. **Record.** Add to [PATTERNS.md](PATTERNS.md) the cases this run produced
   that the file does not already carry: a new pattern, a keep with new
   grounds, an inventory entry the rewrite overturned, a maintainer answer
   that will decide later entries.
   Done when a later run in another area would not have to rediscover them.

## Rewrite rules

- **Verify, then change.** The inventory was written by another agent and is
  wrong often enough to matter. Read the code and its callers first.
- **A timeout goes only where the user can already stop the thing.** Find the
  cancel path (stop the session, interrupt the turn, kill the process) and
  confirm it reaches this code. Where the wait blocks the very loop that would
  deliver the stop, the timeout is the only recovery: leave it, and report
  that the cancel path comes first.
- **Find what else answers for the defence before removing it.** Another layer
  may depend on the answer the defence gives (a fold that clears the pending
  request, a restart that relies on the cleared cursor). Removing one half
  leaves the harness waiting on something the user can no longer see.
- **Delete, do not wrap.** No compatibility wrapper, no fallback switch, no old
  and new side by side.
- **Delete downstream with it.** Whatever existed only for the defence goes too.
  Feed nothing a made-up value to keep a remaining consumer satisfied.
- **Remove the check, keep the capability.** What the user could do before,
  the user can do after.
- **Add no mechanism.** No new threshold, counter, retry, timeout,
  compatibility layer or setting. A failure that must now be reported goes out
  through a path that already exists.
- **Read one shape.** A wire field is read as the harness sends it, confirmed
  from the harness's own schema, source or documentation.
  An event missing a field the harness always sends is not guessed at.
- **Leave contracts alone inside a lane.** A change to `AgentEvent`,
  `SessionCommand`, `crates/protocol` or the locale files is reported as
  needed, and done as its own entry.
- **Tests follow CONTRIBUTING.** A test asserting a removed defence is deleted
  with it; an assertion is never rewritten to the new output.
- **Prove with the source.** A behaviour the rewrite makes true is shown by a
  fixture in the harness's own shape put through the adapter, or by a probe
  against the real harness, run and reported. Where neither can reach it, say so in the
  pull request; write no stand-in to fill the gap.

## Dispatch

- Write the brief to a file and confirm it exists before dispatching; each
  agent's prompt is its lane, its report path and the brief's path. The brief
  carries the definition of a defence, the grounds, the record format and, for the
  rewrite round, the rewrite rules and the maintainer's answers.
- Use the model and effort the maintainer names for delegated work.
- Inventory agents write nothing but their report. Rewrite agents each get an
  isolated worktree and a lane of files no other agent has, commit locally,
  and neither push nor open a pull request.
- Tell every agent that a comment, a document, an issue or the surrounding
  code's own practice is not grounds, that what it writes is judged by the
  definition and not by its neighbours, and that an entry it cannot verify is
  left alone and reported.
- Rewrite agents run only their own crate's tests; the main session runs the
  workspace after merging.
- Read every report's claims against the code before acting on them. In PR
  #1051 an inventory called a test seam uncalled that had a dozen callers,
  and a rewrite found three inventory entries wrong.

## Handoff

Report to the maintainer:

- what was removed, by pattern, and what each now does instead;
- what was kept, with its ground;
- what was left out of the rewrite and the precondition it waits on;
- the tests removed or changed and where each contract is still proved;
- the checks actually run, and the ones not run;
- defects met on the way that are not defences;
- the questions that remain, as yes/no questions.
