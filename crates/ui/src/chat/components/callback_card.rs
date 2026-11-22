use std::borrow::Cow;

use crate::icon::{Icon, IconName};
use crate::material::event_card::{self, CardTone};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};

use tcode_core::notice::{CallbackEvent, OrchestrateCallback};

use super::super::model::one_line;
use super::changed_files::ClickHandler;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

const BODY_MAX_HEIGHT: f32 = 320.;
/// The lines of a report shown before the reader asks for all of it.
const PREVIEW_LINES: usize = 4;
/// A report this long may wrap past [`PREVIEW_LINES`] at a narrow width,
/// where a shorter one does not; the card cannot know how it wraps before it
/// is laid out, so past this it offers the rest.
const PREVIEW_CHARS: usize = 100;

/// The child thread a callback reports on, as this client knows it.
pub(crate) struct CallbackChild {
    /// Its vendor's glyph, `None` once it has gone from the host.
    pub(crate) glyph: Option<Icon>,
    /// Its vendor and model.
    pub(crate) detail: Option<String>,
    /// Opens it, `None` once it has gone.
    pub(crate) on_open: Option<ClickHandler>,
}

/// A child thread's callback to the thread that dispatched it. What the
/// callback told the parent's model is the card's body: the report, or what
/// the child waits on, previewed and then whole, never only behind a link.
/// A child waiting on an approval or a question is answered in the child,
/// which the card's action opens.
#[allow(clippy::too_many_arguments)]
pub(crate) fn callback_card(
    key: &str,
    callback: &OrchestrateCallback,
    child: CallbackChild,
    steering: bool,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    hover: crate::motion::ListHover,
    motion: &crate::motion::owned::Owned,
) -> AnyElement {
    let key = key.to_owned();
    let tone = tone(&callback.event);
    let waiting = tone == CardTone::Waiting;
    let title: SharedString = one_line(&callback.title).into();
    let subtitle: SharedString = std::iter::once(event_phrase(&callback.event).into_owned())
        .chain(child.detail)
        .collect::<Vec<_>>()
        .join(" · ")
        .into();
    let (full, preview) = texts(callback);
    let long =
        waiting || full.lines().count() > PREVIEW_LINES || full.chars().count() > PREVIEW_CHARS;
    let empty = full.is_empty();
    let full = if empty {
        crate::tr!("chat.orchestrate_callback_empty").into_owned()
    } else {
        full
    };
    let glyph = child.glyph;
    let on_open = child.on_open;
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        let theme = cx.theme();
        let tile = match glyph {
            Some(glyph) => event_card::glyph_tile(glyph, Some(Icon::new(IconName::Bot)), cx),
            None => event_card::icon_tile(IconName::Bot, cx),
        };
        let steering_tag = steering.then(|| {
            crate::material::semantic_chip(
                crate::tr!("chat.steering"),
                theme.secondary,
                theme.muted_foreground,
                cx,
            )
            .debug_selector({
                let selector = format!("callback-card-steering-{key}");
                move || selector
            })
        });
        // A waiting card opens its child from its action instead.
        let (link, on_open) = if waiting {
            (None, on_open)
        } else {
            let link = motion
                .shown(
                    format!("callback-open-shown-{key}"),
                    on_open.is_some().then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|shown| {
                    let reveal = hover.reveal(&format!("callback-open-{key}"), window, cx);
                    shown.fade(open_child_link(&key, on_open, &hover, reveal, cx))
                });
            (link, None)
        };
        let header = event_card::header(false, cx)
            .child(tile)
            .child(event_card::header_text(title, Some(subtitle), cx))
            .children(steering_tag)
            .children(link);

        let body_text = |text: String, cx: &App| {
            div()
                .w_full()
                .text_body(false, cx)
                .text_color(if empty {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().foreground
                })
                .child(text)
        };
        let preview = motion
            .shown(
                format!("callback-preview-{key}"),
                (!expanded).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|fold| {
                fold.fold(
                    body_text(preview.clone().unwrap_or_else(|| full.clone()), cx)
                        .line_clamp(PREVIEW_LINES),
                )
            });
        let whole = motion
            .shown(
                format!("callback-whole-{key}"),
                expanded.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|fold| {
                fold.fold(
                    div()
                        .id(SharedString::from(format!("callback-body-{key}")))
                        .w_full()
                        .max_h(px(BODY_MAX_HEIGHT))
                        .overflow_y_scroll_area()
                        .debug_selector({
                            let selector = format!("callback-card-body-{key}");
                            move || selector
                        })
                        .child(body_text(full.clone(), cx)),
                )
            });
        let section = event_card::section(cx)
            .gap_0()
            .children(preview)
            .children(whole);

        let toggle = long.then(|| {
            let label: SharedString = if expanded {
                crate::tr!("chat.callback_card.show_less")
            } else {
                crate::tr!("chat.callback_card.show_all")
            }
            .into_owned()
            .into();
            super::changed_files::quiet_control(
                SharedString::from(format!("callback-card-toggle-{key}")),
                label,
                Box::new(on_toggle),
                cx,
            )
            .motion(&motion)
            .aria_expanded(expanded)
            .debug_selector({
                let selector = format!("callback-card-toggle-{key}");
                move || selector
            })
        });
        let action = waiting.then(|| {
            Button::new(SharedString::from(format!("callback-card-open-{key}")))
                .motion(&motion)
                .primary()
                .small()
                .label(crate::tr!("chat.callback_card.open_thread"))
                .disabled(on_open.is_none())
                .when_some(on_open, |button, on_open| {
                    button.on_click(move |event, window, cx| on_open(event, window, cx))
                })
                .debug_selector({
                    let selector = format!("callback-card-open-{key}");
                    move || selector
                })
        });
        let footer = (toggle.is_some() || action.is_some())
            .then(|| event_card::footer(cx).children(toggle).children(action));

        event_card::event_card(tone, cx)
            .debug_selector({
                let selector = format!("callback-card-{key}");
                move || selector
            })
            .child(header)
            .child(section)
            .children(footer)
            .into_any_element()
    })
    .into_any_element()
}

/// Reports record, a fallback for a missing report went wrong without
/// stopping anything, a failure stopped the child, and a request waits on
/// whoever answers it.
fn tone(event: &CallbackEvent) -> CardTone {
    match event {
        CallbackEvent::Ended(state) => match state.as_str() {
            "unreported" => CardTone::Warning,
            "failed" => CardTone::Danger,
            _ => CardTone::Neutral,
        },
        CallbackEvent::Approval | CallbackEvent::Question => CardTone::Waiting,
    }
}

fn event_phrase(event: &CallbackEvent) -> Cow<'static, str> {
    match event {
        CallbackEvent::Ended(state) => match state.as_str() {
            "completed" => crate::tr!("chat.callback_card.completed"),
            "reported" => crate::tr!("chat.callback_card.reported"),
            "updated" => crate::tr!("chat.callback_card.updated"),
            "unreported" => crate::tr!("chat.callback_card.unreported"),
            "failed" => crate::tr!("chat.callback_card.failed"),
            other => Cow::Owned(other.to_string()),
        },
        CallbackEvent::Approval => crate::tr!("chat.callback_card.approval"),
        CallbackEvent::Question => crate::tr!("chat.callback_card.question"),
    }
}

/// The body whole, and what stands for it while it is folded when that is
/// not its first lines: a request's one-line summary, its raw text being
/// instructions to the parent's model.
fn texts(callback: &OrchestrateCallback) -> (String, Option<String>) {
    match callback.report() {
        Some(report) => (report.trim().to_owned(), None),
        None => (
            callback.body.trim().to_owned(),
            callback.request_summary().map(|summary| one_line(&summary)),
        ),
    }
}

/// The link to a callback's child, its underline fading in at the list's
/// hover `reveal`. Once the child has gone the link is leaving, and takes no
/// activation. Not gpui-base's `Link`: it keeps its focus handle to itself,
/// so its keyboard focus ring could only switch at once; the clickable's
/// fades.
fn open_child_link(
    key: &str,
    on_open: Option<ClickHandler>,
    hover: &crate::motion::ListHover,
    reveal: f32,
    cx: &App,
) -> AnyElement {
    let id = SharedString::from(format!("callback-open-{key}"));
    let primary = cx.theme().primary;
    crate::material::accessible_clickable(
        div(),
        id.clone(),
        Role::Link,
        SharedString::from(crate::tr!("agents.open").into_owned()),
        cx,
    )
    .relative()
    .child(hover.hitbox(id))
    .when(reveal > 0., |link| {
        link.underline()
            .text_decoration_color(primary.opacity(reveal))
    })
    .when_some(on_open, |link, on_open| {
        link.cursor_pointer()
            .on_click(move |event, window, cx| on_open(event, window, cx))
    })
    .flex_none()
    .text_token(&cx.theme().tokens.typography.xs)
    .text_color(primary)
    .child(crate::widgets::fade_text(crate::tr!("agents.open_child")))
    .into_any_element()
}
