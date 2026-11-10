# Codex app-server fixtures

- `v2_messages.jsonl`: written by hand. Each line has the shape the
  codex-cli 0.167.0 app-server protocol gives it, generated with
  `codex app-server generate-ts --experimental`:
  `v2/TurnStartedNotification.ts` and `v2/TurnCompletedNotification.ts`
  (`v2/Turn.ts`), `v2/ItemStartedNotification.ts` and
  `v2/ItemCompletedNotification.ts` (the `fileChange` item of
  `v2/ThreadItem.ts`), `v2/FileChangeRequestApprovalParams.ts`,
  `v2/AgentMessageDeltaNotification.ts`,
  `v2/ReasoningSummaryTextDeltaNotification.ts`,
  `v2/CommandExecutionOutputDeltaNotification.ts` and
  `v2/ThreadTokenUsageUpdatedNotification.ts`. Ids, paths, times and text
  are invented.
- `mcp_tool_results.jsonl`: `item/completed` notifications of `mcpToolCall`
  items in the shape an app-server session sent them (`df411764c`; the
  `mcpToolCall` item of `v2/ThreadItem.ts`, with the `_meta` keys it
  carried). The content is invented: ids, titles, code, app ids and the
  query are made up, and the image is a 1x1 JPEG.
