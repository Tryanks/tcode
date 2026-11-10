//! Windows-only caption controls: minimize, maximize/restore, close.
//!
//! On Windows the window is client-decorated (`crates/app/src/main.rs` marks the
//! titlebar transparent there), so the system draws no caption buttons and the
//! app must supply them. macOS keeps its native traffic lights and Linux keeps
//! its system titlebar, so on both this module renders nothing.
//!
//! The window has no titlebar row of its own: the sidebar, chat and right-panel
//! columns all run to the window top. So the cluster rides the top strip of
//! whichever column is currently *rightmost* — [`hosts_caption_for_state`] is
//! that routing rule, and it names at most one surface so no layout can render
//! two clusters.
//!
//! The buttons are hit-tested natively: each declares a
//! [`gpui::WindowControlArea`], which the Windows backend answers `WM_NCHITTEST`
//! with (`HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE`). That gives real Windows
//! behavior — snap layouts on maximize hover included — and means the buttons
//! need no click handlers of their own and are never part of the surrounding
//! drag area.

use crate::theme::ActiveTheme as _;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    App, InteractiveElement, IntoElement, ParentElement as _, Pixels,
    StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div, px,
};
use tcode_core::ui::RightTab;

use crate::window_state::Route;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// Height of a caption strip. Matches the shell's 52px top rows so the cluster
/// sits flush with the window top on every host surface.
pub(crate) const CAPTION_STRIP_HEIGHT: f32 = 52.;
/// Width of one button — the width Windows uses for its own caption buttons.
const CAPTION_BUTTON_WIDTH: f32 = 46.;
/// Horizontal space the whole cluster occupies, for surfaces that must reserve
/// room for it rather than simply place it last in a row.
pub(crate) const CAPTION_CLUSTER_WIDTH: f32 = CAPTION_BUTTON_WIDTH * 3.;
/// Whether this build owns its window chrome and must draw caption buttons.
const CLIENT_DECORATED: bool = cfg!(target_os = "windows");
/// What the caption cluster takes from an open right panel's top strip. With
/// the panel open the workspace's top-right corner is the panel's, so on a
/// build that draws its own caption buttons they sit in its strip.
pub(crate) const RIGHT_PANEL_CAPTION_WIDTH: f32 = if CLIENT_DECORATED {
    CAPTION_CLUSTER_WIDTH
} else {
    0.
};

/// What [`traffic_light_inset`] reserves, for width budgets with no window to ask.
pub(crate) const TRAFFIC_LIGHT_INSET: f32 = 80.;

/// Left inset a top strip keeps clear of the native macOS traffic lights,
/// which the system draws over the window's top-left outside full screen.
pub(crate) fn traffic_light_inset(window: &Window) -> Pixels {
    if cfg!(target_os = "macos") && !window.is_fullscreen() {
        px(TRAFFIC_LIGHT_INSET)
    } else {
        Pixels::ZERO
    }
}

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
    /// The hosts content header (the hosts route keeps the sidebar beside it).
    Hosts,
}

/// The compact layout has exactly one top strip, so it always owns the corner
/// where this platform expects its caption buttons.
pub(crate) fn compact_hosts_caption() -> bool {
    CLIENT_DECORATED
}

/// Whether `surface` must render the caption cluster this frame: the one whose
/// top strip owns the window's top-right corner, on a build that draws its
/// own caption buttons.
pub(crate) fn hosts_caption_for_state(
    surface: CaptionSurface,
    route: Route,
    right_panel_open: bool,
    right_tab: RightTab,
) -> bool {
    CLIENT_DECORATED
        && surface
            == match route {
                Route::Settings => CaptionSurface::Settings,
                Route::Hosts => CaptionSurface::Hosts,
                Route::Chat if !right_panel_open => CaptionSurface::Chat,
                Route::Chat if right_tab == RightTab::Preview => CaptionSurface::Preview,
                Route::Chat => CaptionSurface::RightPanel,
            }
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

    fn icon(self, maximized: bool) -> IconName {
        match self {
            Self::Minimize => IconName::WindowMinimize,
            Self::MaximizeRestore if maximized => IconName::WindowRestore,
            Self::MaximizeRestore => IconName::WindowMaximize,
            Self::Close => IconName::WindowClose,
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

/// Where a page's own close goes on a strip that hosts the caption cluster:
/// on a row of its own under the strip, a gap below the cluster, its right
/// end as far from the window edge as on a strip without the cluster — over
/// the cluster's Close. Beside the cluster it would read as a fourth window
/// control.
pub(crate) fn below_caption(close: impl IntoElement, cx: &App) -> gpui::Div {
    let spacing = &cx.theme().tokens.spacing;
    div()
        .flex()
        .justify_end()
        .w_full()
        .pt(spacing.md)
        .pr(spacing.sm)
        .child(close)
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

    let rest_fg = cx.theme().muted_foreground;
    crate::motion::Sampled::new(move |window, cx| {
        let (hovered, on_hover) = crate::motion::hovered(button.id(), window, cx);
        let settle = || gpui_base::motion::Transition::new(GLIDE);
        let fill = gpui_base::motion::transition(
            (button.id(), "fill"),
            if hovered {
                hover_bg
            } else {
                hover_bg.opacity(0.)
            },
            settle(),
            window,
            cx,
        );
        let fg = gpui_base::motion::transition(
            (button.id(), "fg"),
            if hovered { active_fg } else { rest_fg },
            settle(),
            window,
            cx,
        );
        div()
            .id(button.id())
            .debug_selector(move || button.id().into())
            .flex()
            .flex_none()
            .h_full()
            .w(px(CAPTION_BUTTON_WIDTH))
            .items_center()
            .justify_center()
            .bg(fill)
            .text_color(fg)
            .window_control_area(button.area())
            // Blocking the mouse ends gpui's hit test at this button, so neither the
            // header's drag listeners nor any enclosing `Drag` control area is in
            // the hit set — the platform sees a caption button, never a drag area.
            .occlude()
            .on_hover(on_hover)
            .active(move |style| style.bg(pressed_bg).text_color(active_fg))
            .child(Icon::new(button.icon(maximized)).small())
            .into_any_element()
    })
}
