# VM screenshot session seed

This directory is a complete `SessionStore` fixture for `tools/vm-screenshot.sh --seed`.
It contains the current `sessions.json` index (one project and one session) and
the session's timestamped JSONL event log. The VM installer also creates the
fixture's `/Users/admin/tcode-demo` working directory.

To regenerate it, construct `Project` and `SessionMeta` values and persist them
with `SessionStore::persist_index`, then append canonical `AgentEvent` values
with `SessionStore::append_event`. Copy the resulting `sessions.json` and
`<session-id>.jsonl` here, keeping the project/session paths rooted at
`/Users/admin/tcode-demo`. Validate changes with the services store tests and a
seeded light/dark VM screenshot run.
