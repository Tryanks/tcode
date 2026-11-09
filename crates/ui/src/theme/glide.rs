//! The theme gliding from one mode's palette to the other's.
//!
//! gpui-base has no theme transition, and GPUI cannot draw the window into a
//! texture to cross-fade a snapshot of it: every view reads the theme's
//! tokens as it renders. Tcode builds the theme itself, so each frame of a
//! glide installs a palette whose colours are mixed between the two modes'
//! and draws the window again. What is not a colour — the mode, fonts,
//! radii, the syntax palette — is the new mode's from the first frame.

use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

use gpui::{App, Global, Hsla, Rgba, Window};
use gpui_base::motion::{Easing, Timing};

use super::{ActiveTheme as _, Theme, ThemeMode};

/// A change of the whole window's luminance: longer than a control's settle,
/// so the eye follows it rather than sees a flash, and even at both ends.
const GLIDE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Glide {
    current: Option<Run>,
    /// Which glide a frame callback belongs to, so one replaced or stopped
    /// stops asking for frames.
    generation: u64,
}

struct Run {
    from: Theme,
    to: Theme,
    started: Instant,
}

impl Global for Glide {}

/// Switch to `mode`, its colours gliding from the palette shown now — the
/// one a glide under way has reached, if any. Under reduced motion the
/// switch is immediate.
pub fn glide_mode(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    let gliding = cx
        .try_global::<Glide>()
        .is_some_and(|glide| glide.current.is_some());
    let from = cx.theme().clone();
    let to = super::themes(mode, cx);
    if cx.reduce_motion() || (!gliding && from.mode == mode) {
        super::change_mode(mode, Some(window), cx);
        return;
    }
    let started = cx.background_executor().now();
    let glide = cx.default_global::<Glide>();
    glide.generation += 1;
    let generation = glide.generation;
    glide.current = Some(Run { from, to, started });
    // The glass behind a translucent window is native and has no palette to
    // mix; it cross-fades to the new appearance on the glide's frames.
    #[cfg(target_os = "macos")]
    crate::macos_backdrop::glide_appearance(mode, cx);
    step(generation, window, cx);
}

/// Stop a glide under way, leaving the palette it reached.
pub(super) fn stop(cx: &mut App) {
    if let Some(glide) = cx.try_global::<Glide>()
        && glide.current.is_some()
    {
        let glide = cx.global_mut::<Glide>();
        glide.current = None;
        glide.generation += 1;
    }
}

fn step(generation: u64, window: &mut Window, cx: &mut App) {
    let now = cx.background_executor().now();
    let glide = cx.default_global::<Glide>();
    if glide.generation != generation {
        return;
    }
    let Some(run) = &glide.current else {
        return;
    };
    let sample = Timing::new(GLIDE)
        .ease(Easing::EaseInOut)
        .sample(now.saturating_duration_since(run.started));
    let theme = if sample.finished {
        let theme = run.to.clone();
        glide.current = None;
        theme
    } else {
        run.from.mix(&run.to, sample.directed_progress)
    };
    super::install(theme, cx);
    #[cfg(target_os = "macos")]
    crate::macos_backdrop::glide_step(
        if sample.finished {
            1.
        } else {
            sample.directed_progress
        },
        cx,
    );
    window.refresh();
    if !sample.finished {
        window.on_next_frame(move |window, cx| step(generation, window, cx));
    }
}

/// `from` mixed toward `to` by `progress`, over premultiplied alpha, so a
/// colour gaining or losing transparency does not flash the hue of its
/// clear end, and what it paints over anything lies between what its ends
/// paint there.
pub(crate) fn mix(from: Hsla, to: Hsla, progress: f32) -> Hsla {
    if progress <= 0. {
        return from;
    }
    if progress >= 1. {
        return to;
    }
    let lerp = |x: f32, y: f32| x + (y - x) * progress;
    let alpha = lerp(from.a, to.a);
    // One colour fading, or a clear end, adds no hue: the other end's own
    // colour at the alpha reached, exactly, where a round trip through RGB
    // would not keep it.
    if (from.h, from.s, from.l) == (to.h, to.s, to.l) || from.a <= 0. {
        return Hsla { a: alpha, ..to };
    }
    if to.a <= 0. {
        return Hsla { a: alpha, ..from };
    }
    let (a, b) = (from.to_rgb(), to.to_rgb());
    let channel = |x: f32, y: f32| lerp(x * a.a, y * b.a) / alpha;
    Rgba {
        r: channel(a.r, b.r),
        g: channel(a.g, b.g),
        b: channel(a.b, b.b),
        a: alpha,
    }
    .into()
}

impl Theme {
    /// This palette mixed toward `to` by `progress`: every colour mixed, the
    /// rest `to`'s.
    fn mix(&self, to: &Theme, progress: f32) -> Theme {
        let m = |from: Hsla, to: Hsla| mix(from, to, progress);
        let (a, b) = (&self.tokens.colors, &to.tokens.colors);
        let mut tokens = to.tokens.clone();
        let colors = &mut tokens.colors;
        colors.background = m(a.background, b.background);
        colors.foreground = m(a.foreground, b.foreground);
        colors.surface = m(a.surface, b.surface);
        colors.surface_foreground = m(a.surface_foreground, b.surface_foreground);
        colors.primary = m(a.primary, b.primary);
        colors.primary_foreground = m(a.primary_foreground, b.primary_foreground);
        colors.secondary = m(a.secondary, b.secondary);
        colors.secondary_foreground = m(a.secondary_foreground, b.secondary_foreground);
        colors.muted = m(a.muted, b.muted);
        colors.muted_foreground = m(a.muted_foreground, b.muted_foreground);
        colors.accent = m(a.accent, b.accent);
        colors.accent_foreground = m(a.accent_foreground, b.accent_foreground);
        colors.destructive = m(a.destructive, b.destructive);
        colors.destructive_foreground = m(a.destructive_foreground, b.destructive_foreground);
        colors.border = m(a.border, b.border);
        colors.input = m(a.input, b.input);
        colors.ring = m(a.ring, b.ring);
        colors.selection = m(a.selection, b.selection);
        Theme {
            accent: m(self.accent, to.accent),
            background: m(self.background, to.background),
            border: m(self.border, to.border),
            danger: m(self.danger, to.danger),
            danger_active: m(self.danger_active, to.danger_active),
            danger_foreground: m(self.danger_foreground, to.danger_foreground),
            font_family: to.font_family.clone(),
            foreground: m(self.foreground, to.foreground),
            highlight_theme: to.highlight_theme.clone(),
            info: m(self.info, to.info),
            info_foreground: m(self.info_foreground, to.info_foreground),
            input: m(self.input, to.input),
            link: m(self.link, to.link),
            list_active: m(self.list_active, to.list_active),
            list_hover: m(self.list_hover, to.list_hover),
            mode: to.mode,
            darkness: self.darkness + (to.darkness - self.darkness) * progress,
            mono_font_family: to.mono_font_family.clone(),
            muted: m(self.muted, to.muted),
            muted_foreground: m(self.muted_foreground, to.muted_foreground),
            popover: m(self.popover, to.popover),
            primary: m(self.primary, to.primary),
            primary_foreground: m(self.primary_foreground, to.primary_foreground),
            ring: m(self.ring, to.ring),
            scrollbar: m(self.scrollbar, to.scrollbar),
            scrollbar_thumb: m(self.scrollbar_thumb, to.scrollbar_thumb),
            scrollbar_thumb_hover: m(self.scrollbar_thumb_hover, to.scrollbar_thumb_hover),
            secondary: m(self.secondary, to.secondary),
            secondary_active: m(self.secondary_active, to.secondary_active),
            selection: m(self.selection, to.selection),
            sidebar: m(self.sidebar, to.sidebar),
            sidebar_accent: m(self.sidebar_accent, to.sidebar_accent),
            sidebar_foreground: m(self.sidebar_foreground, to.sidebar_foreground),
            success: m(self.success, to.success),
            success_foreground: m(self.success_foreground, to.success_foreground),
            tab_active: m(self.tab_active, to.tab_active),
            theme_name: to.theme_name.clone(),
            tokens,
            warning: m(self.warning, to.warning),
            warning_foreground: m(self.warning_foreground, to.warning_foreground),
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{IntoElement, Render, TestAppContext, Window};
    use tcode_core::settings::ThemeMode as Setting;

    use super::*;
    use crate::settings_page::apply_theme;

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            gpui::Empty
        }
    }

    fn backgrounds(cx: &mut gpui::VisualTestContext) -> (Hsla, Hsla) {
        cx.update(|_, cx| {
            (
                super::super::themes(ThemeMode::Light, cx).background,
                super::super::themes(ThemeMode::Dark, cx).background,
            )
        })
    }

    fn shown(cx: &mut gpui::VisualTestContext) -> (Hsla, Hsla) {
        cx.update(|_, cx| {
            (
                cx.theme().background,
                gpui_base::Theme::global(cx).tokens.colors.background,
            )
        })
    }

    /// Choosing the other mode carries every colour across to its palette
    /// over the frames that follow, Tcode's and gpui-base's alike, while
    /// the mode itself is the new one at once.
    #[gpui::test]
    fn a_theme_change_glides_the_palette(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Blank);
        let (light, dark) = backgrounds(cx);
        assert_ne!(light, dark);

        cx.update(|window, cx| apply_theme(Setting::Dark, window, cx));
        assert!(cx.update(|_, cx| cx.theme().mode.is_dark()));
        cx.executor().advance_clock(GLIDE / 2);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        let (mid, base) = shown(cx);
        assert_ne!(mid, light, "half way, the palette has left the light one");
        assert_ne!(mid, dark, "but has not reached the dark one");
        assert_eq!(mid, base, "gpui-base's tokens glide with Tcode's");

        // Turned back half way, it starts from where it got to.
        cx.update(|window, cx| apply_theme(Setting::Light, window, cx));
        let (turned, _) = shown(cx);
        assert_eq!(turned, mid);

        crate::test_frames::draw_settled(cx);
        assert_eq!(shown(cx), (light, light));
    }

    /// With motion reduced, the palette changes at once.
    #[gpui::test]
    fn a_theme_change_under_reduced_motion_is_immediate(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Blank);
        let (_, dark) = backgrounds(cx);
        cx.update(|_, cx| cx.set_reduce_motion(true));
        cx.update(|window, cx| apply_theme(Setting::Dark, window, cx));
        assert_eq!(shown(cx), (dark, dark));
    }
}
