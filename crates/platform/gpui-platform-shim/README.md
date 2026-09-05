Shim only: the published `gpui-pre-platform` 0.3.3 plus one fallback arm in
`current_platform` so the crate compiles on targets it has no backend for.

Why it exists: `gpui-base` 0.6.0 declares `gpui-pre-platform` as a hard
dependency on every non-wasm target without using it, so iOS/Android builds
of anything on top of `gpui-base` fail inside that crate. tcode itself never
calls `gpui_platform` on mobile or in the browser; every entry point supplies
its own `gpui::Platform` via `Application::with_platform`.

Upstream fix: longbridge/gpui-kit#2962 (issue) and #2963 (PR) move the
native examples into their own package so `gpui-base` no longer declares
`gpui_platform`. Once a gpui-base release with that change is in Cargo.lock,
delete this directory and the `[patch.crates-io]` entry.
