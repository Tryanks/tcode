//! The question window: questions the agent put to the user as a call, each
//! with the options it offered and the answer it got back.

use std::path::Path;

use agent::{AskedQuestion, ToolFamily};
use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use super::window::{Argument, Chip, Header, Verb};
use super::{Call, ToolCtx, ToolView, copy};
use crate::chat::model::one_line;
use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let Some(ToolFamily::Question { questions }) = call.family else {
        return None;
    };
    (!questions.is_empty()).then(|| Box::new(Ask { questions }) as Box<dyn ToolView + 'a>)
}

struct Ask<'a> {
    questions: &'a [AskedQuestion],
}

impl ToolView for Ask<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        let questions = self.questions;
        Header::new(IconName::CircleQuestionMark, Verb("ask"))
            .argument(Argument::plain(one_line(&questions[0].question)))
            .chip((questions.len() > 1).then(|| {
                Chip::Count(crate::tr!("tools.questions", count = questions.len()).into_owned())
            }))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let cx = ctx.app();
        let theme = cx.theme();
        let questions = self.questions.iter().enumerate().map(|(index, question)| {
            let chosen = &question.answers;
            let options = question.options.iter().map(|choice| {
                let picked = chosen.contains(&choice.label);
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .items_start()
                    .child(
                        div().flex_none().pt_0p5().child(
                            Icon::new(if picked {
                                IconName::CircleCheck
                            } else {
                                IconName::CircleDashed
                            })
                            .text_color(if picked {
                                theme.success
                            } else {
                                theme.muted_foreground
                            }),
                        ),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_color(if picked {
                                        theme.foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .when(picked, |label| label.font_medium())
                                    .child(choice.label.clone()),
                            )
                            .when(!choice.description.is_empty(), |column| {
                                column.child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .whitespace_normal()
                                        .child(choice.description.clone()),
                                )
                            }),
                    )
            });
            // An answer in the user's own words matches no option.
            let typed = chosen.iter().filter(|answer| {
                !question
                    .options
                    .iter()
                    .any(|choice| &choice.label == *answer)
            });
            let selector = format!("tool-question-{}-{index}", call.id());
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .debug_selector(move || selector.clone())
                .child(
                    div()
                        .font_medium()
                        .text_color(theme.foreground)
                        .whitespace_normal()
                        .child(question.question.clone()),
                )
                .children(options)
                .children(typed.map(|answer| {
                    div()
                        .text_color(theme.foreground)
                        .child(crate::tr!("tools.answered", answer = answer).into_owned())
                }))
                .into_any_element()
        });
        let questions = v_flex()
            .w_full()
            .min_w_0()
            .gap_3()
            .text_secondary(false, cx)
            .children(questions)
            .into_any_element();
        // What the harness gave back, which the answers were read from.
        let raw = call
            .output
            .filter(|output| !output.trim().is_empty())
            .map(|output| super::raw_section(call.entry, output, ctx));
        super::body(std::iter::once(questions).chain(raw))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        let questions = self
            .questions
            .iter()
            .map(|question| question.question.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        copy(&mut items, "chat.copy_text", &questions);
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}
