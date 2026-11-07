use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::icon::IconName;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu};
use gpui::{
    AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _, ObjectFit,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::rc::Rc;

use tcode_core::session::SteeringStatus;

use super::super::OpenRewind;
use super::assistant;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};
use crate::widgets::copy::{action_button, copy_button};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a picture drawn here takes to fade in once it has decoded.
const ARRIVE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub(crate) type UserMessageArgs<'a> = (
    usize,
    &'a str,
    &'a str,
    &'a Path,
    Option<usize>,
    &'a [String],
    Option<SteeringStatus>,
    bool,
);

pub(crate) struct BubbleData<'a> {
    pub(crate) entry_id: &'a str,
    /// A steer the agent just took, on its way from the turn's trailer: how
    /// far it has come. Its chip and border change from pending as it goes.
    pub(crate) taken: Option<f32>,
    pub(crate) visible: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) attachments: &'a [String],
    pub(crate) steering: Option<SteeringStatus>,
    pub(crate) pinned: bool,
    pub(crate) compact: bool,
    pub(crate) copied: bool,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) rewind: Option<AnyElement>,
    /// The rewind the context menu offers, when the harness has one.
    pub(crate) rewind_menu: Option<RewindMenu>,
    /// The timeline's hover, which reveals the message's actions.
    pub(crate) hover: crate::motion::ListHover,
    /// The chat's motion, which keeps the rewind's fold and the pictures'
    /// fades.
    pub(crate) motion: crate::motion::owned::Owned,
}

/// The rewind a message offers: where it goes, and why it cannot start now.
#[derive(Clone, PartialEq)]
pub(crate) struct RewindMenu {
    /// The message's entry id.
    pub(crate) key: String,
    pub(crate) turn: usize,
    pub(crate) steer: Option<String>,
    /// Why no rewind can start now, which the disabled action says.
    pub(crate) blocked: Option<String>,
    /// The rewind to this message is being made.
    pub(crate) in_progress: bool,
}

/// The bubble's context-menu items: copy the message, then the rewind,
/// which opens the panel the bubble's rewind action opens.
/// Shared by the Markdown inside the bubble and the bubble's own margin.
fn bubble_menu_items(copy_text: Arc<str>, rewind: Option<RewindMenu>) -> MenuExtension {
    Rc::new(move |menu: PopupMenu, window: &mut Window, cx: &mut App| {
        let menu = if copy_text.trim().is_empty() {
            menu
        } else {
            assistant::copy_message_items(copy_text.clone())(menu, window, cx)
        };
        match &rewind {
            Some(rewind) => menu.separator().menu_with_enable(
                crate::tr!("rewind.action_menu").into_owned(),
                Box::new(OpenRewind {
                    key: rewind.key.clone(),
                    turn: rewind.turn,
                    steer: rewind.steer.clone(),
                }),
                rewind.blocked.is_none() && !rewind.in_progress,
            ),
            None => menu,
        }
    })
}

pub(crate) struct BubbleHandlers {
    pub(crate) copy: ClickHandler,
    pub(crate) images: Vec<ClickHandler>,
}

/// The message's rewind action, which opens the panel.
pub(crate) fn rewind_button(
    menu: &RewindMenu,
    compact: bool,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> crate::widgets::Button {
    let key = menu.key.clone();
    action_button(
        SharedString::from(format!("rewind-{key}")),
        IconName::Undo,
        menu.blocked
            .clone()
            .unwrap_or_else(|| crate::tr!("rewind.action").into_owned()),
        cx,
    )
    .disabled(menu.blocked.is_some())
    .touch(compact)
    .motion(motion)
    .debug_selector(move || format!("rewind-{key}"))
}

/// What the action row shows while the rewind to this message is made.
pub(crate) fn rewind_in_progress(key: &str, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let selector = format!("rewind-in-progress-{key}");
    h_flex()
        .gap_1()
        .px_1()
        .items_center()
        .debug_selector(move || selector)
        .text_token(&cx.theme().tokens.typography.xs)
        .text_color(muted)
        .child(crate::widgets::Spinner::new().xsmall().color(muted))
        .child(crate::tr!("rewind.in_progress").into_owned())
        .into_any_element()
}

/// A user message: right-aligned bubble, attachment thumbnails, and actions.
pub(crate) fn user_bubble(
    data: BubbleData<'_>,
    handlers: BubbleHandlers,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    let BubbleData {
        entry_id,
        taken,
        visible,
        cwd,
        attachments,
        steering,
        pinned,
        compact,
        copied,
        markdown,
        rewind,
        rewind_menu,
        hover,
        motion,
    } = data;
    let BubbleHandlers { copy, images } = handlers;
    let menu_items = bubble_menu_items(Arc::from(visible), rewind_menu.clone());
    let prose = cx.theme().tokens.typography.md;
    let spacing = &cx.theme().tokens.spacing;
    let text_style = window.text_style();
    let text_width = visible.lines().fold(px(0.), |width, line| {
        let run = text_style.to_run(line.len());
        width.max(
            window
                .text_system()
                // Must match the bubble's rendered text size below.
                .layout_line(line, prose.size, &[run], None)
                .width,
        )
    });

    let hover_key = SharedString::from(format!("user-{entry_id}"));
    // Only a row that reveals something on hover takes the timeline's hover,
    // so a row moving under a still pointer redraws nothing it need not.
    let reveals = !pinned && !compact;
    let reveal = if reveals {
        hover.reveal(&hover_key, window, cx)
    } else {
        1.
    };
    let copy = (!visible.trim().is_empty()).then(|| {
        copy_button(&format!("user:{entry_id}"), copied, compact, copy, cx).motion(&motion)
    });
    // The rewind comes into the row beside Copy once the harness offers it,
    // and goes as it was, inert, when it stops.
    let rewind_id = SharedString::from(format!("rewind-fold-{entry_id}"));
    let offered = rewind_menu.clone().filter(|_| rewind.is_some());
    let mut rewind = rewind;
    let actions = crate::motion::Sampled::new({
        let motion = motion.clone();
        move |window, cx| {
            let gap = px(2.);
            let rewind = motion
                .shown(rewind_id, offered, SHOW, window, cx)
                .map(|shown| {
                    let button = match rewind.take() {
                        Some(button) if !shown.leaving() => button,
                        _ => rewind_button(&shown.value, compact, &motion, cx).into_any_element(),
                    };
                    shown.fold_across_gap(gap, button)
                });
            h_flex()
                .gap(gap)
                .items_center()
                .justify_end()
                .children(copy)
                .children(rewind)
                .into_any_element()
        }
    });

    let thumbnails = (!attachments.is_empty()).then(|| {
        h_flex().gap_2().justify_end().flex_wrap().children(
            attachments
                .iter()
                .zip(images)
                .enumerate()
                .map(|(image_index, (path, on_click))| {
                    let menu_path = path.clone();
                    let path = PathBuf::from(path);
                    // The image is the control; a `Button` would add chrome
                    // around a picture.
                    crate::material::accessible_clickable(
                        div(),
                        SharedString::from(format!("user-image-{entry_id}-{image_index}")),
                        Role::Button,
                        crate::tr!("markdown.open_image").into_owned(),
                        cx,
                    )
                    .size(px(120.))
                    .rounded(crate::material::radius_card(cx))
                    .overflow_hidden()
                    .bg(cx.theme().muted)
                    .cursor_pointer()
                    .child(crate::motion::Sampled::new({
                        let source = crate::store::image_preview(path, 120., true);
                        let id = SharedString::from(format!("user-image-{entry_id}-{image_index}"));
                        let motion = motion.clone();
                        // The picture fades in over the tile's fill once it
                        // decodes.
                        move |window, cx| {
                            let arrived = crate::motion::image_arrival(
                                crate::motion::Clock::from(&motion),
                                id,
                                &source,
                                ARRIVE,
                                window,
                                cx,
                            );
                            img(source)
                                .id("attachment-image")
                                .size(px(120.))
                                .rounded(crate::material::radius_card(cx))
                                .object_fit(ObjectFit::Cover)
                                .opacity(arrived)
                                .with_fallback(crate::image_viewer::unpreviewed(true, false, cx))
                                .into_any_element()
                        }
                    }))
                    .on_click(on_click)
                    .context_menu(move |menu, _, _| menu.path_items(&menu_path, None))
                })
                .collect::<Vec<_>>(),
        )
    });

    let content = markdown.map_or_else(
        || div().child(visible.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .selectable(true)
                .compact_headings(true)
                .base_dir(cwd)
                .menu_extension(menu_items.clone())
                .into_any_element()
        },
    );
    let bubble_selector = format!("user-bubble-{entry_id}");
    v_flex()
        .id(hover_key.clone())
        .when(reveals, |column| column.on_hover(hover.on_hover(hover_key)))
        .debug_selector(move || bubble_selector)
        .w_full()
        .items_end()
        .gap(px(2.))
        .when_some(steering, |column, steering| {
            let selector = format!("steering-{entry_id}");
            let chip = |label| {
                crate::material::semantic_chip(
                    label,
                    cx.theme().muted,
                    cx.theme().muted_foreground,
                    cx,
                )
            };
            let label = match steering {
                SteeringStatus::Pending => crate::tr!("chat.steering"),
                SteeringStatus::Accepted => crate::tr!("chat.steered"),
            };
            // A steer just taken cross-fades from its pending chip.
            column.child(
                div()
                    .relative()
                    .debug_selector(move || selector)
                    .child(chip(label).when_some(taken, |chip, taken| chip.opacity(taken)))
                    .when_some(taken.filter(|taken| *taken < 1.), |frame, taken| {
                        frame.child(
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .opacity(1. - taken)
                                .child(chip(crate::tr!("chat.steering"))),
                        )
                    }),
            )
        })
        .children(thumbnails)
        .when(!visible.trim().is_empty(), |column| {
            column.child({
                // The pending border fades as a taken steer travels.
                let pending = match (steering, taken) {
                    (Some(SteeringStatus::Pending), _) => Some(1.),
                    (Some(SteeringStatus::Accepted), Some(taken)) if taken < 1. => Some(1. - taken),
                    _ => None,
                };
                div()
                    .w((text_width + spacing.md * 2.).ceil() + px(3.))
                    .flex_none()
                    .max_w_3_4()
                    .px(spacing.md)
                    .py(spacing.xs)
                    .rounded(crate::material::radius_composer(cx))
                    .bg(cx.theme().foreground.opacity(0.08))
                    .when_some(pending, |bubble, pending| {
                        bubble
                            .border_1()
                            .border_dashed()
                            .border_color(cx.theme().border.opacity(pending))
                    })
                    .text_color(cx.theme().foreground)
                    .text_token(&prose)
                    .child(content)
            })
        })
        .child(assistant::reserve_action_row(
            actions, reveal, pinned, compact,
        ))
        .context_menu(move |menu, window, cx| menu_items(menu, window, cx))
        .into_any_element()
}
