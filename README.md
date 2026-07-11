# tcode

The interface follows the system language by default and supports English and Simplified Chinese in Settings.

A native macOS GUI for coding-agent CLIs, built with [GPUI](https://gpui.rs)
and [gpui-component](https://github.com/longbridge/gpui-component). tcode is a
thin desktop layer over the agents you already use — it spawns the official
CLIs and speaks their native protocols, so your accounts, models, and tooling
keep working unchanged.

tcode's design and interaction model are closely modeled on
[T3 Code](https://t3.gg) by T3 Tools — think of it as a native,
reduced-feature homage: the same sidebar/timeline/composer/diff experience,
reimplemented in Rust for two providers. All credit for the original UX design
goes to the T3 Code team.

Supported providers:

- **Claude Code** — spawns `claude`, bidirectional stream-json protocol
  (streaming deltas, tool-use permission prompts over the control protocol,
  session resume).
- **Codex** — spawns `codex app-server`, JSON-RPC protocol v2 (threads/turns,
  item events, command & file-change approvals, resume).

## Features

- Projects sidebar: threads grouped by project, live "Working" status,
  archive, ⌘K palette (thread search + actions).
- Chat timeline: streaming markdown, collapsible per-turn Work Log (commands,
  file changes, tool calls, reasoning), changed-files card with per-file
  +/- counts.
- Approvals: inline panel for command execution / file changes —
  Approve / Always allow (session) / Deny.
- Diff panel: per-turn syntax-highlighted unified diffs in a resizable split.
- Composer: model picker (favorites, ⌘1-9), live context-usage chip,
  interrupt, git branch display.
- Sessions persist as JSONL event logs; conversations resume across restarts
  via each provider's native resume.
- Light/dark themes.

## Build & run

Requires Rust (edition 2024) and macOS. First build compiles GPUI from source
and takes a while.

```sh
cargo build
cargo run
```

Provider CLIs are resolved from PATH (`claude`, `codex`) or configurable in
Settings → Providers.

### Development

```sh
cargo test --workspace

# headless end-to-end smoke (real provider CLI, auto-approves in a throwaway dir):
git init /tmp/smoke && cargo run -- --smoke "claude|/tmp/smoke|Reply with exactly: PONG"

# headless probes for the provider layer:
cargo run -p agent --example probe -- claude "Reply with exactly: PONG" /tmp/smoke
cargo run -p agent --example interrupt_probe -- claude /tmp/smoke
```

Debug launch flags: `--open-latest`, `--open-diff`, `--open-settings`,
`--open-palette`, and `--open-draft <project-id-or-name>` (opens a new-thread
draft for that project). See `docs/DESIGN.md` for the UI spec and the visual
verification protocol.

## Architecture

```
crates/agent   provider layer (no gpui): canonical AgentEvent model,
               codex + claude clients as actor tasks over stdio
src/           gpui app: session store (JSONL), timeline fold, UI surfaces
```

## License

MIT
