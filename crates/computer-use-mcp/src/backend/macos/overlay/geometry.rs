use crate::outline::Frame;

pub(super) const BORDER_PADDING: f64 = 80.0;

#[derive(Clone, Copy)]
pub(super) struct DisplayGeometry {
    pub(super) ax: Frame,
    pub(super) appkit: Frame,
}

/// Converts an AX global point (y-down) into AppKit global coordinates
/// (y-up), using the display containing the point as the flip axis.
pub(super) fn ax_screen_to_appkit(point: (f64, f64), display: DisplayGeometry) -> (f64, f64) {
    (
        display.appkit.x + (point.0 - display.ax.x),
        display.appkit.y + display.ax.y + display.ax.h - point.1,
    )
}

fn ax_frame_to_appkit(frame: Frame, display: DisplayGeometry) -> Frame {
    let (left, bottom) = ax_screen_to_appkit((frame.x, frame.y + frame.h), display);
    Frame {
        x: left,
        y: bottom,
        w: frame.w,
        h: frame.h,
    }
}

pub(super) fn border_frame(window_frame: Frame, display: DisplayGeometry) -> Frame {
    let appkit = ax_frame_to_appkit(window_frame, display);
    Frame {
        x: appkit.x - BORDER_PADDING,
        y: appkit.y - BORDER_PADDING,
        w: appkit.w + BORDER_PADDING * 2.0,
        h: appkit.h + BORDER_PADDING * 2.0,
    }
}

pub(super) fn is_finite_point(point: (f64, f64)) -> bool {
    point.0.is_finite() && point.1.is_finite()
}

pub(super) fn is_valid_frame(frame: Frame) -> bool {
    frame.x.is_finite()
        && frame.y.is_finite()
        && frame.w.is_finite()
        && frame.h.is_finite()
        && (frame.x + frame.w).is_finite()
        && (frame.y + frame.h).is_finite()
        && (frame.w + BORDER_PADDING * 2.0).is_finite()
        && (frame.h + BORDER_PADDING * 2.0).is_finite()
        && frame.w > 0.0
        && frame.h > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_ax_geometry_and_expands_border_on_the_containing_display() {
        let display = DisplayGeometry {
            ax: Frame {
                x: 0.0,
                y: 0.0,
                w: 1_440.0,
                h: 900.0,
            },
            appkit: Frame {
                x: 0.0,
                y: 0.0,
                w: 1_440.0,
                h: 900.0,
            },
        };

        assert_eq!(ax_screen_to_appkit((100.0, 250.0), display), (100.0, 650.0));
        assert_eq!(
            border_frame(
                Frame {
                    x: 100.0,
                    y: 200.0,
                    w: 500.0,
                    h: 400.0,
                },
                display,
            ),
            Frame {
                x: 20.0,
                y: 220.0,
                w: 660.0,
                h: 560.0,
            }
        );

        let offset_display = DisplayGeometry {
            ax: Frame {
                x: 1_440.0,
                y: 100.0,
                w: 1_920.0,
                h: 1_080.0,
            },
            appkit: Frame {
                x: 1_440.0,
                y: -280.0,
                w: 1_920.0,
                h: 1_080.0,
            },
        };
        assert_eq!(
            ax_screen_to_appkit((1_500.0, 200.0), offset_display),
            (1_500.0, 700.0)
        );
    }
}
