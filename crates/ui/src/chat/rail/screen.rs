//! The screenshots Tcode's own computer use took in the held history window.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ObjectFit,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::session::TimelineEntry;

use crate::chat::ChatView;
use crate::chat::tools::{Call, desktop};
use crate::icon::IconName;
use crate::material::{CARD_INSET, CHAT_RAIL_WIDEST};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// How long a screenshot takes to fade in once it has decoded, and to
/// cross-fade over the one it replaces.
const CHANGE: Duration = Duration::from_millis(200);

/// The picture box's height for its width: the 16:10 most screens and
/// windows come near; a screenshot of another shape is fitted inside.
const ASPECT: f32 = 10. / 16.;

/// The box a preview is decoded for: the picture's at the widest rail, so
/// resizing the window only scales the picture rather than reading it
/// again at each width.
const PREVIEW_WIDTH: f32 = CHAT_RAIL_WIDEST - 2. * CARD_INSET;

/// One screenshot an observation read back.
#[derive(Clone, Debug, PartialEq)]
struct Shot {
    entry_id: String,
    /// Its place among the images the call's result carried.
    index: usize,
    app: Option<String>,
    title: Option<String>,
    /// When the observation finished, in unix milliseconds.
    taken_ms: Option<u64>,
}

impl Shot {
    fn is(&self, (entry_id, index): &(String, usize)) -> bool {
        self.entry_id == *entry_id && self.index == *index
    }

    /// The window it shows: its app, and the window's title when it says
    /// more.
    fn label(&self) -> Option<String> {
        match (&self.app, &self.title) {
            (Some(app), Some(title)) => Some(format!("{app} · {title}")),
            (app, title) => app.clone().or_else(|| title.clone()),
        }
    }
}

/// The screenshots the thread's observations read back, oldest first, and
/// the one the reader stepped back to.
#[derive(Default)]
pub(in crate::chat) struct Screenshots {
    shots: Vec<Shot>,
    /// The screenshot shown, by item and image; none follows the latest.
    at: Option<(String, usize)>,
}

impl Screenshots {
    /// Lists the screenshots in `entries` again. The timeline holds only the
    /// history window the reader has paged in, so a screenshot from a turn
    /// above it is not listed until a page brings it in; one the reader
    /// stepped to that leaves the window gives way to the latest.
    pub(in crate::chat) fn sync(&mut self, entries: &[Arc<TimelineEntry>]) {
        self.shots.clear();
        for entry in entries {
            let Some(call) = Call::of(entry) else {
                continue;
            };
            if call.images.is_empty() || !desktop::is_observation(&call) {
                continue;
            }
            let (app, title) = call.output.map(desktop::observed_root).unwrap_or_default();
            let taken_ms = entry.finished_ts.or(entry.ts);
            self.shots.extend((0..call.images.len()).map(|index| Shot {
                entry_id: entry.id.clone(),
                index,
                app: app.clone(),
                title: title.clone(),
                taken_ms,
            }));
        }
        if self
            .at
            .as_ref()
            .is_some_and(|at| !self.shots.iter().any(|shot| shot.is(at)))
        {
            self.at = None;
        }
    }

    /// Where the screenshot shown stands in the list.
    fn position(&self) -> Option<usize> {
        let latest = self.shots.len().checked_sub(1)?;
        Some(
            self.at
                .as_ref()
                .and_then(|at| self.shots.iter().position(|shot| shot.is(at)))
                .unwrap_or(latest),
        )
    }

    /// Shows the screenshot `by` places later (earlier when negative);
    /// reaching the latest follows each new one again.
    fn step(&mut self, by: isize) {
        let Some(position) = self.position() else {
            return;
        };
        let latest = self.shots.len() - 1;
        let to = position.saturating_add_signed(by).min(latest);
        self.at = (to < latest).then(|| {
            let shot = &self.shots[to];
            (shot.entry_id.clone(), shot.index)
        });
    }
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, _cx: &App) -> bool {
    !chat.screen.shots.is_empty()
}

/// The line the card's header shows beside its title, open or shut.
pub(super) fn summary(chat: &ChatView, _cx: &App) -> Option<SharedString> {
    chat.screen.shots.last()?.label().map(SharedString::from)
}

/// What the card shows open, laid out at the rail's width.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let screen = &chat.screen;
    let position = screen.position()?;
    let total = screen.shots.len();
    let session_id = chat.session_id.clone()?;
    // The picture's box is the card's inner width. The cross-fade layers
    // have no size of their own, so the box and the rows beside it take it
    // explicitly.
    let width = f32::from(chat.body.width()) - 2. * CARD_INSET;
    let height = width * ASPECT;
    let preview = |shot: &Shot| {
        crate::store::item_image_preview(
            session_id.clone(),
            shot.entry_id.clone(),
            shot.index,
            PREVIEW_WIDTH,
            PREVIEW_WIDTH * ASPECT,
        )
    };
    let target = &screen.shots[position];
    // The screenshot shown gives way to another only once that one has
    // decoded, so a new one cross-fades in over the last rather than over
    // an empty box. With none shown yet, the first fades in over the box.
    let shown: Rc<RefCell<Option<Shot>>> = chat.motion.keep("rail-screen-shown");
    let ready = crate::motion::image_load(&preview(target), window, cx).settled();
    let current = {
        let mut shown = shown.borrow_mut();
        if ready
            || !shown
                .as_ref()
                .is_some_and(|shot| screen.shots.contains(shot))
        {
            *shown = Some(target.clone());
        }
        shown.clone().unwrap_or_else(|| target.clone())
    };

    let muted = cx.theme().muted_foreground;
    let picture = chat.motion.cross_fade(
        "rail-screen-picture",
        current.clone(),
        CHANGE,
        window,
        cx,
        |shot, window, cx| {
            let source = preview(shot);
            let arrived = crate::motion::image_arrival(
                crate::motion::Clock::from(&chat.motion),
                SharedString::from(format!("rail-screen-{}-{}", shot.entry_id, shot.index)),
                &source,
                CHANGE,
                window,
                cx,
            );
            img(source)
                .w(px(width))
                .h(px(height))
                .object_fit(ObjectFit::Contain)
                .opacity(arrived)
                .with_fallback(crate::image_viewer::unpreviewed(true, false, cx))
                .into_any_element()
        },
    );
    let caption = chat.motion.cross_fade_part(
        "rail-screen-caption",
        current.clone(),
        CHANGE,
        window,
        cx,
        |shot, _, cx| {
            let ago = shot.taken_ms.map(|ms| {
                crate::time::humanize_ago(crate::time::now_secs().saturating_sub(ms / 1000))
            });
            h_flex()
                .w(px(width))
                .gap(cx.theme().tokens.spacing.sm)
                .text_secondary(false, cx)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(cx.theme().foreground)
                        .children(shot.label().map(crate::widgets::fade_text)),
                )
                .children(ago.map(|ago| div().flex_none().text_color(muted).child(ago)))
                .into_any_element()
        },
    );

    let name: SharedString = current
        .label()
        .unwrap_or_else(|| crate::tr!("chat.rail_screen").into_owned())
        .into();
    let open = {
        let session_id = session_id.clone();
        let preview = preview(&current);
        let name = name.clone();
        let shot = current;
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            crate::chat::components::images::open_item_image(
                &session_id,
                &shot.entry_id,
                shot.index,
                preview.clone(),
                name.clone(),
                window,
                cx,
            );
        }
    };
    // The picture is the control; a `Button` would add chrome around it.
    let picture = crate::material::accessible_clickable(
        div()
            .relative()
            .w(px(width))
            .h(px(height))
            .cursor_pointer()
            .rounded(crate::material::radius_card(cx))
            .overflow_hidden()
            .bg(cx.theme().muted)
            .child(picture),
        "rail-screen-picture",
        Role::Button,
        crate::tr!("chat.screen_open", name = name),
        cx,
    )
    .debug_selector(|| "rail-screen-picture".into())
    .on_click(open);

    let stepper = chat
        .motion
        .shown(
            "rail-screen-stepper",
            (total > 1).then_some((position, total)),
            CHANGE,
            window,
            cx,
        )
        .map(|fold| {
            let (position, total) = fold.value;
            let count = chat.motion.cross_fade_part(
                "rail-screen-count",
                (position, total),
                CHANGE,
                window,
                cx,
                |&(position, total), _, cx| {
                    div()
                        .text_secondary(false, cx)
                        .text_color(muted)
                        .child(crate::tr!(
                            "chat.screen_position",
                            index = position + 1,
                            total = total
                        ))
                        .into_any_element()
                },
            );
            fold.fold(
                h_flex()
                    .w(px(width))
                    .justify_center()
                    .gap(cx.theme().tokens.spacing.sm)
                    .child(
                        Button::new("rail-screen-earlier")
                            .motion(&chat.motion)
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronLeft)
                            .disabled(position == 0)
                            .tooltip(crate::tr!("chat.screen_earlier"))
                            .on_click(cx.listener(|chat, _, _, cx| {
                                chat.screen.step(-1);
                                cx.notify();
                            })),
                    )
                    .child(count)
                    .child(
                        Button::new("rail-screen-later")
                            .motion(&chat.motion)
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronRight)
                            .disabled(position + 1 >= total)
                            .tooltip(crate::tr!("chat.screen_later"))
                            .on_click(cx.listener(|chat, _, _, cx| {
                                chat.screen.step(1);
                                cx.notify();
                            })),
                    ),
            )
        });

    Some(
        v_flex()
            .w_full()
            .gap(cx.theme().tokens.spacing.sm)
            .child(picture)
            .child(caption)
            .children(stepper)
            .into_any_element(),
    )
}
