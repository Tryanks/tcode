# Contributing to Tcode

Thanks for taking the time. Tcode is a small project — bug reports, UI polish and
provider work are all welcome. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md).

This file has two parts. **Principles** is the one text the maintainer has
personally reviewed; every other document, comment and issue in this repository
is derived from it. **Process** is how to build, check and submit work.

## Principles

1. **Tcode is a native GUI proxy for TUI coding agents.** When a native harness
   already has a capability, Tcode surfaces it as it is instead of building its
   own equivalent. Tcode never hides, alters or replaces what the model would
   see or do through the native CLI. Anything Tcode injects into a model — MCP
   tools, prompts, transcripts — is visible to the user. Capabilities a harness
   does not have (for example rewind outside Claude Code) are not synthesized.

2. **Host and client.** One Tcode host owns all state: providers, threads,
   terminals, projects. Every client — the desktop window, a phone, a tablet, a
   browser tab — is a screen for that host and speaks the same protocol whether
   it runs on the same machine or across a network. Layers depend strictly
   downward: `app → ui/runtime/services`, `ui → runtime/core`,
   `runtime → services/core` and lower adapters, `services → core`.

3. **Six native providers, then ACP.** Claude Code, Codex, pi, OpenCode, Cursor
   and Grok are maintained natively, permanently, over each CLI's own protocol.
   No seventh native integration is planned; every other agent connects over
   ACP, and ACP registry entries that duplicate a native integration are hidden.

4. **One canonical event stream.** Every provider normalizes into one
   `AgentEvent` stream and accepts one `SessionCommand` set
   (`crates/agent/src/lib.rs`). The stream is the union of what all providers
   can emit: an event only one provider produces still enters the union and is
   handled uniformly, so a provider that never emits it simply appears not to
   have it. The UI never branches on the provider kind.

5. **Remote access.** The target transport is iroh, end to end encrypted, as the
   only transport for native clients; the browser client uses iroh's browser
   support over a relay. An official service, **Traverse**, provides discovery
   and relay fallback: no accounts, on by default, self-hostable, sees only
   ciphertext, and never required — on a LAN, direct connections give the full
   product without it. Abuse is limited without accounts: the relay only carries
   traffic between paired peers, discovery, pairing and relay bandwidth are
   rate-limited per node and per IP, a pairing code dies after five failures on
   the service as it does locally, and no SLA is promised. Current status and
   plan: [#376](https://github.com/Tryanks/tcode/issues/376).

6. **Orchestrate is core.** Multi-agent orchestration is a core capability and
   keeps iterating. The long-term plan is a WASM plugin framework with
   Orchestrate as its first plugin; the plugin API is designed first, and until
   it exists nothing is restructured toward plugins.

7. **No house UI principles.** Tcode follows the recommendations and defaults of
   gpui-base and gpui-kit. There is no separate design contract to keep in sync
   with the code.

8. **Code and tests are the only source of truth.** This section is the only
   maintainer-authored text; comments, issues and other documents are derived
   from it and are corrected when they disagree with it or with the code. A
   comment is kept only when it states a constraint the code cannot express or
   the reason an obvious alternative was rejected; narrated steps, history and
   "mirrors X" notes are deleted when touched. A constraint drafted by an AI is
   not a requirement until it appears here.

9. **Adopt upstream early, never fork it.** Prefer an upstream's experimental
   feature and feed findings back over waiting for it to mature. Do not maintain
   a fork of a dependency: when upstream declines a fix, Tcode carries the bug
   and waits.

## Process

### Reporting bugs and asking for features

Open an [issue](https://github.com/Tryanks/tcode/issues). For a bug, include
your OS, which agent (Claude Code / Codex / which ACP agent), what you did, what
happened and what you expected. A screenshot beats a paragraph for anything
visual. If you are unsure whether something is a bug or intended, open an issue
anyway — the answer is worth writing down either way.

### Building

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

Provider CLIs are resolved from `PATH` (`claude`, `codex`, `pi`, `opencode`) and
can be overridden in **Settings → Providers**.

### Before you open a pull request

CI checks formatting, Clippy, workspace builds and tests on macOS, Windows and
Linux. Run the same checks locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
cargo test --workspace --locked
```

The CI workflow always starts so its required check names are reported, but it
plans the affected scope before allocating build runners. Documentation-only
changes and wording-only edits to bundled Orchestrate prompts skip heavyweight
checks; Rust changes run the Cargo checks for affected packages and their
dependents. Cargo, build, workflow and unclassified input changes fall back to
the full workspace. When reporting local evidence, run the full commands above
unless you are reproducing the narrower scope printed by the CI planning job.

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

If you changed something visual, launch the surface and look at it in both
themes and at a wide and a narrow width; compile and unit checks alone do not
establish visual correctness. `cargo run -p tcode-ui --example phone` opens the
shared shell at phone geometry without a device.

### Verifying real behaviour

**Provider-layer probes** (no GUI — print the raw canonical event trace).
Use an existing working directory and image path in these examples.

```sh
cargo run -p agent --example probe -- claude "Reply with exactly: PONG" /tmp/smoke
cargo run -p agent --example probe -- claude "Run sleep 30, then reply DONE" /tmp/smoke --interrupt-after 5
cargo run -p agent --example probe -- codex "Run sleep 30, then reply DONE" /tmp/smoke --steer "Stop and reply PONG"
cargo run -p agent --example probe -- claude \
    "What color is this image? Reply with just the color." /tmp/smoke --image /tmp/blue.png
```

`TCODE_DATA_DIR` points Tcode at a throwaway profile (its own sessions, settings
and installed ACP agents) — useful for demos, screenshots and trying a change
without touching your real threads.

**Launch flags**: `--open-latest` reopens the most recent thread,
`--connect <host_id>` starts attached to a paired host, `--pair <addr> <port> <code>`
pairs from the command line, and `--preview-smoke` / `--cu-smoke` run the
preview and computer-use smoke phases against the live desktop.

Native platform behaviour — macOS permission grants, input delivery, camera
pairing on a phone — is only established by exercising it on that platform.
Ignored tests that need a desktop slot or credentials say so in their reason;
run them deliberately, never report an early return as a pass.

### Code layout

```
crates/core              pure domain types and folds (the event → timeline fold
                         is shared by live streams and replay)
crates/protocol          serializable client ↔ host contract, PROTOCOL_VERSION
crates/client            transport-agnostic client endpoint; ClientHost is the
                         platform-capability contract
crates/services          persistence, filesystem, process, git, import, probes
crates/runtime           session and provider lifecycle, queues, orchestration,
                         terminals, semantic events; AppState is reached only
                         through serialized protocol messages
crates/remote            remote transport, pairing, discovery, multi-client mux,
                         preview proxy
crates/ui                the one GPUI shell every client opens via `run_shell`;
                         owns localization (crates/ui/src/i18n.rs) and presentation
crates/app/src/main.rs   desktop binary and composition root (default `cargo run`)
crates/headless          headless host binary
crates/ios, crates/android, crates/web
                         platform bootstrap only: a ClientHost and a call into
                         `tcode_ui::run_shell`
crates/platform/*        GPUI platform backends for iOS and Android
crates/agent             provider clients (no GPUI) — claude.rs, codex.rs, pi.rs,
                         opencode.rs, acp.rs
crates/term              terminal implementation (PTY, host-side emulator)
crates/mcp-host          shared authenticated loopback host for in-process MCP servers
crates/preview-mcp       MCP server exposing the preview browser to the agent
crates/orchestrate-mcp   MCP server for orchestration tools
crates/computer-use-mcp  MCP server for desktop automation (macOS, Windows)
crates/voice             macOS dictation; explicit unsupported elsewhere
```

Runtime emits semantic events; UI owns their localization and presentation.

**Adding a provider** means writing one client in `crates/agent` that translates
its wire protocol into `AgentEvent`, and nothing else. If you find yourself
special-casing a provider inside `crates/ui/src`, the contract is missing
something — say so in the PR. Changing `crates/agent/src/lib.rs` requires the
full-workspace build, because every client consumes it.

Never spawn a child process with `std::process::Command::new` directly: use the
process helpers in `crates/services/src/process.rs` and
`crates/agent/src/process.rs`, which suppress the console window on Windows and
resolve binaries against `PATH`/`PATHEXT`. A guard rejects direct
`Command::new` usage.

### Review

**One owner per behaviour.** Before adding a type, state field, helper or
dependency, find its current owner and callers, and extend that owner when it
already represents the same concept. An abstraction should hide a real policy,
platform boundary or lifecycle; a forwarder or fallible signature needs a
responsibility beyond passing values through. Compute derived state where it is
consumed unless caching has a measured benefit and an explicit invalidation
path. Remove obsolete callers, conversions, fixtures and dependencies with the
code they supported. Keep documentation with the behaviour it describes and
link to the owner instead of copying values.

**Tests earn their maintenance.** Each test identifies an observable contract
or a realistic failure it would catch, with expected values derived from that
contract, a recorded fixture or a known regression — not from the
implementation. A serialization round trip alone cannot establish wire
compatibility: assert literal messages or older persisted inputs. Use the
smallest production entry point that exercises the behaviour; assertions
exercise production logic, not an algorithm recreated in the test. Library and
upstream behaviour, constant/getter wiring and repeated happy paths need a
project-specific reason to be tested. When removing or merging tests, say which
coverage was redundant and where any remaining contract is still covered. For a
bug fix, show the regression test fails without the fix when practical. Test
counts and deleted line counts are not quality targets.

**Evidence in the pull request.** Describe the changed behaviour, why added
abstractions are needed or removed ones redundant, and the contract protected
by changed tests. Report the checks actually run, including platform or
live-service gaps. The same criteria apply to human and AI contributions;
passing CI cannot decide whether an abstraction or a test is useful. Merge only
after all checks on the final commit pass, including dependency hygiene and the
mobile and Web checks.

### Release signing

The [release workflow](.github/workflows/release.yml) signs and notarizes the
macOS app and DMG when all six `MACOS_*` secrets are configured, publishes
unsigned builds when none are, and fails naming the missing ones when the set is
partial. The credentials must come from the maintainer's Apple Developer
account; the secret names, how to obtain each value and how to verify a signed
build are documented in the workflow next to the signing step. Windows builds
ship unsigned. The Android APK is a release build signed with Gradle's debug
keystore until a release key exists.
