//! The question window: questions the agent put to the user as a call
//! (Claude Code's `AskUserQuestion`, Codex's `request_user_input`), each
//! with the options it offered and the answer it got back.

use std::path::Path;

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use serde::Deserialize;
use serde_json::Value;

use super::window::{Argument, Chip, Header, Verb};
use super::{Call, ToolCtx, ToolView, copy};
use crate::chat::model::one_line;
use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const QUESTIONS: &[&str] = &[
    "askuserquestion",
    "request_user_input",
    "request_user_input_async",
    "ask_user_question",
    "question",
];

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(borrow)]
    questions: Vec<Question<'a>>,
}

#[derive(Deserialize)]
struct Question<'a> {
    #[serde(default, borrow)]
    id: Option<&'a str>,
    #[serde(borrow, alias = "title", alias = "prompt")]
    question: &'a str,
    #[serde(default)]
    options: Vec<Choice>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Choice {
    Label(String),
    Option {
        label: String,
        #[serde(default)]
        description: Option<String>,
    },
}

impl Choice {
    fn label(&self) -> &str {
        match self {
            Self::Label(label) | Self::Option { label, .. } => label,
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Option { description, .. } => description.as_deref(),
            Self::Label(_) => None,
        }
    }
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    if !call.named(QUESTIONS) {
        return None;
    }
    let input = call.input::<Input<'a>>()?;
    (!input.questions.is_empty()).then(|| Box::new(Ask { input }) as Box<dyn ToolView + 'a>)
}

struct Ask<'a> {
    input: Input<'a>,
}

impl Ask<'_> {
    /// The answers each question got, as the harness reported them: Claude
    /// Code's `"question"="answer"` pairs, or Codex's
    /// `{"answers": {id: {"answers": […]}}}`.
    fn answers(&self, output: Option<&str>) -> Vec<Vec<String>> {
        let output = output.unwrap_or_default();
        let codex = serde_json::from_str::<Value>(output.trim()).ok();
        self.input
            .questions
            .iter()
            .map(|question| {
                if let Some(answers) = codex.as_ref().and_then(|value| {
                    value
                        .get("answers")?
                        .get(question.id?)?
                        .get("answers")?
                        .as_array()
                }) {
                    return answers
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                }
                let asked = format!("\"{}\"=\"", question.question);
                output
                    .find(&asked)
                    .and_then(|start| {
                        let rest = &output[start + asked.len()..];
                        rest.find('"').map(|end| rest[..end].to_owned())
                    })
                    .map(|answer| answer.split(", ").map(str::to_owned).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect()
    }
}

impl ToolView for Ask<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        let questions = &self.input.questions;
        Header::new(IconName::CircleQuestionMark, Verb("ask"))
            .argument(Argument::plain(one_line(questions[0].question)))
            .chip((questions.len() > 1).then(|| {
                Chip::Count(crate::tr!("tools.questions", count = questions.len()).into_owned())
            }))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let answers = self.answers(call.output);
        let cx = ctx.app();
        let theme = cx.theme();
        let questions = self.input.questions.iter().zip(&answers).enumerate().map(
            |(index, (question, chosen))| {
                let options = question.options.iter().map(|choice| {
                    let picked = chosen.iter().any(|answer| answer == choice.label());
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
                                        .child(choice.label().to_owned()),
                                )
                                .when_some(choice.description(), |column, description| {
                                    column.child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .whitespace_normal()
                                            .child(description.to_owned()),
                                    )
                                }),
                        )
                });
                // An answer in the user's own words matches no option.
                let typed: Vec<&String> = chosen
                    .iter()
                    .filter(|answer| {
                        !question
                            .options
                            .iter()
                            .any(|choice| choice.label() == answer.as_str())
                    })
                    .collect();
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
                            .child(question.question.to_owned()),
                    )
                    .children(options)
                    .children(typed.into_iter().map(|answer| {
                        div()
                            .text_color(theme.foreground)
                            .child(crate::tr!("tools.answered", answer = answer).into_owned())
                    }))
                    .into_any_element()
            },
        );
        v_flex()
            .w_full()
            .min_w_0()
            .gap_3()
            .text_secondary(false, cx)
            .children(questions)
            .into_any_element()
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        let questions = self
            .input
            .questions
            .iter()
            .map(|question| question.question)
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
