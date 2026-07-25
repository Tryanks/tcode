# tcode sync protocol

The wire contract between a **host** (a machine that can actually run agents:
spawn provider CLIs, own the repo, hold the session log) and a **client** (a
phone, browser, tablet, or another desktop that cannot).

`crates/sync-protocol` is normative. This document explains the shape and the
reasoning; when the two disagree, fix one of them — deliberately.

Background and the decision to make sync a prerequisite rather than a third
feature: `docs/multiplatform-plan.md` §1, §3.

---

## 1. Shape

One host, many clients, **single writer**. The host owns all state; clients
hold only presentation state (scroll offset, which panel is open, unsent draft
text). There is no offline editing and no merge: a client that cannot reach its
host can queue user input and replay it on reconnect, and a conflict is an
error the user retries. Agent sessions are inherently single-writer, so a CRDT
would double the work to model a problem we do not have.

Transport is WebSocket with JSON text frames. Nothing in the frame definitions
depends on that choice; the crate is transport-agnostic types plus serde, with
no I/O, so it builds for wasm.

## 2. Why these types already exist

The protocol did not need a new vocabulary. All five providers — Codex, Claude,
pi, OpenCode, ACP — already normalize into one canonical stream, and the
existing seam is literally commands-in / events-out:

```rust
// crates/agent/src/lib.rs
pub struct SessionHandle {
    pub commands: async_channel::Sender<SessionCommand>,
    pub events:   async_channel::Receiver<AgentEvent>,
}
```

So the wire is `SessionCommand` one way and `AgentEvent` the other. Both are
serde types. A remote session becomes another `start_session` backend rather
than a parallel universe, and `Timeline::fold_events` — a pure reducer — folds
replicated events into exactly the timeline a local replay would produce.

## 3. Ordering and cursors

Every persisted event carries a **per-session `seq`**: 1-based, contiguous,
strictly increasing (`crates/services/src/store.rs`).

`seq` exists because `ts` cannot do this job. `ts` is a wall clock that the
writer explicitly tolerates repeating or going backwards, so two events can
share a millisecond and a clock adjustment can invert a pair. A cursor built on
`ts` would silently skip or duplicate events at exactly the moments that matter.

A cursor is therefore just "the last `seq` I have". Resume is exact, not
best-effort: the log is append-only and the sequence is dense, so `from_seq: N`
means "send `N+1` onward" with no ambiguity about what is missing.

## 4. Frames

### Client → host

| frame | meaning |
| --- | --- |
| `Hello { min_version, max_version, client, token }` | First frame on every connection. Nothing else is accepted before it. |
| `ListSessions` | Enumerate sessions this token may see. |
| `Subscribe { session_id, from_seq }` | Stream a session. `from_seq: None` means from the beginning. |
| `Unsubscribe { session_id }` | Stop streaming; the session keeps running on the host. |
| `Command { session_id, command }` | A `SessionCommand` — send a turn, approve, interrupt, steer, rewind. |
| `Ping { nonce }` | Liveness. Mobile networks drop idle connections silently. |

### Host → client

| frame | meaning |
| --- | --- |
| `Welcome { version, host }` | Handshake accepted; `version` is the agreed one. |
| `Refused { reason }` | Handshake rejected. Carries the host's supported range on a version mismatch, so the client can say something better than "failed". |
| `SessionList { sessions }` | Answer to `ListSessions`. |
| `Events { session_id, events, caught_up }` | A batch. `caught_up: false` while replaying backlog, `true` once live. |
| `CommandRejected { session_id, command_ref, reason }` | The host could not apply a command. Silence would leave the client showing a turn that never ran. |
| `SessionEnded { session_id, reason }` | The provider exited or the session was archived/deleted. |
| `Pong { nonce }` | |

## 5. Version negotiation

The client sends a **range**, not a version, and the host picks the highest
value both support. A single version forces host and client to upgrade in
lockstep, which is not a thing you can ask of an App Store release.

On no overlap the host answers `Refused { reason: UnsupportedVersion {
host_min, host_max } }` — enough for the client to tell the user which side is
too old.

This is the answer to multi-device version drift, and it is why the
"ship one wasm UI artifact to every client" idea was rejected rather than
adopted (`docs/multiplatform-plan.md` §8.1): drift is a compatibility problem,
and it is far cheaper to negotiate compatibility than to make every client
execute the same binary.

## 6. Delivery guarantees

**Host → client is exactly-once, by construction.** Events are numbered and the
client tracks its cursor, so a duplicate batch is discarded by `seq` and a gap
is impossible without the client noticing.

**Client → host is at-least-once, made idempotent by the existing
`delivery_id`.** `SessionCommand::SendTurn` already carries one, and
`AgentEvent::TurnAccepted` already echoes it back — a mechanism that predates
this protocol and exists because the local UI has the same question after a
provider restart. A client that reconnects mid-turn compares the `delivery_id`s
it sent against the `TurnAccepted` events it can now see, and resends only what
is genuinely missing. No new dedup layer.

Commands without a natural idempotency token (`Interrupt`, `RespondApproval`)
are either harmless to repeat or scoped by a `request_id` the host can match
against state it already holds.

## 7. Deliberately not in v1

- **Pairing.** v1 carries a pre-shared token. The pairing-code exchange is host
  UX, and putting it in the wire format before that UX exists would be guessing.
- **A general app-level command surface.** `AppState` exposes 209 public
  methods; the protocol should carry the ones a remote client is shown to need,
  discovered by building the client, not by translating the whole surface up
  front.
- **Binary framing.** JSON is legible and debuggable. Revisit when a measurement
  says to, not before.
- **Multi-host federation.** One client talks to one host at a time.
