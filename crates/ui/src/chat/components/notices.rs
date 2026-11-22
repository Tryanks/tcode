//! The notices of the flow that are not a divider: the capsules a change of
//! model or of harness is drawn with; a message Tcode or another thread sent
//! the model, drawn as a bubble coming in; tags under the message they
//! describe; amber lines for what went wrong while the thread carries on; a
//! muted line for what other threads did; a placeholder for content that is
//! missing; and the thread's banner for records this build cannot read.

use std::path::Path;

use crate::icon::{Icon, IconName};
use crate::markdown::{MarkdownState, MarkdownView};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::TooltipTrigger as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::tooltip::Tooltip;
use gpui::{
    AnyElement, App, ClickEvent, Div, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_core::session::SteeringStatus;

use super::changed_files::ClickHandler;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a reason may run and still be told in the working line itself;
/// a longer one is told on hover and in the Work Log.
const RETRY_REASON_CHARS: usize = 48;

/// The tallest an opened line's texts stand before they scroll.
const BODY_MAX_HEIGHT: f32 = 320.;

fn selector(selector: String) -> impl Fn() -> String {
    move || selector.clone()
}

/// What one capsule names: a model or a harness, after its vendor's glyph.
pub(crate) struct Capsule {
    pub(crate) glyph: Option<Icon>,
    pub(crate) label: String,
}

/// One capsule: a rounded pill with a faint fill, in the warning tint when
/// the change it is part of was not the user's.
fn capsule(capsule: Capsule, warning: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let spacing = &theme.tokens.spacing;
    let fill = if warning {
        theme.warning.opacity(0.12)
    } else {
        theme.foreground.opacity(0.06)
    };
    h_flex()
        .min_w_0()
        .items_center()
        .gap(spacing.xs + px(2.))
        .px(spacing.sm + px(3.))
        .py(px(3.))
        .rounded_full()
        .bg(fill)
        .text_color(theme.foreground)
        .font_medium()
        .children(capsule.glyph.map(|glyph| glyph.flex_none()))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .child(crate::widgets::fade_text(capsule.label)),
        )
}

/// A change from one capsule to another, centred, with what follows it on
/// the same line (`trailing`); one capsule alone where nothing was named
/// before. `warning` tints the capsules for a change the harness made on
/// its own.
pub(crate) fn capsule_row(
    id: SharedString,
    from: Option<Capsule>,
    to: Capsule,
    warning: bool,
    trailing: Option<AnyElement>,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    h_flex()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_center()
        .gap_2()
        .text_token(&theme.tokens.typography.xs)
        .text_color(muted)
        .when_some(from, |row, from| {
            row.child(capsule(from, warning, cx))
                .child(Icon::new(IconName::ArrowRight).text_color(muted))
        })
        .child(capsule(to, warning, cx))
        .children(trailing)
}

/// A centred line under a [`capsule_row`] that says in words what it shows,
/// with the action that answers it.
pub(crate) fn capsule_note(
    id: SharedString,
    text: String,
    tint: gpui::Hsla,
    action: Option<AnyElement>,
    cx: &App,
) -> Stateful<Div> {
    h_flex()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_center()
        .gap_2()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(tint)
        .child(div().min_w_0().text_center().child(text))
        .children(action)
}

/// A thread's title as a link of its own, which opens that thread where
/// `on_open` can.
fn thread_link(
    id: String,
    title: SharedString,
    on_open: Option<ClickHandler>,
    cx: &App,
) -> AnyElement {
    crate::material::accessible_clickable(
        div(),
        SharedString::from(id.clone()),
        Role::Link,
        title.clone(),
        cx,
    )
    .debug_selector(selector(id))
    .min_w_0()
    .flex_shrink_1()
    .overflow_hidden()
    .text_color(cx.theme().primary)
    .child(crate::widgets::fade_text(title))
    .when_some(on_open, |link, on_open| {
        link.cursor_pointer()
            .hover(|link| link.underline())
            .on_click(move |event, window, cx| on_open(event, window, cx))
            .with_tooltip(|_, _| Tooltip::new(crate::tr!("tools.open_thread").into_owned()))
    })
    .into_any_element()
}

/// Who sent a message the host wrote into the thread.
pub(crate) enum Sender {
    /// Tcode itself.
    Tcode,
    /// Threads, by title, each opening that thread where it is known here.
    Threads(Vec<(SharedString, Option<ClickHandler>)>),
}

/// A message that came in rather than one the user wrote: who sent it as
/// the caption, then the message as the model was sent it, whole, in a
/// bubble tinted with the primary colour.
pub(crate) struct Incoming<'a> {
    pub(crate) id: &'a str,
    pub(crate) sender: Sender,
    pub(crate) text: &'a str,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) image_host: crate::store::ImageHost,
    pub(crate) cwd: &'a Path,
    pub(crate) steering: Option<SteeringStatus>,
    pub(crate) compact: bool,
}

pub(crate) fn incoming(message: Incoming<'_>, window: &mut Window, cx: &App) -> AnyElement {
    let Incoming {
        id,
        sender,
        text,
        markdown,
        image_host,
        cwd,
        steering,
        compact,
    } = message;
    let theme = cx.theme();
    let (padding_y, padding_x) = crate::material::user_bubble_padding(compact);
    let primary = theme.primary;
    let prose = theme.tokens.typography.md;
    // As a user's bubble is, as wide as its longest line, up to its share of
    // the column.
    let text_style = window.text_style();
    let text_width = text.lines().fold(px(0.), |width, line| {
        let run = text_style.to_run(line.len());
        width.max(
            window
                .text_system()
                .layout_line(line, prose.size, &[run], None)
                .width,
        )
    });
    let caption = match sender {
        Sender::Tcode => h_flex().child(crate::tr!("chat.from_tcode").into_owned()),
        Sender::Threads(threads) => {
            // The title sits where the locale puts it, as a link of its own.
            const TITLE: &str = "\u{1}";
            let template = crate::tr!("chat.from_thread", title = TITLE).into_owned();
            let (before, after) = template.split_once(TITLE).unwrap_or((&template, ""));
            let mut caption = h_flex()
                .min_w_0()
                .gap_1()
                .when(!before.trim().is_empty(), |caption| {
                    caption.child(before.trim_end().to_owned())
                });
            for (index, (title, on_open)) in threads.into_iter().enumerate() {
                if index > 0 {
                    caption = caption.child("·");
                }
                caption = caption.child(thread_link(
                    format!("incoming-open-{id}-{index}"),
                    title,
                    on_open,
                    cx,
                ));
            }
            caption.when(!after.trim().is_empty(), |caption| {
                caption.child(after.trim_start().to_owned())
            })
        }
    };
    let steering = steering.map(|steering| {
        let selector = format!("incoming-steering-{id}");
        crate::material::semantic_chip(
            match steering {
                SteeringStatus::Pending => crate::tr!("chat.steering"),
                SteeringStatus::Accepted => crate::tr!("chat.steered"),
            },
            theme.muted,
            theme.muted_foreground,
            cx,
        )
        .debug_selector(move || selector)
    });
    let content = markdown.map_or_else(
        || div().child(text.to_owned()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .selectable(true)
                .compact_headings(true)
                .image_host(image_host)
                .base_dir(cwd)
                .into_any_element()
        },
    );
    let selector = format!("incoming-{id}");
    v_flex()
        .debug_selector(move || selector)
        .w_full()
        .items_start()
        .gap(px(2.))
        .child(
            h_flex()
                .max_w_full()
                .min_w_0()
                .px(padding_x)
                .items_center()
                .gap_2()
                .text_secondary(compact, cx)
                .child(caption)
                .children(steering),
        )
        .child(
            div()
                .w((text_width + padding_x * 2.).ceil() + px(3.))
                .flex_none()
                .max_w_3_4()
                .px(padding_x)
                .py(padding_y)
                .rounded(crate::material::radius_user_bubble())
                .border_1()
                .border_color(primary.opacity(0.22))
                .bg(primary.opacity(0.08))
                .text_color(theme.foreground)
                .text_token(&prose)
                .child(content),
        )
        .into_any_element()
}

/// A change of the model serving the thread. `rerouted` is what the
/// harness said when it changed it on its own, in words, with the control
/// that switches back.
pub(crate) struct ModelChange {
    pub(crate) from: Option<Capsule>,
    pub(crate) to: Capsule,
    pub(crate) rerouted: Option<(String, Option<(String, ClickHandler)>)>,
}

pub(crate) fn model_change(id: &str, change: ModelChange, cx: &App) -> AnyElement {
    let ModelChange { from, to, rerouted } = change;
    let warning = cx.theme().warning;
    v_flex()
        .w_full()
        .gap_1()
        .child(capsule_row(
            SharedString::from(format!("model-change-{id}")),
            from,
            to,
            rerouted.is_some(),
            None,
            cx,
        ))
        .when_some(rerouted, |column, (note, switch_back)| {
            let action = switch_back.map(|(label, on_click)| {
                let selector = format!("model-switch-back-{id}");
                Button::new(SharedString::from(selector.clone()))
                    .outline()
                    .xsmall()
                    .label(label)
                    .on_click(on_click)
                    .debug_selector(move || selector)
                    .into_any_element()
            });
            column.child(capsule_note(
                SharedString::from(format!("model-change-reason-{id}")),
                note,
                warning,
                action,
                cx,
            ))
        })
        .into_any_element()
}

/// A handoff to another harness or session: who handed over to whom, a
/// line saying more where it was not a plain handoff (`note`), and the text
/// the new session was sent (`relayed`), whole on demand.
pub(crate) struct Relay {
    pub(crate) from: Capsule,
    pub(crate) to: Capsule,
    pub(crate) note: Option<String>,
    pub(crate) relayed: Option<String>,
    pub(crate) expanded: bool,
}

pub(crate) fn relay_handoff(
    id: &str,
    key: &str,
    relay: Relay,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    motion: &crate::motion::owned::Owned,
) -> AnyElement {
    let Relay {
        from,
        to,
        note,
        relayed,
        expanded,
    } = relay;
    let id = id.to_owned();
    let key = key.to_owned();
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        let primary = cx.theme().primary;
        let toggle = relayed.is_some().then(|| {
            let selector = format!("relay-toggle-{id}");
            let label = crate::tr!("chat.relay_content").into_owned();
            Button::new(SharedString::from(selector.clone()))
                .motion(&motion)
                .text()
                .xsmall()
                .aria_label(label.clone())
                .aria_expanded(expanded)
                .on_click(on_toggle)
                .debug_selector(move || selector)
                .child(
                    h_flex()
                        .items_center()
                        .gap_0p5()
                        .text_token(&cx.theme().tokens.typography.xs)
                        .text_color(primary)
                        .child(label)
                        .child(crate::widgets::Chevron::owned_icon(
                            &motion,
                            SharedString::from(format!("relay-chevron-{id}")),
                            expanded,
                            window,
                            cx,
                        )),
                )
                .into_any_element()
        });
        let body = relayed.and_then(|relayed| {
            motion
                .shown(
                    format!("relay-fold-{key}"),
                    expanded.then_some(()),
                    SHOW,
                    window,
                    cx,
                )
                .map(|fold| fold.fold(super::disclosure::disclosure_body(&key, &relayed, cx)))
        });
        let muted = cx.theme().muted_foreground;
        v_flex()
            .w_full()
            .gap_1()
            .child(capsule_row(
                SharedString::from(format!("relay-{id}")),
                Some(from),
                to,
                false,
                toggle,
                cx,
            ))
            .when_some(note, |column, note| {
                column.child(capsule_note(
                    SharedString::from(format!("relay-note-{id}")),
                    note,
                    muted,
                    None,
                    cx,
                ))
            })
            .children(body)
            .into_any_element()
    })
    .into_any_element()
}

/// What the runtime noted about the settings a message ran on, told under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageTag {
    /// It ran on the live process's old settings.
    SettingsKept,
    /// It selected this context window.
    ContextWindow(u64),
}

impl MessageTag {
    pub(crate) fn of(content: &tcode_core::session::EntryContent) -> Option<Self> {
        use tcode_core::session::EntryContent;
        match content {
            EntryContent::SettingsKept => Some(Self::SettingsKept),
            EntryContent::ContextWindowChanged { window } => Some(Self::ContextWindow(*window)),
            _ => None,
        }
    }
}

/// The tags under a user message, right-aligned beneath its bubble.
pub(crate) fn message_tags(entry_id: &str, tags: &[MessageTag], cx: &App) -> AnyElement {
    h_flex()
        .justify_end()
        .flex_wrap()
        .gap_1p5()
        .children(tags.iter().enumerate().map(|(index, tag)| {
            let (label, tooltip, warning, name) = match tag {
                MessageTag::SettingsKept => (
                    crate::tr!("chat.settings_kept_tag").into_owned(),
                    crate::tr!("chat.settings_kept").into_owned(),
                    true,
                    "settings-kept",
                ),
                MessageTag::ContextWindow(window) => {
                    let window = agent::context_window::format(*window);
                    (
                        crate::tr!("chat.context_window_tag", window = window.as_str())
                            .into_owned(),
                        crate::tr!("chat.context_window_changed", window = window.as_str())
                            .into_owned(),
                        false,
                        "context-window",
                    )
                }
            };
            let tint = if warning {
                cx.theme().warning
            } else {
                cx.theme().muted_foreground
            };
            let fill = if warning {
                cx.theme().warning.opacity(0.11)
            } else {
                cx.theme().foreground.opacity(0.06)
            };
            h_flex()
                .id(SharedString::from(format!(
                    "message-tag-{entry_id}-{index}"
                )))
                .debug_selector(selector(format!("message-tag-{name}-{entry_id}")))
                .flex_none()
                .gap_1()
                .items_center()
                .px(px(7.))
                .py(px(1.))
                .rounded(crate::material::radius_chip(cx))
                .bg(fill)
                .text_token(&cx.theme().tokens.typography.xs)
                .text_color(tint)
                .child(label)
                .when(warning, |tag| {
                    tag.child(Icon::new(IconName::Info).xsmall().text_color(tint))
                })
                .with_tooltip(move |_, _| Tooltip::new(tooltip.clone()))
        }))
        .into_any_element()
}

/// The leading part of a message's action row while a rewind or its undo
/// failed: why, in amber, until the next message is sent.
pub(crate) fn rewind_failure(key: &str, label: String, cx: &App) -> AnyElement {
    let tooltip = label.clone();
    h_flex()
        .id(SharedString::from(format!("rewind-failure-{key}")))
        .debug_selector(selector(format!("rewind-failure-{key}")))
        .min_w_0()
        .flex_shrink_1()
        .px_1()
        .items_center()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(cx.theme().warning)
        .child(crate::widgets::fade_text(label))
        .with_tooltip(move |_, _| Tooltip::new(tooltip.clone()))
        .into_any_element()
}

/// A request the harness tries again, as the working line tells it while
/// the turn runs: the attempt's number, and the harness's words when short.
pub(crate) fn retry_clause(turn: usize, attempt: usize, message: &str, cx: &App) -> AnyElement {
    let mut label = crate::tr!("chat.request_retrying", count = attempt).into_owned();
    let message = super::super::model::one_line(message);
    if message.chars().count() <= RETRY_REASON_CHARS && !message.is_empty() {
        label.push_str(" · ");
        label.push_str(&message);
    }
    h_flex()
        .id(SharedString::from(format!("working-retry-{turn}")))
        .debug_selector(selector(format!("working-retry-{turn}")))
        .min_w_0()
        .gap_2()
        .items_center()
        .text_color(cx.theme().warning)
        .child(div().text_color(cx.theme().muted_foreground).child("·"))
        .child(div().min_w_0().child(crate::widgets::fade_text(label)))
        .with_tooltip(move |_, _| Tooltip::new(message.clone()))
        .into_any_element()
}

/// One retry in an expanded Work Log, in the harness's words.
pub(crate) fn retry_row(entry_id: &str, message: &str, cx: &App) -> AnyElement {
    h_flex()
        .id(SharedString::from(format!("retry-row-{entry_id}")))
        .debug_selector(selector(format!("retry-row-{entry_id}")))
        .w_full()
        .px_1()
        .gap_1p5()
        .items_start()
        .text_body(false, cx)
        .text_color(cx.theme().warning)
        .child(
            div()
                .flex_none()
                .h(cx.theme().tokens.typography.sm.line_height)
                .flex()
                .items_center()
                .child(Icon::new(IconName::RefreshCw).small()),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .child(crate::tr!("chat.request_retry_row", message = message).into_owned()),
        )
        .into_any_element()
}

/// The warning mark and `label` on one amber line, for what went wrong while
/// the thread carries on.
pub(crate) fn alert_line(id: String, label: String, cx: &App) -> AnyElement {
    h_flex()
        .id(SharedString::from(id.clone()))
        .debug_selector(selector(id))
        .w_full()
        .gap_1p5()
        .items_start()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(cx.theme().warning)
        .child(alert_mark(cx))
        .child(div().min_w_0().flex_1().child(label))
        .into_any_element()
}

/// The warning mark, centred on the first line of the xs text beside it.
fn alert_mark(cx: &App) -> AnyElement {
    div()
        .flex_none()
        .h(cx.theme().tokens.typography.xs.line_height)
        .flex()
        .items_center()
        .child(Icon::new(IconName::TriangleAlert).xsmall())
        .into_any_element()
}

/// A line that opens on the full text it summarizes: the turn's warnings,
/// or the thread's unreadable records.
pub(crate) struct Expandable<'a> {
    pub(crate) key: &'a str,
    pub(crate) label: String,
    /// Each text the line opens on, as given.
    pub(crate) texts: Vec<String>,
    pub(crate) expanded: bool,
    /// Drawn as a banner across the column rather than a bare line.
    pub(crate) banner: bool,
}

/// An [`Expandable`] in amber, led by the warning mark.
pub(crate) fn expandable_alert(
    line: Expandable<'_>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    motion: &crate::motion::owned::Owned,
) -> AnyElement {
    let Expandable {
        key,
        label,
        texts,
        expanded,
        banner,
    } = line;
    let key = key.to_owned();
    let motion = motion.clone();
    crate::motion::Sampled::new(move |window, cx| {
        let warning = cx.theme().warning;
        let radius = crate::material::radius_card(cx);
        let header = crate::material::accessible_clickable(
            h_flex(),
            SharedString::from(format!("{key}-toggle")),
            Role::Button,
            label.clone(),
            cx,
        )
        .debug_selector(selector(format!("{key}-toggle")))
        .aria_expanded(expanded)
        .w_full()
        .min_w_0()
        .gap_1p5()
        .items_center()
        .cursor_pointer()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(warning)
        .when(banner, |row| {
            row.px(px(11.))
                .py(px(7.))
                .rounded(radius)
                .bg(warning.opacity(0.09))
        })
        .on_click(on_toggle)
        .child(Icon::new(IconName::TriangleAlert).xsmall())
        .child(
            div()
                .min_w_0()
                .flex_shrink_1()
                .overflow_hidden()
                .child(crate::widgets::fade_text(label.clone())),
        )
        .child(
            crate::widgets::Chevron::new(SharedString::from(format!("{key}-chevron")), expanded)
                .owned(&motion)
                .text_color(warning),
        );
        let body = motion
            .shown(
                format!("{key}-fold"),
                expanded.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|fold| {
                fold.fold(
                    div()
                        .id(SharedString::from(format!("{key}-body")))
                        .debug_selector(selector(format!("{key}-body")))
                        .w_full()
                        .max_h(px(BODY_MAX_HEIGHT))
                        .overflow_y_scroll_area()
                        .child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .pl(px(if banner { 31. } else { 20. }))
                                .text_token(&cx.theme().tokens.typography.xs)
                                .text_color(cx.theme().muted_foreground)
                                .children(
                                    texts.iter().map(|text| div().w_full().child(text.clone())),
                                ),
                        ),
                )
            });
        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .children(body)
            .into_any_element()
    })
    .into_any_element()
}

/// A consecutive run of what one other thread did to this thread's run:
/// its title, which opens it, then what it did.
pub(crate) fn control_actions_line(
    id: &str,
    actor_title: &str,
    on_open: ClickHandler,
    actions: String,
    details: Vec<String>,
    cx: &App,
) -> AnyElement {
    let link = thread_link(
        format!("control-actor-{id}"),
        crate::chat::model::one_line(actor_title).into(),
        Some(on_open),
        cx,
    );
    h_flex()
        .id(SharedString::from(format!("control-actions-{id}")))
        .debug_selector(selector(format!("control-actions-{id}")))
        .w_full()
        .min_w_0()
        .gap_1p5()
        .items_center()
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(cx.theme().muted_foreground)
        .child(link)
        .child(
            div()
                .id(SharedString::from(format!("control-actions-label-{id}")))
                .flex_none()
                .max_w_full()
                .child(actions)
                .when(!details.is_empty(), |label| {
                    let details = details.join("\n");
                    label.with_tooltip(move |_, _| Tooltip::new(details.clone()))
                }),
        )
        .into_any_element()
}

/// Where reading a withheld record whole stands, with what its control does.
pub(crate) enum WithheldRead {
    /// Not asked for; the control loads it.
    Offered(ClickHandler),
    /// On its way; the control stops it.
    Loading(ClickHandler),
    /// The read failed with this; the control tries again.
    Failed(String, ClickHandler),
}

/// A record too large to send to this client, which the host keeps, drawn
/// as the content missing from its place, with the control that reads it
/// whole (`read`, absent where its row is unknown).
pub(crate) fn withheld_placeholder(
    id: &str,
    bytes: u64,
    read: Option<WithheldRead>,
    cx: &App,
) -> AnyElement {
    let size = crate::thread_export::format_size(bytes);
    let control_id = format!("withheld-{id}-read");
    let (label, control) = match read {
        None => (crate::tr!("chat.record_withheld", size = size), None),
        Some(WithheldRead::Offered(load)) => (
            crate::tr!("chat.record_withheld", size = size),
            Some((crate::tr!("chat.load_record"), load)),
        ),
        Some(WithheldRead::Loading(cancel)) => (
            crate::tr!("chat.record_loading", size = size),
            Some((crate::tr!("chat.cancel_record_load"), cancel)),
        ),
        Some(WithheldRead::Failed(error, load)) => (
            crate::tr!("chat.record_load_failed", size = size, error = error),
            Some((crate::tr!("chat.load_record"), load)),
        ),
    };
    h_flex()
        .id(SharedString::from(format!("withheld-{id}")))
        .debug_selector(selector(format!("withheld-{id}")))
        .w_full()
        .gap(px(10.))
        .items_center()
        .px(px(12.))
        .py(px(8.))
        .rounded(crate::material::radius_card(cx))
        .border_1()
        .border_dashed()
        .border_color(cx.theme().border)
        .text_token(&cx.theme().tokens.typography.sm)
        .text_color(cx.theme().muted_foreground)
        .child(div().min_w_0().flex_1().child(label.into_owned()))
        .children(control.map(|(action, on_click)| {
            let action = SharedString::from(action.into_owned());
            Button::new(SharedString::from(control_id.clone()))
                .ghost()
                .outline()
                .xsmall()
                .flex_none()
                .aria_label(action.clone())
                .debug_selector(selector(control_id))
                .text_token(&cx.theme().tokens.typography.xs)
                .on_click(on_click)
                .child(action)
        }))
        .into_any_element()
}
