# Cursor fixtures

All of these concern `cursor-agent` 2026.10.01-e373342 (macOS arm64). Two
kinds live here, and only the first is a recording.

## Recorded

Captured on 2026-10-03 from the real binary while signed out, through Tcode's
production path (`cargo run -p agent --example probe -- --list-models cursor`
and `probe cursor …`, with a wrapper that copied the stdio), in a sandbox
(`HOME` redirected, `AGENT_CLI_CREDENTIAL_STORE=file`, `NO_OPEN_BROWSER=1`).
The only edit: the JSON-RPC request ids are zero UUIDs with a serial number.

- `recorded_signed_out.jsonl` — what the binary wrote: the `initialize` result
  (line 1), then `-32000 Authentication required` for
  `cursor/list_available_models` (line 2) and for `session/new` (line 3).
- `status_signed_out.json` — `cursor-agent status --format json`.

No signed-in session has been recorded.

## Source-derived

Hand-built from the shipped JavaScript of the same build, not recorded:
the shapes are Cursor's, the contents made up.
Line numbers refer to the build's `3351.index.js` (the ACP server) after
`prettier --parser babel` 3.9.9, unless named otherwise. Model names, model
parameters, paths and texts are illustrative: Cursor's catalog comes from its
backend.

`source_derived.json`, by key:

- `list_available_models` — `cursor/list_available_models` (1457-1458,
  1575-1592).
- `turn_tools` — a live turn: `tool_call` (3630-3646, 3784-3801), then
  `in_progress` (3746-3749) and `completed` with `content`, `rawOutput`,
  `locations` (3753-3773). Shell: title, kind, input and
  `rawOutput {exitCode, stdout, stderr}` (4309-4311, 4922, 4502-4503,
  4757-4765). Edit: diff content (4345-4348, 4929, 4511-4515, 4638-4641,
  4679-4731). MCP: `{rejected: true}` (4403-4406, 4932, 4524-4530,
  4836-4846). Grep: `{totalMatches, truncated}` (4313-4340, 4925,
  4786-4805). Text and thoughts (3624-3629, 3686-3689, 3777-3781).
- `run_error` — a thrown agent run becomes the assistant chunk
  `"\n\nError: " + String(e)` (714-734), and the prompt still answers
  `end_turn` (409-426).
- `ask_question` — `cursor/ask_question` params (2039-2049); the reply shapes
  are read at 2053-2075.
- `update_todos`, `update_todos_merge` — `cursor/update_todos`, sent once the
  todo tool completes (3840-3856), statuses from 3902-3915.
- `subagent_turn` — a task tool (4433-4438, 4936, 4552-4559, 4868-4877), its
  `subagent_spawned` and `subagent_state_update` sent under the parent session
  (3158-3189), and the subagent's own updates under its session id
  (2956-2981, 3114-3131).
