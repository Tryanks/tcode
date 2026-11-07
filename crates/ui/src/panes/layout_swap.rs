//! A layout put in while the chat column is faded out.
//!
//! Not a [`crate::motion::Scene`], which keeps the scene left drawn beneath
//! the one rising over it so that what they share holds still: the column is
//! one dock holding one pane tree, and a layout replaces that tree, building
//! every group anew around the same pane views, so no layout is left to draw
//! beneath the new one. GPUI cannot draw the column into a texture to fade a
//! snapshot of it either, so the one column dips through near-nothing while
//! its layout is replaced.

use gpui::{App, ElementId, Window};
use gpui_base::motion::{Presence, PresencePhase, PresenceSample, Transition};

use super::PaneLayout;

/// How faint the column is at the bottom of a layout's fade: the frame the
/// layout is put in still draws it, faintly, rather than nothing.
const LOWEST: f32 = 0.04;

/// How long a layout swap takes, out and back in.
const SWAP: std::time::Duration = std::time::Duration::from_millis(200);

/// A layout chosen for the whole column, swapped in: the column
/// fades out over half of [`SWAP`], the layout is put in during
/// the frame the column is faintest, and the column fades back in over the
/// other half. A layout chosen meanwhile joins in: while the column fades
/// out it keeps going and puts that one in at the bottom; while it fades in
/// it turns back down from where it is.
#[derive(Default)]
pub(crate) struct LayoutSwap {
    current: Option<Switch>,
    generation: u64,
    opacity: Option<f32>,
}

struct Switch {
    generation: u64,
    /// The layout to put in at the bottom; none while the column fades in.
    chosen: Option<PaneLayout>,
}

impl LayoutSwap {
    /// Fade the column out to put `layout` in.
    pub(crate) fn choose(&mut self, layout: PaneLayout) {
        match &mut self.current {
            Some(switch) => switch.chosen = Some(layout),
            None => {
                self.generation += 1;
                self.current = Some(Switch {
                    generation: self.generation,
                    chosen: Some(layout),
                });
            }
        }
    }

    /// Whether no layout is being put in.
    pub(crate) fn is_idle(&self) -> bool {
        self.current.is_none()
    }

    /// Whether the column is fading out, the panes on it about to be
    /// replaced.
    pub(crate) fn leaving(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|switch| switch.chosen.is_some())
    }

    /// The column's opacity this frame, below one while it switches.
    pub(crate) fn opacity(&self) -> Option<f32> {
        self.opacity
    }

    /// Stop switching: the column is drawn at rest, nothing put in.
    pub(crate) fn land(&mut self) {
        self.current = None;
        self.opacity = None;
    }

    /// This frame of the switch: the layout to put in now, at the bottom of
    /// the fade.
    pub(crate) fn sample(&mut self, window: &mut Window, cx: &mut App) -> Option<PaneLayout> {
        let Some(switch) = &mut self.current else {
            self.opacity = None;
            return None;
        };
        let id = ElementId::NamedInteger("pane-layout-swap".into(), switch.generation);
        let fade = |out: bool, window: &mut Window, cx: &mut App| -> PresenceSample {
            Presence::new(id.clone(), out)
                .transition(Transition::new(SWAP / 2))
                .sample(window, cx)
        };
        let mut sample = fade(switch.chosen.is_some(), window, cx);
        let mut install = None;
        if sample.phase == PresencePhase::Present {
            install = switch.chosen.take();
            // The rise starts in this frame, so it lasts its whole half.
            sample = fade(false, window, cx);
        }
        if sample.phase == PresencePhase::Absent {
            self.current = None;
            self.opacity = None;
        } else {
            self.opacity = Some(1. - sample.progress * (1. - LOWEST));
        }
        install
    }
}
