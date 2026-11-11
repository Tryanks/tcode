# Auditing an area

One run takes the area the maintainer describes, in the whole implementation
(production code, tests, fixtures, CI and the seams between them), finds
every habit in it that has no grounds, and brings it to the bar in
[SKILL.md](SKILL.md). Each step ends on its criterion; the next starts
after.

The first run in a codebase keeps reading and changing as separate rounds
with the maintainer between them: the answers are what later runs go by.
Once [PATTERNS.md](PATTERNS.md) holds rulings that cover a lane, one agent
reads the lane and changes it in the same pass, decides by those rulings,
and brings back only what none of them settles. Go ahead on your own
judgment for what a ruling covers and for what was never forbidden; a
question the maintainer asks out of curiosity is a question.

## Steps

1. **Scope.** The maintainer gives an area, not paths. Follow it from its
   entry points to every layer it touches, and cut the files into **lanes**:
   no file in two, each small enough for one agent to read whole (about
   10 000 lines). An ambiguous description gets the wider reading, stated.
   Done when every file the area touches is in one lane or named out of
   scope with the reason.

2. **Read and change**, one agent per lane in its own worktree
   ([Dispatch](#dispatch), [Rules for a change](#rules-for-a-change)). For
   each thing found the lane's report gives: where; which source and what
   is done to it; where it came from (`git log -S`, the commit and its
   reason); what the user sees now. It lists what it kept and on which
   ground, what it deleted without being sure, and what it read and did not
   read.
   Done when every place in the lane that hides, alters, replaces, cuts
   short, refuses, invents or stands in has been judged, and every other
   habit met is named with where else it occurs.

3. **The tests, twice.** The first pass removes tests that are wholly about
   a primitive, a dependency or a private table. The second goes by what
   the first left: count the **marks** before it starts (frame and clock
   stepping, assertions on a position or size, fixed loops, tests over a
   second and a half), then turn each into a wait for the window to rest,
   an assertion taken out of a kept test, or a test deleted.
   Done when the report gives, for each mark, how many remain and why each
   stays, and the count and summed time of the tests before and after.

4. **Take it in.** Read each lane's diff; open the code at what it claims.
   Leave out a commit whose own precondition failed. Take the rest onto one
   branch, and after every lane run Clippy, the formatter and the
   workspace's tests for this machine's platform: lanes that ran at once
   break each other where they meet. The other platforms are CI's.
   Done when the checks pass on the final commit and the pull request names
   what was left out and why.

5. **Across the layers.** What a lane could not do inside its files (a
   contract, the host and the UI changed together) is written down as it is
   met and done afterwards by **theme**, one agent following one thing
   through every crate it needs. A wire change goes in the note above
   `PROTOCOL_VERSION`; stored data a released version wrote still reads,
   with a test that says so.
   Before the themes are cut, count the written-down items against the
   code: some were done by a later lane and some never held. Each that is
   still there gets one of three words: do, leave with the reason, or ask.
   Done when each written-down item is done, shown not to hold, or carried
   forward with what it waits on.

6. **Look at it running** ([Running the app](#running-the-app)), before the
   next round starts.
   Done when each changed surface is marked seen, wrong or not reached with
   the reason, and what was wrong is fixed or written down.

7. **Record.** Add to PATTERNS.md what this run produced that it does not
   carry: a habit named, a keep with new grounds, a claim the code
   overturned, an answer of the maintainer's that will decide later
   entries, a mistake of the audit's own.
   Done when a run in another area would not have to find them again.

## Rules for a change

- **Verify, then change.** A report was written by another agent and is
  wrong often enough to matter. Read the code and every caller first.
- **Find what else answers for it.** Another layer may depend on what a
  defence gives. Removing one half leaves the source waiting on something
  the user can no longer see.
- **Delete, do not wrap**, and delete downstream with it: the constant,
  field, helper, error variant, comment, test and fixture that existed only
  for it. Feed nothing a made-up value to keep a consumer satisfied.
- **Remove the check, keep the capability.** What the user could do before,
  the user can do after.
- **Read one shape.** A wire field is read as the source sends it,
  confirmed from its schema, source or documentation.
- **A failure that must now be reported** goes out by a path that exists,
  in the source's own words.
- **A test asserting what was removed goes with it**; an assertion is never
  rewritten to the new output, and a seam is never added to keep a test.
- **A leftover helper is removed by its file**, not its name: the same name
  elsewhere may be production. Deleting tests also leaves what only another
  platform's build shows, such as a module compiled under
  `cfg(any(test, …))` for its tests' sake.
- **A defect met on the way** is fixed in its own commit, without a test
  for the fix.
- **What only the real thing proves is run**: a path that reads a CLI's
  output, a control that kills a process, a page nothing headless draws.
  Say in the report that it was run and what it showed, or that it was not.
- **What is new is judged by the definitions**, not by the code beside it.

## Dispatch

- Write the brief to a file and confirm it exists first. It carries the
  definitions, the grounds, the maintainer's rulings and words, the lane,
  and the report's form. Tell every agent that a comment, a document, an
  issue or its neighbours' practice is not grounds.
- Use the model and effort the maintainer names. That covers what a lane
  starts in turn: its own subagents, and any thread it opens in a running
  app.
- Each lane has its own worktree, commits locally, and neither pushes nor
  opens a pull request. It runs its own crates' checks; the main session
  runs the workspace after each merge.
- A lane may start read-only subagents to read for it and checks their
  findings in the code before acting. They count against the session's
  limit on concurrent agents, and every agent shares one allowance of
  requests to the code host. Fetch a CI log once and keep it.
- A lane stops what it started before it ends; stopping a lane does not
  stop its subagents or watchers.
- A fix for something broken on the main branch goes up alone and first;
  the rest of the round is stacked on it.
- A round outlives a quota. Keep a note of what is merged, what is open,
  which lanes stopped where and what comes next, where a session that
  remembers nothing can read it; a stopped lane keeps its commits and its
  working tree and is told to go on.
- Build output is per worktree and per revision. Each lane cleans its
  target when it ends, and the main checkout's is pruned after each merge:
  a full disk stops every lane at once.

## Running the app

- Start a second instance on a throwaway data directory (`TCODE_DATA_DIR`),
  away from the maintainer's data and the maintainer's own window.
- A window that is fully covered does not redraw. Bring the instance's own
  window to the front.
- A thread opened to try a conversation uses the cheapest model at its
  lowest effort, chosen before the first prompt, and the title model is set
  to the same before it. A model the maintainer named that is not in the
  list is reported with the list, not replaced.
- The isolation is from the running Tcode and its data. A harness run the
  maintainer authorised uses that harness's own home as it is; what it
  leaves there is not cleaned up and not a finding.
- A probe of Stop or cancel stretches the moment it needs with a wrapper set
  as the harness executable in the throwaway profile, and samples processes
  while it presses, so that "gone" has evidence.
- An accessibility press is not a click: without a press action of its own a
  control is pressed at the place it last was. A press that lands elsewhere
  is a finding about the control, and says nothing about a real click.
- What a walk-through did not see is not thereby absent, and synthetic
  input can mislead. When a finding is about feel, start the same build for
  the maintainer and ask what they see.
- Screenshots and the report carry no real session content, path, name or
  address.

## Handoff

Report to the maintainer:

- what changed, by pattern, and what each now does;
- what was kept, with its ground;
- what was deleted without being sure, so it can be read again;
- the tests removed, with where each contract is still proved or that none
  is; counts and summed time before and after;
- the checks run and the ones not run, and what has still not been looked
  at in the running app or run against a real harness: that list is a debt,
  and the next round waits on it;
- defects met and what became of them;
- what is carried forward and what each item waits on.
