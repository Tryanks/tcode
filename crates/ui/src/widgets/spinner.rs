use std::cell::RefCell;
use std::collections::HashSet;
use std::panic::Location;
use std::time::Duration;

use crate::{
    icon::{Icon, IconName},
    sizing::{Sizable, Size},
};
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, GlobalElementId, Hsla, IntoElement,
    ParentElement as _, Render, RenderOnce, Styled as _, Transformation, Window, div, percentage,
    prelude::FluentBuilder as _,
};
use gpui_base::motion::{Easing, IterationCount, Keyframe, Keyframes, Timing, animate_keyframes};

/// A rotating loader icon. With no explicit size it takes the current font
/// size, like [`Icon`].
///
/// It turns in a view of its own, kept as the element's state: the turn asks
/// for a frame on every frame, which builds the view drawing it again, so
/// what holds a spinner (a composer while its turn runs, a running command's
/// row) is drawn from last frame around it rather than built again with it.
#[derive(IntoElement)]
pub struct Spinner {
    size: Option<Size>,
    color: Option<Hsla>,
    /// Where it was made: spinners made in one place under one element take
    /// a view each, in the order they are drawn.
    at: &'static Location<'static>,
}

impl Spinner {
    #[track_caller]
    pub fn new() -> Self {
        Self {
            size: None,
            color: None,
            at: Location::caller(),
        }
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl Default for Spinner {
    #[track_caller]
    fn default() -> Self {
        Self::new()
    }
}

impl Sizable for Spinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = Some(size.into());
        self
    }
}

/// How a spinner is drawn: its size and colour.
type Look = (Option<Size>, Option<Hsla>);

thread_local! {
    /// The spinner views taken in the frame being drawn, by where they were
    /// made and how many were made there before.
    static TAKEN: RefCell<HashSet<(GlobalElementId, usize)>> = RefCell::default();
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let look: Look = (self.size, self.color);
        window.with_global_id(ElementId::CodeLocation(*self.at), |made, window| {
            let (nth, first) = TAKEN.with(|taken| {
                let mut taken = taken.borrow_mut();
                let first = taken.is_empty();
                let nth = (0..)
                    .find(|nth| taken.insert((made.clone(), *nth)))
                    .expect("a free place");
                (nth, first)
            });
            if first {
                window.defer(cx, |_, _| TAKEN.with(|taken| taken.borrow_mut().clear()));
            }
            window.with_global_id(ElementId::Integer(nth as u64), |id, window| {
                // Placed without reading it: what draws the spinner is not
                // built again with it.
                window.with_element_state(id, |held: Option<(Look, Entity<SpinnerView>)>, _| {
                    let view = match held {
                        Some((held, view)) => {
                            if held != look {
                                view.update(cx, |view, cx| {
                                    (view.size, view.color) = look;
                                    cx.notify();
                                });
                            }
                            view
                        }
                        None => cx.new(|_| SpinnerView {
                            size: look.0,
                            color: look.1,
                        }),
                    };
                    (view.clone(), (look, view))
                })
            })
        })
    }
}

struct SpinnerView {
    size: Option<Size>,
    color: Option<Hsla>,
}

impl Render for SpinnerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Reduced motion samples the last frame, a full turn: the glyph rests.
        let turn = animate_keyframes(
            "spinner-turn",
            &Keyframes::try_new([Keyframe::new(0., 0.), Keyframe::new(1., 1.)])
                .expect("two ordered endpoints"),
            Timing::new(Duration::from_millis(800))
                .iterations(IterationCount::Infinite)
                .ease(Easing::EaseInOut),
            window,
            cx,
        )
        .value;
        div().child(
            Icon::new(IconName::Loader)
                .when_some(self.size, |icon, size| icon.with_size(size))
                .when_some(self.color, |icon, color| icon.text_color(color))
                .transform(Transformation::rotate(percentage(turn))),
        )
    }
}
