//! Windows-only caption controls: minimize, maximize/restore, close.
//!
//! On Windows the window is client-decorated (`crates/app/src/main.rs` marks the
//! titlebar transparent there), so the system draws no caption buttons and the
//! app must supply them. macOS keeps its native traffic lights and Linux keeps
//! its system titlebar, so on both this module renders nothing.
//!
//! The window has no titlebar row of its own: the sidebar, chat and right-panel
//! columns all run to the window top. So the cluster rides the top strip of
//! whichever column is currently *rightmost* — [`caption_host`] is that routing
//! rule, kept platform-parameterized so it stays testable off Windows, and it
//! returns at most one surface so no layout can render two clusters.
//!
//! The buttons are hit-tested natively: each declares a
//! [`gpui::WindowControlArea`], which the Windows backend answers `WM_NCHITTEST`
//! with (`HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE`). That gives real Windows
//! behavior — snap layouts on maximize hover included — and means the buttons
//! need no click handlers of their own and are never part of the surrounding
//! drag area.

use gpui::{
    App, InteractiveElement, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, Window, WindowControlArea, div, px,
};
use gpui_component::ActiveTheme as _;
use tcode_runtime::app::{AppState, RightTab, Route};

/// Height of a caption strip. Matches the shell's 52px top rows so the cluster
/// sits flush with the window top on every host surface.
pub(crate) const CAPTION_STRIP_HEIGHT: f32 = 52.;
/// Width of one button — the width Windows uses for its own caption buttons.
const CAPTION_BUTTON_WIDTH: f32 = 46.;
/// Horizontal space the whole cluster occupies, for surfaces that must reserve
/// room for it rather than simply place it last in a row.
pub(crate) const CAPTION_CLUSTER_WIDTH: f32 = CAPTION_BUTTON_WIDTH * 3.;
/// The system icon font Windows 11 ships; it carries the caption glyphs below.
const CAPTION_FONT: &str = "Segoe Fluent Icons";

/// Whether this build owns its window chrome and must draw caption buttons.
const CLIENT_DECORATED: bool = cfg!(target_os = "windows");

/// A top strip that can host the caption cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptionSurface {
    /// The chat header — rightmost when no right panel is open.
    Chat,
    /// The Diff/Plan panel's tab strip.
    RightPanel,
    /// The Preview panel's chrome row.
    Preview,
    /// The settings content header (the settings route replaces the workspace).
    Settings,
}

/// Which surface's top strip owns the window's top-right corner, or `None` when
/// the platform draws its own caption buttons.
fn caption_host(
    client_decorated: bool,
    route: Route,
    right_panel_open: bool,
    right_tab: RightTab,
) -> Option<CaptionSurface> {
    if !client_decorated {
        return None;
    }
    Some(match route {
        // Settings replaces the whole workspace; its content paper is rightmost.
        Route::Settings => CaptionSurface::Settings,
        Route::Chat if !right_panel_open => CaptionSurface::Chat,
        Route::Chat if right_tab == RightTab::Preview => CaptionSurface::Preview,
        // Diff and Plan share the one right-panel container.
        Route::Chat => CaptionSurface::RightPanel,
    })
}

/// Whether `surface` must render the caption cluster this frame.
pub(crate) fn hosts_caption(surface: CaptionSurface, state: &AppState) -> bool {
    caption_host(
        CLIENT_DECORATED,
        state.route,
        state.diff_panel_open(),
        state.right_tab(),
    ) == Some(surface)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptionButton {
    Minimize,
    MaximizeRestore,
    Close,
}

impl CaptionButton {
    const fn id(self) -> &'static str {
        match self {
            Self::Minimize => "window-caption-minimize",
            Self::MaximizeRestore => "window-caption-maximize",
            Self::Close => "window-caption-close",
        }
    }

    const fn area(self) -> WindowControlArea {
        match self {
            Self::Minimize => WindowControlArea::Min,
            Self::MaximizeRestore => WindowControlArea::Max,
            Self::Close => WindowControlArea::Close,
        }
    }

    /// Segoe Fluent Icons: ChromeMinimize, ChromeMaximize, ChromeRestore,
    /// ChromeClose — the glyphs Windows' own caption buttons use.
    const fn glyph(self, maximized: bool) -> &'static str {
        match self {
            Self::Minimize => "\u{e921}",
            Self::MaximizeRestore if maximized => "\u{e923}",
            Self::MaximizeRestore => "\u{e922}",
            Self::Close => "\u{e8bb}",
        }
    }
}

/// Mark an *inert* stretch of a top strip as the window's drag handle.
///
/// [`crate::window_drag_area`] moves the window with `start_window_move` and
/// zooms it with `titlebar_double_click` — both no-ops on Windows, where the
/// only way to drag (and to double-click-maximize, and to get snap) is a native
/// `HTCAPTION` region. So on Windows the strips additionally hand their empty
/// area to the platform.
///
/// Apply this only to elements with no controls inside: an `HTCAPTION` area
/// swallows clicks on anything under it that does not [`InteractiveElement::occlude`]
/// itself — which is exactly why the caption buttons above do occlude.
pub(crate) fn drag_region<E: InteractiveElement>(el: E) -> E {
    if CLIENT_DECORATED {
        el.window_control_area(WindowControlArea::Drag)
    } else {
        el
    }
}

/// The caption cluster, in Windows order: minimize, maximize/restore, close.
/// Call only when [`hosts_caption`] is true for the surface being rendered.
pub(crate) fn caption_controls(window: &Window, cx: &App) -> impl IntoElement {
    let maximized = window.is_maximized();
    div()
        .id("window-caption")
        .flex()
        .flex_none()
        .h_full()
        .items_center()
        .child(caption_button(CaptionButton::Minimize, maximized, cx))
        .child(caption_button(
            CaptionButton::MaximizeRestore,
            maximized,
            cx,
        ))
        .child(caption_button(CaptionButton::Close, maximized, cx))
}

fn caption_button(button: CaptionButton, maximized: bool, cx: &App) -> impl IntoElement {
    // Close is destructive, so it takes the full danger fill; the ordinary
    // controls stay subtle. The glyph on that fill is `primary_foreground`
    // (white in both modes) — Windows' own white close glyph. Not
    // `danger_foreground`: that token is the *red* used to write danger text on
    // a normal background, so it would be red-on-red here.
    let (hover_bg, pressed_bg, active_fg) = if button == CaptionButton::Close {
        (
            cx.theme().danger,
            cx.theme().danger_active,
            cx.theme().primary_foreground,
        )
    } else {
        (
            cx.theme().accent,
            cx.theme().secondary_active,
            cx.theme().foreground,
        )
    };

    div()
        .id(button.id())
        .flex()
        .flex_none()
        .h_full()
        .w(px(CAPTION_BUTTON_WIDTH))
        .items_center()
        .justify_center()
        .text_color(cx.theme().muted_foreground)
        // Native hit-testing: no click handlers needed.
        .window_control_area(button.area())
        // Blocking the mouse ends gpui's hit test at this button, so neither the
        // header's drag listeners nor any enclosing `Drag` control area is in
        // the hit set — the platform sees a caption button, never a drag area.
        .occlude()
        .font_family(CAPTION_FONT)
        .text_size(px(10.))
        .hover(move |style| style.bg(hover_bg).text_color(active_fg))
        .active(move |style| style.bg(pressed_bg).text_color(active_fg))
        .child(button.glyph(maximized))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURFACES: [CaptionSurface; 4] = [
        CaptionSurface::Chat,
        CaptionSurface::RightPanel,
        CaptionSurface::Preview,
        CaptionSurface::Settings,
    ];
    const ROUTES: [Route; 2] = [Route::Chat, Route::Settings];
    const TABS: [RightTab; 3] = [RightTab::Diff, RightTab::Plan, RightTab::Preview];

    #[test]
    fn chat_hosts_the_cluster_when_no_right_panel_is_open() {
        for tab in TABS {
            assert_eq!(
                caption_host(true, Route::Chat, false, tab),
                Some(CaptionSurface::Chat),
                "closed right panel leaves chat rightmost (remembered tab {tab:?})"
            );
        }
    }

    #[test]
    fn the_right_panel_hosts_the_cluster_for_diff_and_plan() {
        for tab in [RightTab::Diff, RightTab::Plan] {
            assert_eq!(
                caption_host(true, Route::Chat, true, tab),
                Some(CaptionSurface::RightPanel),
                "{tab:?} shares the diff container"
            );
        }
    }

    #[test]
    fn preview_hosts_the_cluster_when_it_is_the_open_tab() {
        assert_eq!(
            caption_host(true, Route::Chat, true, RightTab::Preview),
            Some(CaptionSurface::Preview)
        );
    }

    #[test]
    fn the_settings_header_hosts_the_cluster_on_the_settings_route() {
        // Settings replaces the workspace, whatever the chat layout was.
        for open in [false, true] {
            for tab in TABS {
                assert_eq!(
                    caption_host(true, Route::Settings, open, tab),
                    Some(CaptionSurface::Settings)
                );
            }
        }
    }

    #[test]
    fn exactly_one_surface_hosts_the_cluster_in_every_layout() {
        for route in ROUTES {
            for open in [false, true] {
                for tab in TABS {
                    let hosting = SURFACES
                        .into_iter()
                        .filter(|surface| caption_host(true, route, open, tab) == Some(*surface))
                        .count();
                    assert_eq!(
                        hosting, 1,
                        "route {route:?}, right panel open {open}, tab {tab:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_surface_hosts_the_cluster_without_client_decorations() {
        for route in ROUTES {
            for open in [false, true] {
                for tab in TABS {
                    assert_eq!(
                        caption_host(false, route, open, tab),
                        None,
                        "macOS and Linux keep their platform chrome"
                    );
                }
            }
        }
    }
}
