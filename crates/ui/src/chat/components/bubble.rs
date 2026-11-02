use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, PopupMenu};
use agent::RewindMode;
use gpui::{
    Anchor, AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _,
    ObjectFit, ParentElement as _, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::rc::Rc;

use tcode_core::session::SteeringStatus;

use super::super::RewindTurn;
use super::assistant;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};
use crate::widgets::copy::{action_button, copy_button};

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
pub(crate) type SharedClickHandler = Arc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
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
}

#[derive(Clone, PartialEq)]
pub(crate) struct RewindMenu {
    pub(crate) turn: usize,
    pub(crate) disabled: bool,
    /// The modes the harness restores this turn in, in its order.
    pub(crate) modes: Vec<RewindMode>,
}

fn rewind_label(mode: RewindMode) -> String {
    match mode {
        RewindMode::FilesAndConversation => crate::tr!("chat.rewind_all"),
        RewindMode::Conversation => crate::tr!("chat.rewind_conversation"),
        RewindMode::Files => crate::tr!("chat.rewind_files"),
    }
    .into_owned()
}

/// The bubble's context-menu items: copy the message, then the rewind modes
/// the popover button offers. Shared by the Markdown inside the bubble and
/// the bubble's own margin.
fn bubble_menu_items(copy_text: Arc<str>, rewind: Option<RewindMenu>) -> MenuExtension {
    Rc::new(move |menu: PopupMenu, window: &mut Window, cx: &mut App| {
        let menu = if copy_text.trim().is_empty() {
            menu
        } else {
            assistant::copy_message_items(copy_text.clone())(menu, window, cx)
        };
        let Some(RewindMenu {
            turn,
            disabled,
            modes,
        }) = &rewind
        else {
            return menu;
        };
        modes.iter().fold(menu.separator(), |menu, &mode| {
            menu.menu_with_enable(
                rewind_label(mode),
                Box::new(RewindTurn { turn: *turn, mode }),
                !disabled,
            )
        })
    })
}

pub(crate) struct BubbleHandlers {
    pub(crate) copy: ClickHandler,
    pub(crate) images: Vec<ClickHandler>,
}

/// The rewind popover for a turn: one item per mode in `menu`, each running
/// its handler.
pub(crate) fn native_rewind_button(
    menu: RewindMenu,
    compact: bool,
    handlers: Vec<SharedClickHandler>,
    cx: &App,
) -> AnyElement {
    let RewindMenu {
        turn,
        disabled,
        modes,
    } = menu;

    let trigger = action_button(
        SharedString::from(format!("rewind-{turn}")),
        IconName::Undo,
        if disabled {
            crate::tr!("chat.rewind_blocked").into_owned()
        } else {
            crate::tr!("chat.rewind").into_owned()
        },
        cx,
    )
    .disabled(disabled)
    .touch(compact)
    .debug_selector(move || format!("rewind-{turn}"));
    crate::material::overlay_popover(("rewind-menu", turn), cx)
        .anchor(Anchor::TopRight)
        .when(compact, |popover| {
            popover.bottom_sheet(crate::tr!("chat.rewind"))
        })
        .trigger(trigger)
        .content(move |_state, _window, cx| {
            let muted = cx.theme().muted_foreground;
            let hover = cx.theme().muted;
            let popover = cx.entity();
            let mut menu = v_flex()
                .w(px(240.))
                .p_1()
                .gap_0p5()
                .id(("rewind-options", turn))
                .role(Role::Menu)
                .aria_label(crate::tr!("chat.rewind"));
            for (index, (&mode, on_select)) in modes.iter().zip(&handlers).enumerate() {
                let label = rewind_label(mode);
                let on_select = on_select.clone();
                let popover = popover.clone();
                menu = menu.child(
                    crate::material::accessible_clickable(
                        crate::material::menu_item_frame(compact, cx),
                        ("rewind-option", index),
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .debug_selector(move || format!("rewind-option-{mode:?}"))
                    .w_full()
                    .items_center()
                    .cursor_pointer()
                    .text_body(compact, cx)
                    .hover(move |style| style.bg(hover))
                    .child(Icon::new(IconName::Undo).text_color(muted))
                    .child(label)
                    .on_click(move |event, window, cx| {
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                        on_select(event, window, cx);
                    }),
                );
            }
            menu.into_any_element()
        })
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
    let copy = (!visible.trim().is_empty())
        .then(|| copy_button(&format!("user:{entry_id}"), copied, compact, copy, cx));
    // The rewind comes into the row beside Copy once the harness offers it,
    // and goes as it was, inert, when it stops.
    let rewind_id = SharedString::from(format!("rewind-fold-{entry_id}"));
    let offered = rewind_menu.clone().filter(|_| rewind.is_some());
    let mut rewind = rewind;
    let actions = crate::motion::Sampled::new(move |window, cx| {
        let gap = px(2.);
        let rewind = crate::motion::shown(rewind_id, offered, window, cx).map(|shown| {
            let button = match rewind.take() {
                Some(button) if !shown.leaving() => button,
                _ => native_rewind_button(shown.value.clone(), compact, Vec::new(), cx),
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
                        // The picture fades in over the tile's fill once it
                        // decodes.
                        move |window, cx| {
                            let arrived = crate::motion::image_arrival(id, &source, window, cx);
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
            column.child(
                crate::material::semantic_chip(
                    match steering {
                        SteeringStatus::Pending => crate::tr!("chat.steering"),
                        SteeringStatus::Accepted => crate::tr!("chat.steered"),
                    },
                    cx.theme().muted,
                    cx.theme().muted_foreground,
                    cx,
                )
                .debug_selector(move || selector),
            )
        })
        .children(thumbnails)
        .when(!visible.trim().is_empty(), |column| {
            column.child({
                let pending = steering == Some(SteeringStatus::Pending);
                div()
                    .w((text_width + spacing.md * 2.).ceil() + px(3.))
                    .flex_none()
                    .max_w_3_4()
                    .px(spacing.md)
                    .py(spacing.xs)
                    .rounded(crate::material::radius_composer(cx))
                    .bg(cx.theme().foreground.opacity(0.08))
                    .when(pending, |bubble| {
                        bubble
                            .border_1()
                            .border_dashed()
                            .border_color(cx.theme().border)
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
