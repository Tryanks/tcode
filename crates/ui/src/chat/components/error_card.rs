use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, ClickEvent, IntoElement as _, ParentElement as _, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

pub(crate) enum LimitResume {
    /// No resume is queued and the reset is still ahead: offer to schedule one.
    Offer { on_schedule: ClickHandler },
    /// A resume is queued for the reset: live countdown plus cancel.
    Scheduled {
        remaining_secs: u64,
        on_cancel: ClickHandler,
    },
}

/// A provider/app error as a first-class timeline block: a danger-tinted card
/// carrying the full message, wrapped across as many lines as it needs.
pub(crate) fn error_card(
    id: &str,
    message: &str,
    copied: bool,
    resume: Option<LimitResume>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let danger = cx.theme().danger;
    let copy = Button::new(SharedString::from(format!("copy-error:{id}")))
        .ghost()
        .xsmall()
        .icon(if copied {
            IconName::Check
        } else {
            IconName::Copy
        })
        .label(if copied {
            crate::tr!("chat.copied")
        } else {
            crate::tr!("chat.copy")
        })
        .on_click(on_copy);
    let resume_row = resume.map(|resume| match resume {
        LimitResume::Offer { on_schedule } => h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("schedule-limit-resume:{id}")))
                    .outline()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .label(crate::tr!("chat.limit_resume.schedule"))
                    .on_click(on_schedule),
            )
            .into_any_element(),
        LimitResume::Scheduled {
            remaining_secs,
            on_cancel,
        } => h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger_foreground)
                    .child(crate::tr!(
                        "chat.limit_resume.resumes_in",
                        countdown = crate::composer::model::format_countdown(remaining_secs)
                    )),
            )
            .child(div().flex_1())
            .child(
                Button::new(SharedString::from(format!("cancel-limit-resume:{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .label(crate::tr!("chat.limit_resume.cancel"))
                    .on_click(on_cancel),
            )
            .into_any_element(),
    });
    let content = v_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .p_3()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .xsmall()
                        .text_color(cx.theme().danger_foreground),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .font_medium()
                        .text_color(cx.theme().danger_foreground)
                        .child(crate::material::tracked_uppercase(
                            crate::tr!("chat.error_label").as_ref(),
                        )),
                )
                .child(div().flex_1())
                .child(copy),
        )
        .child(
            div()
                .w_full()
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(cx.theme().danger_foreground)
                .whitespace_normal()
                .child(message.to_string()),
        )
        .when_some(resume_row, |content, row| content.child(row));
    h_flex()
        .w_full()
        .items_stretch()
        .rounded(crate::material::radius_card())
        .overflow_hidden()
        .border_1()
        .border_color(danger.opacity(0.22))
        .bg(danger.opacity(0.12))
        .child(
            div()
                .flex_none()
                .w(px(2.))
                .ml(px(8.))
                .my(px(8.))
                .rounded_full()
                .bg(danger),
        )
        .child(content)
        .into_any_element()
}
