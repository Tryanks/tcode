# Provider data gaps

Inventory of data that provider CLIs/protocols put on the wire but tcode does not yet
capture. Compiled from a full audit of the adapter parsers in `crates/agent/src/`
(claude, codex, opencode, pi, acp) against recorded fixtures and protocol schemas.

Already captured as of this document's introduction (see `feat/provider-data-capture`):
per-message/turn served model + model-change events (claude, codex, opencode), claude
structured stop reasons, cost (claude/opencode/acp) and provider-reported turn duration
(claude), claude `structuredPatch` real diffs, opencode exit codes.

Legend: **slot exists** = a field on `AgentEvent`/`TokenUsage`/`ItemContent`/`TurnMeta`
could hold it today; **new plumbing** = needs a new event variant or field.

## Cross-cutting themes

| Theme | Providers offering it | Status |
|---|---|---|
| Rate limits / quota (`resetsAt`, used %, plan type, credits) | claude (`rate_limit_event`), codex (`account/rateLimits/updated`) | Entirely dropped; no representation anywhere. Largest new-plumbing item. |
| Reasoning-token split (`thinking_tokens`, `reasoningOutputTokens`, `tokens.reasoning`) | claude, codex, opencode | Dropped or folded into `output_tokens`; needs a `TokenUsage` field. |
| Cache-write token split | claude (`cache_creation.ephemeral_{5m,1h}`), codex (`cacheWriteInputTokens`), pi (`cacheWrite`) | Only flat `cache_creation_input_tokens` survives (claude); others dropped. |
| Provider-reported per-item timing | codex (`startedAtMs`/`completedAtMs`), opencode (`state.time.*`, `part.time.*`) | Dropped; `TurnTiming` is reconstructed from tcode's own clock. |
| stdout/stderr split for shell tools | claude (`tool_use_result.stdout/stderr`), codex (`stdout`/`stderr` alongside `aggregatedOutput`) | Collapsed into one output string; `ItemContent::CommandExecution` has a single `output`. |
| Structured stop/finish reasons | pi (`stopReason`), opencode (`step-finish.reason`), acp (`StopReason`) | Collapsed to Completed/Failed; claude now captured, others still dropped. |

## claude (`crates/agent/src/claude.rs`)

### Whole events dropped

- `rate_limit_event` — status, `resetsAt`, overage state. New plumbing.
- `content_block_start` / `content_block_stop` — earliest tool-call start signal
  (tool cards currently appear only when the full `assistant` line arrives) and block
  boundaries. New plumbing in the stream path.

### `system/init`

- `tools` (enabled tool list), `mcp_servers` (incl. connection **status** — tcode never
  learns whether Claude actually connected its MCP servers), `agents` (subagent types),
  `permissionMode` (actual effective mode), `plugins`, `capabilities`, `apiKeySource`,
  `output_style`, `cwd`, `fast_mode_state`.
- `claude_code_version` — currently obtained via a redundant `claude --version`
  subprocess even though the init line carries it.

### Stream / assistant events

- `message_start.message.usage` — prompt-size reading at turn start (context gauge could
  update before the turn ends). `ttft_ms` — time to first token.
- `message_delta.usage.output_tokens_details.thinking_tokens`, `usage.iterations[]`.
- Live usage has no denominator: the `message_delta` path passes `context_window: None`;
  only `result.modelUsage.contextWindow` fills it at turn end.
- Content blocks: `redacted_thinking` (silently skipped — renders as a gap),
  `server_tool_use` / `web_search_tool_result` (web search results vanish entirely),
  `document`, `image`. `tool_use` blocks' `caller.type`.
- `request_id` (for support escalation).

### Tool results (`user` events)

- `tool_use_result.originalFile` (pre-edit content — exact before/after diffing,
  rewind previews), `filePath` (authoritative resolved path), `userModified`.
- Bash: `stdout`/`stderr` split, `interrupted` flag. `exit_code` is currently
  fabricated from `is_error` (0/1).
- `MultiEdit.edits[]` / `NotebookEdit` — only path-level FileChange, no diff.
- TodoWrite `activeForm` — present-tense label Claude's own UI uses for the running step.

### `result` (turn end)

- `modelUsage.<model>.*` per-model breakdown (tokens, `costUSD`, `maxOutputTokens`,
  `webSearchRequests`) — only `contextWindow` is read.
- `num_turns`, `api_error_status`, `terminal_reason`, `ttft_ms`, `usage.service_tier`,
  `usage.speed`, `permission_denials` (which tools were denied this turn — natural
  Warning/work-log row).

### Approvals / control

- `control_request.tool_use_id` — supplied by the CLI but dropped in the general path,
  so approval prompts can't be linked to their timeline tool card.
- `control_request.display_name`, `permission_suggestions` (stored verbatim, never shown).

### Subagents (`subagent_tail.rs`)

- Child `message.model`, `usage`, `stop_reason`, `tool_use_result` payloads, and the
  entire child `result` line (per-subagent usage/cost) are dropped.
- `task_notification.usage` (`duration_ms`, `tool_uses`, `total_tokens`) — per-subagent
  cost; `ItemContent::Subagent` has no slot.
- `compact_boundary.compact_metadata` (`trigger`, `pre_tokens`, `post_tokens`) — the
  "context compacted" row can't say how much was compacted.

## codex (`crates/agent/src/codex.rs`)

### Never-handled notifications (dropped at the `_ =>` arm)

`account/rateLimits/updated` (windows, `usedPercent`, `resetsAt`, credits, `planType`,
`spendControlReached`), `model/safetyBuffering/updated`, `model/verification`,
`thread/compacted` (token deltas), `thread/name|goal|settings/updated`,
`turn/moderationMetadata`, `item/mcpToolCall/progress`, `item/reasoning/summaryPartAdded`,
`item/autoApprovalReview/*`, `mcpServer/startupStatus/updated`, `skills/changed`,
`account/updated`.

### Turn / usage

- `turn.error` on `turn/completed` — a failed turn's reason never reaches the UI
  (any unrecognized status silently maps to Completed).
- `turn.durationMs`, `turn.startedAt`/`completedAt`, per-item `startedAtMs`/`completedAtMs`.
- `TokenUsage` gaps: `reasoningOutputTokens`, `cacheWriteInputTokens`, and the whole
  `total.*` breakdown (only `total.totalTokens` survives).
- Typed error taxonomy on `error` notifications (`errorType`, `failureStage`,
  `httpStatusCode` — `contextWindowExceeded`, `usageLimitExceeded`, …) — only
  `message` + `willRetry` are read.

### Items

- `commandExecution`: `stdout`/`stderr` split, `parsedCmd` (friendly rendering codex's
  own TUI uses), `cwd`, `processId`.
- `fileChange`: rename destination `movePath` is used only as a presence test — the
  target path itself is thrown away; `autoApproved` flag.
- `webSearch.results` — only the query is kept.
- `mcpToolCall`: `connectorId`, `appName`, `actionName`; error collapsed to
  `error.message`.
- `dynamicToolCall`: `namespace` (same-name tools collide), `callId`, `success`/`error`.
- `reasoning`: summary vs raw content flattened into one string; delta
  `summaryIndex`/`contentIndex` section boundaries dropped (sections run together).
- Unmapped item types render as raw JSON via `ItemContent::Other` — notably
  `enteredReviewMode`/`exitedReviewMode` with full structured code-review findings
  (`reviewOutput`, `overallCorrectness`, per-finding fields).

### Requests / responses

- Approval requests: server-supplied `decisions` list is ignored (options hardcoded);
  `approvalId` dropped (multiple approvals can share one `itemId` — misrouting risk);
  `parsedCmd`, `grantRoot`, `environmentId`.
- `thread/start` response: `serviceTier`, `approvalPolicy`, `sandbox`,
  `activePermissionProfile`, `ThreadSettings` (effort, collaborationMode, …),
  `ThreadStatus.activeFlags` (`usageLimited`, `budgetLimited`, `blocked`, `paused`).
- `model/list`: `inputModalities` (image support never checked), `upgradeInfo`.
- Known dead code: the `turn/start` response is read at `/result/turn/id` but the real
  shape is `{turnId}` — the read never matches (benign; `turn/started` fills it).

## opencode (`crates/agent/src/opencode.rs`)

- `message.updated`: `info.time.created/completed` (timestamps used only as booleans),
  `info.parentID`, `info.mode`, `info.agent`, `info.path.*`; `info.error` flattened
  (loses `error.name`/`data`).
- `step-finish`: `reason` (finish reason), `tokens.reasoning` (folded into output),
  `tokens.cache.write` (only summed into `used`).
- Tool parts: `state.metadata` beyond `exit` (stdout/stderr split, description),
  `state.title`, `state.time.start/end` (per-tool duration on the wire, unread).
- `patch` parts: kind forced to Modify, `diff: None`, `part.hash` dropped.
- `session.diff`: `additions`/`deletions` counts.
- `session.status` retry: `attempt` flattened into a warning string.
- Permissions: `always[]` options, `time`.
- Whole part kinds: `step-start`, `snapshot`, `file`, `agent`, `todo`. Whole events:
  `message.removed`, `message.part.removed`, `session.updated/deleted`, `file.edited`,
  LSP events.

## pi (`crates/agent/src/pi.rs`)

- `usage.cacheWrite`; `stopReason` (only `"error"` is distinguished).
- `turn_end.toolResults[]`, `agent_end.messages[]`.
- Tool results: exit code hardcoded `None`; non-text result parts dropped; edit old/new
  strings present in args but only `path` is read (`diff: None`).
- Retry/compaction/extension events flattened into free-text warnings
  (`attempt`/`maxAttempts`/`delayMs` lose structure).
- `message_update` types other than text/thinking deltas (`toolcall_start`,
  `toolcall_delta`) silently dropped.
- `get_state.data` fields beyond `sessionId`/`sessionFile`/`model` never enumerated.

## acp (`crates/agent/src/acp.rs`)

- `usage.thought_tokens`, `usage.cached_write_tokens`.
- `stopReason` discriminant (Refusal / MaxTokens / MaxTurnRequests collapse to Failed
  plus prose).
- `session_info_update` — entire variant ignored (agent-supplied session title,
  `updatedAt`).
- `TerminalExitStatus.signal` — signal-killed commands render with no exit info.
- `locations[].line` (no follow-along cursor); image/audio tool content replaced with
  `"[image]"`/`"[audio]"` markers.
- `PlanEntry.priority`; `AvailableCommand.input` (argument hint).
- Mid-session model change via `config_option_update` re-emits options only — the
  recorded session model goes stale.
