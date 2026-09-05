Shim only: the published `gpui-pre-platform` 0.3.3 plus one fallback arm in
`current_platform` so the crate compiles on targets it has no backend for.

Why it exists: `gpui-base` 0.6.0 declares `gpui-pre-platform` as a hard
dependency on every non-wasm target without using it, so iOS/Android builds
of anything on top of `gpui-base` fail inside that crate. tcode itself never
calls `gpui_platform` on mobile or in the browser; every entry point supplies
its own `gpui::Platform` via `Application::with_platform`.

Upstream fix (prepared, see docs/plans/remote-and-mobile.md Decision 9):
gpui-kit PR "base: stop depending on gpui_platform from the library" moves the
dependency to dev-dependencies. Once a gpui-base release with that change is
in Cargo.lock, delete this directory and the `[patch.crates-io]` entry.
