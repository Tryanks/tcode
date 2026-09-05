# Resume after the Claude Code usage-limit reset

When Claude Code rejects a turn because the account's 5-hour window is exhausted,
the turn fails and the error lands on the timeline as an error card. This feature
lets the session pick the task back up when the window resets.

## Behaviour

- **Settings → Conversation → "Resume when the usage limit resets"** (default on).
- Claude Code emits `{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":<unix secs>,"rateLimitType":"five_hour",…}}`
  before the failed `result`. The Claude adapter remembers `resetsAt` and, on the
  failed result, emits `AgentEvent::UsageLimitReached { resets_at }` right after the
  `AgentEvent::Error`.
- The timeline folds `resets_at` into the preceding error entry
  (`EntryContent::Error { message, limit_resets_at }`).
- Runtime: when the toggle is on, the session schedules a turn with the text
  `Continue from where you left off.` (Claude Code's own resume prompt) at
  `resets_at`, via the existing scheduled-queue machinery (`push_scheduled` +
  `reschedule_scheduled_wake`). Nothing is scheduled when the toggle is off.
- Error card:
  - a scheduled resume exists for this `resets_at` → "Resumes in H:MM:SS" countdown + **Cancel** (drops the queued message);
  - no scheduled resume and `resets_at` is in the future → **Resume when limit resets** button (schedules the turn);
  - otherwise the plain card.
  The two states are a toggle: the card reads the composer queue, so cancel/resume
  flip it without any new state.

## Acceptance

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p agent -p tcode-core -p tcode-runtime -p tcode-protocol -p tcode-ui
```

Named review questions:
1. A `rate_limit_event` with `status: "rejected"` followed by a failed `result` yields `Error` then `UsageLimitReached { resets_at }` (adapter unit test).
2. With the toggle on, `UsageLimitReached` puts exactly one scheduled queue entry at `resets_at` on the session; with it off, none (runtime test).
3. Replaying a persisted transcript containing the new event sets `limit_resets_at` on the error entry and does not panic.
4. The toggle is in both locales; the parity test passes.
