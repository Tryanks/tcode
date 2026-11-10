//! A time shown to the second, drawn by a view of its own that ticks only
//! while a frame draws it.

use std::time::Duration;

use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, ElementId, Entity, IntoElement,
    ParentElement as _, Render, RenderOnce, SharedString, Task, Window, div,
};

/// How far a moment is from now, in milliseconds, written for the reader.
pub(crate) type ClockText = fn(u64) -> SharedString;

/// Now, in unix milliseconds, by the clock the executor's timers run on:
/// the system's in the app, the one a test advances in a test.
fn now_ms(executor: &BackgroundExecutor) -> u64 {
    let now = executor.scheduler_executor().scheduler().clock().utc_now();
    u64::try_from(now.timestamp_millis()).unwrap_or(0)
}

/// The time since a moment, or until one, redrawn as its second changes.
///
/// The view that draws it is the element's state, so a clock no longer
/// drawn drops it with its timer. Its parent only places the view and never
/// reads it: the tick redraws the clock alone, not what it is shown in.
#[derive(IntoElement)]
pub(crate) struct Clock {
    id: ElementId,
    at_ms: u64,
    until: bool,
    text: ClockText,
}

impl Clock {
    /// The time since `since_ms` (unix ms).
    pub(crate) fn since(id: impl Into<ElementId>, since_ms: u64, text: ClockText) -> Self {
        Self {
            id: id.into(),
            at_ms: since_ms,
            until: false,
            text,
        }
    }

    /// The time left until `deadline_secs` (unix seconds), which stops at
    /// zero.
    pub(crate) fn until(id: impl Into<ElementId>, deadline_secs: u64, text: ClockText) -> Self {
        Self {
            id: id.into(),
            at_ms: deadline_secs.saturating_mul(1000),
            until: true,
            text,
        }
    }
}

impl RenderOnce for Clock {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            at_ms,
            until,
            text,
        } = self;
        window.with_global_id(id, |id, window| {
            window.with_element_state(id, |held: Option<(u64, Entity<Ticks>)>, _| {
                let view = match held {
                    Some((held_at, view)) if held_at == at_ms => view,
                    _ => cx.new(|cx| Ticks::new(at_ms, until, text, cx)),
                };
                (view.clone(), (at_ms, view))
            })
        })
    }
}

struct Ticks {
    at_ms: u64,
    until: bool,
    text: ClockText,
    _tick: Task<()>,
}

impl Ticks {
    fn new(at_ms: u64, until: bool, text: ClockText, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                let now = now_ms(cx.background_executor());
                if until && now >= at_ms {
                    break;
                }
                // A fixed wait drifts off the second and sooner or later
                // skips a number; this one lands on the next one.
                let into_second = (i128::from(now) - i128::from(at_ms)).rem_euclid(1000) as u64;
                cx.background_executor()
                    .timer(Duration::from_millis(1000 - into_second))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            at_ms,
            until,
            text,
            _tick: tick,
        }
    }
}

impl Render for Ticks {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_ms(cx.background_executor());
        let distance = if self.until {
            // Rounded up, as whole seconds left of a deadline on the second
            // are: the last one shows until the deadline itself.
            self.at_ms.saturating_sub(now).div_ceil(1000) * 1000
        } else {
            now.saturating_sub(self.at_ms)
        };
        div().child((self.text)(distance))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    thread_local! {
        static SHOWN: Cell<Option<u64>> = const { Cell::new(None) };
    }

    /// What the clock last drew, in milliseconds from its moment.
    fn record(distance: u64) -> SharedString {
        SHOWN.with(|shown| shown.set(Some(distance)));
        SharedString::default()
    }

    struct Since(u64);

    impl Render for Since {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(Clock::since("clock", self.0, record))
        }
    }

    fn draw(cx: &mut VisualTestContext) -> Option<u64> {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        SHOWN.with(Cell::get)
    }

    /// The time since a moment is the executor's, and the clock redraws as
    /// each second of it passes.
    #[gpui::test]
    fn a_clock_counts_the_seconds_the_executor_passes(cx: &mut TestAppContext) {
        let started = cx.update(|cx| now_ms(cx.background_executor())) - 5_000;
        let (_, cx) = cx.add_window_view(|_, _| Since(started));
        assert_eq!(draw(cx), Some(5_000));
        cx.executor().advance_clock(Duration::from_secs(1));
        assert_eq!(draw(cx), Some(6_000));
    }
}
