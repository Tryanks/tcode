use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::theme::ActiveTheme as _;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    Anchor, AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _,
    ObjectFit, ParentElement as _, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use tcode_core::session::SteeringStatus;

use super::assistant;
use crate::markdown::{MarkdownState, MarkdownView};

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
}

pub(crate) struct BubbleHandlers {
    pub(crate) copy: ClickHandler,
    pub(crate) images: Vec<ClickHandler>,
}

pub(crate) struct RewindHandlers {
    pub(crate) files_and_conversation: SharedClickHandler,
    pub(crate) conversation: SharedClickHandler,
    pub(crate) files: SharedClickHandler,
}

pub(crate) fn native_rewind_button(
    turn: usize,
    (state, compact): (Option<(bool, bool)>, bool),
    handlers: RewindHandlers,
    cx: &App,
) -> Option<AnyElement> {
    let (available, disabled) = state?;
    if !available {
        return None;
    }

    // Same 24px icon-only geometry as Copy: one row, one button shape.
    let trigger = assistant::action_button(
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
    .when(compact, |button| button.min_w(px(44.)).min_h(px(44.)));
    Some(
        crate::material::overlay_popover(("rewind-menu", turn))
            .anchor(Anchor::TopRight)
            .when(compact, |popover| {
                popover.bottom_sheet(crate::tr!("chat.rewind"))
            })
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let accent = cx.theme().accent;
                let popover = cx.entity();
                let RewindHandlers {
                    files_and_conversation,
                    conversation,
                    files,
                } = &handlers;
                let mut modes = Vec::new();
                if turn > 0 {
                    modes.push((
                        crate::tr!("chat.rewind_all").into_owned(),
                        files_and_conversation.clone(),
                    ));
                    modes.push((
                        crate::tr!("chat.rewind_conversation").into_owned(),
                        conversation.clone(),
                    ));
                }
                modes.push((crate::tr!("chat.rewind_files").into_owned(), files.clone()));
                let mut menu = v_flex()
                    .w(px(240.))
                    .p_1()
                    .gap_0p5()
                    .id(("rewind-options", turn))
                    .role(Role::Menu)
                    .aria_label(crate::tr!("chat.rewind"));
                for (index, (label, on_select)) in modes.into_iter().enumerate() {
                    let popover = popover.clone();
                    menu = menu.child(
                        crate::material::accessible_clickable(
                            h_flex(),
                            ("rewind-option", index),
                            Role::MenuItem,
                            label.clone(),
                            cx,
                        )
                        .w_full()
                        .when(compact, |row| row.min_h(px(44.)))
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .hover(move |style| style.bg(accent))
                        .child(Icon::new(IconName::Undo).xsmall().text_color(muted))
                        .child(label)
                        .on_click(move |event, window, cx| {
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                            on_select(event, window, cx);
                        }),
                    );
                }
                menu.into_any_element()
            })
            .into_any_element(),
    )
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
    } = data;
    let BubbleHandlers { copy, images } = handlers;
    let text_style = window.text_style();
    let text_width = visible.lines().fold(px(0.), |width, line| {
        let run = text_style.to_run(line.len());
        width.max(
            window
                .text_system()
                // Must match the bubble's rendered text size below.
                .layout_line(line, px(15.), &[run], None)
                .width,
        )
    });

    let group_key = SharedString::from(format!("user-{entry_id}"));
    let mut actions = h_flex().gap(px(2.)).items_center().justify_end();
    if !visible.trim().is_empty() {
        actions = actions.child(assistant::copy_button(
            &format!("user:{entry_id}"),
            copied,
            compact,
            copy,
            cx,
        ));
    }
    if let Some(rewind) = rewind {
        actions = actions.child(rewind);
    }

    let thumbnails = (!attachments.is_empty()).then(|| {
        h_flex().gap_2().justify_end().flex_wrap().children(
            attachments
                .iter()
                .zip(images)
                .enumerate()
                .map(|(image_index, (path, on_click))| {
                    let path = PathBuf::from(path);
                    div()
                        .id(SharedString::from(format!(
                            "user-image-{entry_id}-{image_index}"
                        )))
                        .size(px(120.))
                        .rounded_xl()
                        .overflow_hidden()
                        .bg(cx.theme().muted)
                        .cursor_pointer()
                        .child(
                            img(path)
                                .size(px(120.))
                                .rounded_xl()
                                .object_fit(ObjectFit::Cover),
                        )
                        .on_click(on_click)
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
                .into_any_element()
        },
    );
    v_flex()
        .group(group_key.clone())
        .w_full()
        .items_end()
        .gap(px(2.))
        .when_some(steering, |column, steering| {
            // Flush against the bubble's top-trailing corner: chip and bubble
            // read as one unit instead of a label floating above a message.
            column.child(
                div()
                    .h(px(18.))
                    .px(px(6.))
                    .mb(px(-2.))
                    .flex()
                    .items_center()
                    .rounded(crate::material::radius_chip())
                    .bg(cx.theme().muted)
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(match steering {
                        SteeringStatus::Pending => crate::tr!("chat.steering"),
                        SteeringStatus::Accepted => crate::tr!("chat.steered"),
                    }),
            )
        })
        .children(thumbnails)
        .when(!visible.trim().is_empty(), |column| {
            column.child({
                let pending = steering == Some(SteeringStatus::Pending);
                div()
                    .w((text_width + px(20.)).ceil() + px(3.))
                    .flex_none()
                    .max_w_3_4()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(12.))
                    .bg(cx.theme().foreground.opacity(0.08))
                    .when(pending, |bubble| {
                        bubble
                            .border_1()
                            .border_dashed()
                            .border_color(cx.theme().border)
                    })
                    .text_color(cx.theme().foreground)
                    .text_size(px(15.))
                    .child(content)
            })
        })
        .child(assistant::reserve_action_row(
            actions, group_key, pinned, compact,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    #[test]
    fn steering_status_strings_are_exact_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        assert_eq!(crate::tr!("chat.steering"), "Steering…");
        assert_eq!(crate::tr!("chat.steered"), "Steered");

        crate::set_locale(crate::LANGUAGE_SIMPLIFIED_CHINESE);
        assert_eq!(crate::tr!("chat.steering"), "引导中…");
        assert_eq!(crate::tr!("chat.steered"), "已引导");
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}
