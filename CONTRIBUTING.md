# Contributing to tcode

Thanks for taking the time. tcode is a small project — bug reports, UI polish and
new provider work are all welcome.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Reporting bugs and asking for features

Open an [issue](https://github.com/Tryanks/tcode/issues). For a bug, the useful
things to include are: your OS, which agent (Claude Code / Codex / which ACP
agent), what you did, what happened, and what you expected. A screenshot beats a
paragraph for anything visual.

If you're unsure whether something is a bug or intended, open an issue anyway —
the answer is worth writing down either way.

## Building

You need a recent Rust toolchain (edition 2024). The first build compiles GPUI
from source and takes 10–20 minutes; later builds are fast.

```sh
git clone https://github.com/Tryanks/tcode
cd tcode
cargo run
```

Platform prerequisites:

- **macOS** — Xcode command-line tools.
- **Windows** — MSVC toolchain; the WebView2 runtime for the preview browser.
- **Linux** — a Vulkan driver plus the usual GPUI build deps (`libxkbcommon-dev`,
  `libwayland-dev`, `libxcb*`, `libssl-dev`, `libasound2-dev`, `libfontconfig-dev`).
  The embedded preview browser is compiled out on Linux.

Provider CLIs are resolved from `PATH` (`claude`, `codex`) and can be overridden
in **Settings → Providers**.

## Before you open a pull request

CI runs all four of these on macOS, Windows and Linux, and it is not permitted to
be yellow — no `continue-on-error`, no `#[allow]` used to dodge a lint:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build            # must be warning-free
cargo test --workspace
```

New user-facing strings must be added to **both** `locales/en.yml` and
`locales/zh-CN.yml` — a parity test enforces it.

If you changed the UI, also update [`docs/DESIGN.md`](docs/DESIGN.md). It is the
visual contract: when the code and the doc disagree, one of them is a bug.

## Verifying real behaviour

Unit tests don't prove a provider works. There are headless paths for that.

**End-to-end smoke** (spawns the real CLI in a throwaway directory,
auto-approves, exits non-zero on failure):

```sh
git init /tmp/smoke
cargo run -- --smoke "claude|/tmp/smoke|Reply with exactly: PONG"
cargo run -- --smoke "codex|/tmp/smoke|Reply with exactly: PONG"
```

**Provider-layer probes** (no GUI — print the raw canonical event trace):

```sh
cargo run -p agent --example probe -- claude "Reply with exactly: PONG" /tmp/smoke
cargo run -p agent --example interrupt_probe -- claude /tmp/smoke
cargo run -p agent --example steer_probe -- codex /tmp/smoke
cargo run -p agent --example image_probe -- claude /tmp/blue.png \
    "What color is this image? Reply with just the color." /tmp/smoke
```

`TCODE_DATA_DIR` points tcode at a throwaway profile (its own sessions, settings
and installed ACP agents) — useful for demos, screenshots and trying a change
without touching your real threads.

**Launch flags** for driving surfaces that need a running app:
`--open-latest`, `--open-diff`, `--open-settings`, `--open-palette`,
`--open-draft <project>`; and for screenshots, `--debug-compose <text>`,
`--debug-image <path>`, `--debug-live`, `--debug-send <text>`,
`--debug-palette <query>`,
`--debug-settings-section <general|providers|orchestrate|archived>`,
`--debug-queue "msg1|msg2"`, `--debug-edit-open`.

`--debug-edit-resend "<text>"` exercises **Edit & resend** end to end (rewind the
worktree from the turn's git checkpoint, truncate the transcript, roll the
provider session back, send a new turn) — something you cannot drive by hovering
a bubble headlessly:

```sh
git init /tmp/er
cargo run -- --smoke "claude|/tmp/er|Create agent.txt containing hello."
cargo run -- --debug-edit-resend "Create edited.txt containing bye."
# agent.txt is gone; the transcript starts at the edited message; edited.txt exists.
```

## Code layout

```
crates/core              pure domain types and semantics
crates/services          persistence, filesystem, process, git, import, and probes
crates/runtime           session and provider lifecycle, queues, orchestration,
                         terminals, and semantic events
crates/i18n              the sole translation backend
crates/ui                GPUI views, assets, presentation, and localized rendering
crates/app/src/main.rs   the sole binary and composition root
crates/agent             provider clients (no GPUI) — claude.rs, codex.rs, acp.rs
crates/term              terminal implementation (PTY)
crates/preview-mcp       MCP server exposing the preview browser to the agent
crates/orchestrate-mcp   MCP server for orchestration tools
```

The dependency direction is strictly downward: `app -> ui/runtime/services/i18n`;
`ui -> runtime/core/i18n`; `runtime -> services/core` and lower adapters such as
`agent` and `term`; and `services -> core`. No lower layer depends upward.
Runtime emits semantic events; UI owns their localization and presentation.
`crates/app/src/main.rs` is the sole binary and composition root, so the normal
workspace command remains `cargo run`.

`crates/agent/src/lib.rs` is the contract between the two halves: every provider
normalizes into one `AgentEvent` stream and accepts one `SessionCommand` enum, so
the UI never learns anything provider-shaped. Changing it means touching every
client — do it deliberately, and never land it without a full-workspace build.

**Adding a provider** usually means writing one client in `crates/agent` that
translates its wire protocol into `AgentEvent`, and nothing else. If you find
yourself special-casing a provider inside `crates/ui/src`, that's a sign the
contract is missing something — say so in the PR.

Never spawn a child process with `std::process::Command::new` directly: use the
process helpers in `crates/services/src/process.rs` and `crates/agent/src/process.rs`,
which suppress the console window on Windows and resolve binaries
against `PATH`/`PATHEXT`. A guard rejects direct `Command::new` usage.

## Review

I read every PR. Small, focused changes get merged quickly; large ones go faster
if you open an issue first so we can agree on the shape. Claims get verified —
"the tests pass" is checked by running them, so please don't guess.
