# gpui-component → gpui-base migration plan

Goal: drop the styled `gpui-component` layer; keep only `gpui-base` (headless
behavior), `gpui-component-assets` (SVG icons, gpui-only), and `gpui-wry`
(webview, gpui-only). tcode owns all visual styling.

Upstream: https://github.com/longbridge/gpui-component — base split landed
2026-08-13 (PR #2677). Target rev: `c1acb9f3854238da9543af817453726a0efbadb0`.
Base API is young; pin revs, upgrade only at phase boundaries.

Facts established during research (2026-08-13):
- `InputState`/`InputEvent`, `h_flex`/`v_flex`, `StyledExt`, `ElementExt`,
  resizable family, `PopoverState`, `Disableable`, `Selectable` are base types
  re-exported by the styled layer → import-path changes only.
- Base `ActiveTheme` is `pub(crate)`; base `Theme` = 17 semantic color tokens +
  radius/spacing/typography/shadow. No dark/light mode, no JSON registry.
- Styled-layer-only (must rebuild in tcode): Icon/IconName (353 uses),
  Button+variants (221), theme system (577 `cx.theme()` calls),
  Notification/Root/WindowExt, Input chrome, Tooltip chrome, HighlightTheme,
  Kbd, Spinner, Sizable, set_locale.
- Base has full text engine (IME/selection/history) and ToastManager/ToastStack.
- Known upstream breaking change at split: `ScrollbarShow` → `ScrollbarMode`.

## Phases (each ends: builds green, committed)

- [x] Phase 0 — bump pin to post-split rev `c1acb9f`, align gpui via cargo
      update, fix compile (ScrollbarShow→ScrollbarMode etc.). Both layers
      coexist; no behavior change intended.
      Accept: `cargo build --workspace --examples` green; app launches.
- [x] Phase 1 — tcode-owned theme: `Theme` type + `ActiveTheme` trait in
      crates/ui (mirror the field names currently used so `cx.theme()` call
      sites survive), serde-load `themes/tcode.json`, own dark/light mode,
      replace `gpui_component::init` theme parts.
      Accept: build green; theme switching works in running app.
- [x] Phase 2 — owned primitives styled over base: Icon (keep assets crate),
      Button facade (only variants tcode uses), Input chrome, Tooltip chrome,
      Spinner, Kbd. Migrate call sites.
      Accept: build green; gallery example renders all primitives.
- [x] Phase 3 — notifications over base ToastManager + own overlay layer;
      dialogs/sheets over base Dialog/Sheet; retire WindowExt/Root usage.
      Accept: build green; toast + dialog flows work in app.
- [ ] Phase 4 — sweep remaining `gpui_component::` refs, remove
      `gpui-component`/`gpui-component-macros` deps (keep assets + gpui-wry),
      full workspace build + clippy + tests.
      Accept: `grep -r gpui_component crates --include=*.rs` → only
      gpui_component_assets; workspace build/clippy/tests green.

Push to `redesign/beautiful-ui-chat` when all phases done.
