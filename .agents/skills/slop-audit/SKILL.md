---
name: slop-audit
description: "Invoke before writing or changing a test, before adding a timeout, fallback, retry, validation or version branch, when a test fails or is met beside a change, and to audit an area of Tcode for habits with no grounds, code and tests alike."
---

# Slop audit

Much of Tcode was written by agents following the code they found, not
anything the maintainer asked for. What they left is **habit**: the same
shape in every provider, every test module, every start path, with nothing
behind it. This skill is the bar new code and tests are written to, and the
way an area already written is brought to it.

The authority is the **Principles** section of
[CONTRIBUTING.md](../../../CONTRIBUTING.md); read it first. Everything here
is derived from five rounds of audit and binds no one: where it disagrees
with the Principles or with what the maintainer asks, those win.

- Writing code or a test: [The gate](#the-gate), and the definitions under it.
- Auditing an area the maintainer names: read [AUDIT.md](AUDIT.md) for the
  order of work, dispatch and handoff.
- [PATTERNS.md](PATTERNS.md) holds the cases met, the maintainer's rulings
  and what the audit itself got wrong. Read it before an audit, and when a
  judgment here is close.

## The gate

Before writing code that stands between a source and what Tcode shows or
does, answer:

1. Which **source** speaks here, and does this pass it on as given?
2. If it does anything else, which [ground](#grounds) is it on?
3. If it waits with a limit, how does the user stop the thing today?

Before adding or keeping a test, answer:

1. Which **decision** of Tcode's does it prove?
2. What change to Tcode would make it fail?
3. Which test already proves that, and at what boundary? One contract has
   one owner, at the strongest boundary that reaches it.
4. Does it need anything production does not: a `cfg(test)` item, a
   `test-support` accessor, a widened visibility, an injected parameter?
   Then it moves to the real boundary.

A missing answer means it is not written, or it goes.

## Source and defence

Tcode is a proxy. Each thing it shows or does has a **source**: the harness
says what happened, the forge and git say what is there, the user says what
they want, Tcode's own code and stored data say what Tcode did. A
**defence** puts Tcode's belief about a source where the source should be.
Nothing has failed where one stands; it is there in case.

| It | Meaning | Cases |
|---|---|---|
| **hides** | drops something the source said | a failure written only to the log, a question without its header, rows past a limit nobody mentions |
| **alters** | passes it on changed | a Completed command re-judged Failed from its text, a selection trimmed on copy, a report cut to 1200 characters |
| **replaces** | answers in the source's place | cancelling the harness's open question, a default model for an unknown profile, a fresh session where a resume failed |
| **cuts short** | ends or fails it for being slow | a timeout, a deadline, a watchdog |
| **refuses** | will not do what the source allows | a client-side check the host already makes, a path outside the cwd |
| **invents** | adds what no source said | a second spelling of a wire field, an empty id for a missing one, a branch for a state the code cannot reach |
| **stands in** | speaks for the source in a test | a script answering as `claude` would, a fake server, wire data in a shape nothing of the harness backs |

Where to look, by the source distrusted: the harness's result, speed,
version and conversation with the user; the forge's and git's answers;
Tcode's own data and code; the user's patience and input; the host's state
as the client holds it; the real peer, in tests.

Moving a guess is not removing it. An adapter that parses the text layout of
harness output holds the same belief a window guessing by tool name did.
What a source says is the field it gives; where it gives only text, the text
is shown as written, and a layout the adapter does not recognise reaches the
user unchanged.

Reporting a real failure as it happened, a branch the peer's protocol
requires, translating a provider into the one `AgentEvent` stream, and a
feature the maintainer asked for are none of these.

## Grounds

A defence stays on one of three, named with where it comes from:

1. **Fact**: the product does not work without it, or a Principle states the
   behaviour: the limits and admission of Principle 5, compatibility between
   releases of Principle 9, reading data a released version wrote.
2. **Incident**: a fix commit or issue whose symptom this prevents.
3. **The maintainer's word**: in the Principles, or said in the session.
   Every issue and pull request here is drafted by an agent; their text is
   not this ground.

A comment, a document, "it looks safer" and "the code around it does the
same" are not grounds. Security checks stay: authorization, admission, path
containment, secrets kept from other threads.

A limit on what reaches the model or the user stays when it is physical (a
transport frame, a tool result's size) or when it says what it left out and
the whole can be had. A timeout goes where the user can already stop the
thing; where they cannot, the cancel path is built first.

## What a test proves

A test proves a **decision** Tcode makes: which thread shows, what command
goes to which host, which pane holds the keyboard, whether the reader stays
where they were, whether a source is shown as it gave itself, whether data a
released version wrote still reads, who is admitted.

How a **primitive** behaves is the dependency's to prove: how a scroll
bounces, which rows a virtual list builds, how a dock divides a width, what
path an animation takes, when a tooltip opens; serde's round trip, git's own
status, a terminal library's parsing. These tests go, and a defect once
fixed there keeps none of them.

A bug is fixed; no test is written for having fixed it. A test is written
when the fix shows a decision had none: one test, at its owner, failing on
the code before. Motion and looks the maintainer has seen need no test to
say they are still as chosen.

Also gone on sight:

- nothing asserted, or a value compared with itself;
- the expected value computed by the code under test, or copied from its
  table: a match arm, a wording list, a colour;
- a private predicate's truth table, a private field read back, a call
  count, a cache hit, exact English that is no part of a protocol;
- one contract proved again at a second layer, or by each provider for a
  fold the core owns;
- a mock that implements what is asserted; a fixture that supplies the
  answer the owner should produce;
- a negative that passes for another reason; a name that promises more than
  the assertions deliver;
- a test that passes and fails on one tree (Principle 10): deleted where
  met, named in the pull request with where its contract still stands, or
  that it does not.

A kept test asserts the decision and nothing on the way to it: it carries
no frame count, pixel offset or private state, is named for what it proves,
and its scene is the smallest that proves it.

## Proving a harness integration

The proof is cut where Tcode's own code is cut, and no test starts anything
that imitates the harness:

1. **What Tcode sends**: arguments, environment and bodies are built by a
   pure function production calls; assert its return value.
2. **What Tcode understands**: a fixture through the parser or mapper. Its
   **shape** is the harness's, from its schema, types, documentation or
   source, and the fixture says which version and file. Its **content** is
   invented. Output of a real run is never committed: it carries paths,
   accounts and conversations, and the repository is public.
3. **The conversation**: handshake order, pairing and cancellation are the
   actor's state machine. Feed it lines, read what it emits.
4. **Tcode's process handling**: a real trivial process that claims to be
   no harness (`sh -c 'exit 3'`).
5. **That the halves meet**: only the real harness shows it. That is the
   **probe** in CONTRIBUTING's **Verifying real behaviour**, run on purpose
   and reported.

A scripted provider at Tcode's own `AgentEvent` boundary imitates no
harness. Codex is integrated as ChatGPT.app integrates it.

## Time

A test's wait spends no real time. It waits on an event or a state: a
channel, a condition, a process exiting, a window at rest. Behaviour that is
about time runs on a clock the test advances; production takes time from
one owner (the host's clock, the executor's) that the app and the test both
go through, and a place that reads the wall clock itself is the finding. A
bound only a failure reaches is the runner's. A client counts from when it
saw a thing, never by comparing its clock with the host's stamp.

## Building

Deleting is not the goal and building is not a fault: a cancel path, a typed
error, a field a source can say for itself are built when the codebase is
better for them. Say what a change leaves better and what it costs. Write it
by the definitions here, not by its neighbours: the surrounding code is what
is being judged.
