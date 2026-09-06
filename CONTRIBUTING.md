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

CI checks formatting, Clippy, workspace builds and tests on macOS, Windows and
Linux. Run the same checks locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
cargo test --workspace --locked
```

CI also runs `cargo machete` to catch unused dependencies, and checks iOS, Android
and Web with `RUSTFLAGS='-D warnings'`. Use the commands and tool version in
[the workflow](.github/workflows/ci.yml) to reproduce those checks. Fix warnings
at their source; a platform-specific lint exception needs a concrete reason at
the declaration. For a dependency imported under a different crate name, use
Cargo's `package` alias or machete's `renamed` metadata so unused imports remain
detectable. An `ignored` entry requires an explanation of the generated or
implicit use it cannot detect.

New user-facing strings must be added to **both** `locales/en.yml` and
`locales/zh-CN.yml` — a parity test enforces it.

If you changed the UI, also update [`docs/DESIGN.md`](docs/DESIGN.md). It is the
visual contract: when the code and the doc disagree, one of them is a bug.

## Verifying real behaviour

**Provider-layer probes** (no GUI — print the raw canonical event trace):
Use an existing working directory and image path in these examples.

```sh
cargo run -p agent --example probe -- claude "Reply with exactly: PONG" /tmp/smoke
cargo run -p agent --example probe -- claude "Run sleep 30, then reply DONE" /tmp/smoke --interrupt-after 5
cargo run -p agent --example probe -- codex "Run sleep 30, then reply DONE" /tmp/smoke --steer "Stop and reply PONG"
cargo run -p agent --example probe -- claude \
    "What color is this image? Reply with just the color." /tmp/smoke --image /tmp/blue.png
```

`TCODE_DATA_DIR` points tcode at a throwaway profile (its own sessions, settings
and installed ACP agents) — useful for demos, screenshots and trying a change
without touching your real threads.

**Launch flags** for driving surfaces that need a running app: `--open-latest`,
`--open-diff`, `--open-settings`, `--open-palette`, and
`--open-draft <project>`.

## Code layout

```
crates/core              pure domain types and semantics
crates/services          persistence, filesystem, process, git, import, and probes
crates/runtime           session and provider lifecycle, queues, orchestration,
                         terminals, and semantic events
crates/ui/src/i18n.rs     translation backend
crates/ui                GPUI views, assets, presentation, and localized rendering;
                         `run_shell` is the one bootstrap every client opens
crates/app/src/main.rs   desktop binary and composition root
crates/ios, crates/android, crates/web
                         platform bootstrap only: a ClientHost, a window seam,
                         and a call into `tcode_ui::run_shell`
crates/headless          headless host binary
crates/agent             provider clients (no GPUI) — claude.rs, codex.rs, acp.rs
crates/term              terminal implementation (PTY)
crates/preview-mcp       MCP server exposing the preview browser to the agent
crates/orchestrate-mcp   MCP server for orchestration tools
```

The dependency direction is strictly downward: `app -> ui/runtime/services`;
`ui -> runtime/core`; `runtime -> services/core` and lower adapters such as
`agent` and `term`; and `services -> core`. No lower layer depends upward.
Runtime emits semantic events; UI owns their localization and presentation.
`crates/app/src/main.rs` composes the desktop app. It is the default workspace
binary, so the normal source command remains `cargo run`.

`crates/agent/src/lib.rs` is the contract between the two halves: every provider
normalizes into one `AgentEvent` stream and accepts one `SessionCommand` enum, so
the UI never learns anything provider-shaped. When changing it, verify every
client with the workspace checks above, including the full-workspace build.

**Adding a provider** usually means writing one client in `crates/agent` that
translates its wire protocol into `AgentEvent`, and nothing else. If you find
yourself special-casing a provider inside `crates/ui/src`, that's a sign the
contract is missing something — say so in the PR.

Never spawn a child process with `std::process::Command::new` directly: use the
process helpers in `crates/services/src/process.rs` and `crates/agent/src/process.rs`,
which suppress the console window on Windows and resolve binaries
against `PATH`/`PATHEXT`. A guard rejects direct `Command::new` usage.

## Review

### Keep one owner for each behaviour

Before adding a type, state field, helper or dependency, find its current owner
and callers. Extend that owner when it already represents the same concept.
An abstraction should hide a real policy, platform boundary or lifecycle; a
forwarder or fallible signature needs a responsibility beyond passing values
through. Compute derived state where it is consumed unless caching has a
measured benefit and an explicit invalidation path. Remove obsolete callers,
conversions, fixtures and dependencies with the code they supported.

Keep documentation with the behaviour it describes: update or retire plans,
reference assets and development scripts when their work is complete. Link to
the owner instead of copying API lists or values. Comments should explain a
constraint or a reason the code cannot express; remove narrated steps, empty
section headings and historical progress notes. Verify old comments and tests
against the intended contract before preserving their claims.

### Make tests earn their maintenance

Each test should identify an observable contract or realistic failure that it
would catch. Derive expected values from that contract, a recorded external
fixture or a known regression, independently of the implementation. In
particular, a serialization round trip alone cannot establish wire
compatibility: assert literal messages or older persisted inputs.

Use the smallest production entry point that exercises the behaviour. Keep
fixtures limited to setup; assertions should exercise production logic rather
than an algorithm recreated inside the test. Group inputs that protect the
same behaviour when that removes repeated setup, while keeping distinct
failure modes readable. Library behaviour, constant/getter wiring and repeated
happy paths need a project-specific reason to be tested.

Unavailable credentials or platform facilities must appear as explicit ignored
tests with a reason, rather than an early return reported as a pass. Keep
deterministic local integration tests running in CI where supported.

When removing or merging tests, identify the redundant coverage or retired
contract, and where any remaining contract is covered. Preserve checks for
permissions, malformed or older data, cancellation, ordering and recovery when
those behaviours are still supported. For a bug fix, demonstrate that the
regression test fails without the fix when practical. Test counts and deleted
line counts are not quality targets.

### Evidence in the pull request

Describe the changed behaviour, why added abstractions are needed or removed
ones are redundant, and the contract protected by changed tests. Report the
checks actually run, including any platform or live-service gaps. Review these
criteria for both human and AI contributions; passing CI cannot decide whether
an abstraction or a test is useful. Merge only after all checks on the final
commit pass, including dependency hygiene and mobile/Web checks.
