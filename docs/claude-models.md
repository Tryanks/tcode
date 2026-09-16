# Claude Code model list

Tcode does not hand-maintain the Claude Code model catalog. It reads the
[t3code model manifest](https://github.com/pingdotgg/t3code/blob/main/apps/server/src/provider/model-manifest.json)
(MIT licensed): the `claudeAgent` section supplies each model's name, options
(reasoning effort, context window, fast mode, thinking), the CLI flag mapping
(`effortMap`, `[1m]` suffixes, context window sizes) and the minimum Claude Code
version that can run it. Upstream data is adopted as is; there is no local
override table.

The owner is `crates/agent/src/claude_manifest.rs`, which bundles a verbatim
copy (`claude_model_manifest.json`) as the offline fallback and refreshes it
from t3code `main` when the model catalog is listed (app start and provider
reload). The fetch runs at most once an hour, backs off five minutes after a
failure, times out after 10 s, caps the response at 1 MiB and is skipped
entirely when **Settings → Provider update checks** is off. The last good
remote manifest is cached as `claude-model-manifest.json` in the Tcode data dir
(`TCODE_DATA_DIR` or the platform data dir); a bundled copy with a newer
`updatedAt` outranks that cache. Invalid data never replaces a usable manifest,
and a failed fetch never fails the model list.

To refresh the bundle, copy the upstream file over
`crates/agent/src/claude_model_manifest.json`.
