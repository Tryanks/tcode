use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use crate::icon::{Icon, IconName};
use crate::material::event_card::{self, CardTone};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Textarea, TextareaState};
use gpui::{
    AnyElement, App, ClickEvent, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use agent::{ApprovalRequest, UserInputQuestion};
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

/// Where the request a callback carries stands, as the lead's card answers it.
pub(crate) enum CallbackRequest {
    /// The lead's status has not reached this client yet.
    Unknown,
    /// The child waits on this approval now.
    Approval {
        request: ApprovalRequest,
        /// This client's user may answer it ([`crate::composer::approval_answerable`]).
        answerable: bool,
        /// Why it cannot be answered here, while it cannot.
        refusal: SharedString,
        enabled: bool,
        decide: crate::composer::Decide,
    },
    /// The child waits on these questions now.
    Question(QuestionForm),
    /// How it was settled, as the lead's log records it: a first line, then
    /// the answer to each question.
    Settled(Vec<String>),
    /// No longer pending, and the lead's log does not say how it went.
    Gone,
}

/// A step the reader takes in a [`QuestionForm`].
pub(crate) enum QuestionAction {
    /// An option's label chosen for the question shown.
    Choose(String),
    /// Move this many questions on, or back.
    Go(isize),
    /// What is typed is the answer to the question shown.
    SendTyped,
    /// The multi-select question shown is answered.
    Confirm,
}

/// The questions a child asks, one at a time as the composer asks them, with
/// the answers chosen so far.
pub(crate) struct QuestionForm {
    pub(crate) questions: Vec<UserInputQuestion>,
    pub(crate) index: usize,
    pub(crate) selections: HashMap<String, Vec<String>>,
    pub(crate) input: Entity<TextareaState>,
    pub(crate) enabled: bool,
    pub(crate) act: Act,
}

/// Takes a reader's step in a [`QuestionForm`].
pub(crate) type Act = Rc<dyn Fn(QuestionAction, &mut Window, &mut App)>;

/// A child thread's callback to the thread that dispatched it. What the
/// callback told the parent's model is the card's body: the report, or what
/// the child waits on, previewed and then whole, never only behind a link.
/// A child waiting on an approval or a question is answered on the card,
/// which says how once it is.
#[allow(clippy::too_many_arguments)]
pub(crate) fn callback_card(
    key: &str,
    callback: &OrchestrateCallback,
    child: CallbackChild,
    request: Option<CallbackRequest>,
    steering: bool,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    hover: crate::motion::ListHover,
    motion: &crate::motion::owned::Owned,
) -> AnyElement {
    let key = key.to_owned();
    let tone = match &request {
        Some(CallbackRequest::Settled(_) | CallbackRequest::Gone) => CardTone::Neutral,
        _ => tone(&callback.event),
    };
    let waiting = tone == CardTone::Waiting;
    let title: SharedString = one_line(&callback.title).into();
    let subtitle: SharedString =
        std::iter::once(event_phrase(&callback.event, waiting).into_owned())
            .chain(child.detail)
            .collect::<Vec<_>>()
            .join(" · ")
            .into();
    let (full, preview) = texts(callback);
    // The form shows a question in the preview's place.
    let form_shown = matches!(request, Some(CallbackRequest::Question(_)));
    let long = request.is_some()
        || full.lines().count() > PREVIEW_LINES
        || full.chars().count() > PREVIEW_CHARS;
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
                (!expanded && !form_shown).then_some(()),
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
        let body = (preview.is_some() || whole.is_some()).then(|| {
            event_card::section(cx)
                .gap_0()
                .children(preview)
                .children(whole)
        });

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
        let mut decisions = None;
        let outcome = match request {
            None | Some(CallbackRequest::Unknown) => None,
            Some(CallbackRequest::Approval {
                request,
                answerable,
                refusal,
                enabled,
                decide,
            }) => {
                if answerable {
                    decisions = Some(
                        crate::composer::approval_decisions(
                            &format!("callback-approval-{key}"),
                            &request,
                            enabled,
                            false,
                            &motion,
                            decide,
                        )
                        .flex_1(),
                    );
                    None
                } else {
                    Some(
                        event_card::section(cx)
                            .child(div().text_secondary(false, cx).child(refusal)),
                    )
                }
            }
            Some(CallbackRequest::Question(form)) => {
                Some(event_card::section(cx).child(question_form(&key, form, &motion, cx)))
            }
            Some(CallbackRequest::Settled(lines)) => {
                Some(event_card::section(cx).child(settled(&key, lines, cx)))
            }
            Some(CallbackRequest::Gone) => Some(
                event_card::section(cx).child(
                    div()
                        .debug_selector({
                            let selector = format!("callback-card-gone-{key}");
                            move || selector
                        })
                        .text_secondary(false, cx)
                        .child(crate::tr!("chat.callback_card.no_longer_pending")),
                ),
            ),
        };
        let footer = (toggle.is_some() || decisions.is_some())
            .then(|| event_card::footer(cx).children(toggle).children(decisions));

        event_card::event_card(tone, cx)
            .debug_selector({
                let selector = format!("callback-card-{key}");
                move || selector
            })
            .child(header)
            .children(body)
            .children(outcome)
            .children(footer)
            .into_any_element()
    })
    .into_any_element()
}

/// How a request was settled: its first line marked done, then each
/// question's answer.
fn settled(key: &str, lines: Vec<String>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let mut lines = lines.into_iter();
    let first = lines.next().unwrap_or_default();
    v_flex()
        .debug_selector({
            let selector = format!("callback-card-settled-{key}");
            move || selector
        })
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::Check)
                        .size(px(14.))
                        .text_color(theme.muted_foreground),
                )
                .child(div().text_body(false, cx).child(first)),
        )
        .children(lines.map(|line| div().pl_6().text_secondary(false, cx).child(line)))
        .into_any_element()
}

/// The question shown, its options and a typed answer, as the composer's
/// question card offers them: a single-select choice answers it and moves
/// on, a multi-select one is confirmed, and an optional one may be passed
/// over.
fn question_form(
    key: &str,
    form: QuestionForm,
    motion: &crate::motion::owned::Owned,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let QuestionForm {
        questions,
        index,
        selections,
        input,
        enabled,
        act,
    } = form;
    let total = questions.len();
    let index = index.min(total.saturating_sub(1));
    let Some(question) = questions.get(index) else {
        return div().into_any_element();
    };
    let multi = question.multi_select;
    let title = if question.header.trim().is_empty() {
        crate::tr!("userinput.async_header").into_owned()
    } else {
        question.header.clone()
    };
    let nav = |id: &str, icon: IconName, delta: isize, disabled: bool, tooltip| {
        let act = act.clone();
        Button::new(SharedString::from(format!("{id}-{key}")))
            .motion(motion)
            .ghost()
            .xsmall()
            .icon(icon)
            .disabled(disabled)
            .tooltip(tooltip)
            .on_click(move |_, window, cx| act(QuestionAction::Go(delta), window, cx))
    };
    let strip = h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_title(cx)
                .child(crate::widgets::fade_text(title)),
        )
        .when(question.optional, |strip| {
            strip.child(
                div()
                    .flex_none()
                    .text_secondary(false, cx)
                    .child(crate::tr!("userinput.optional")),
            )
        })
        .when(total > 1, |strip| {
            strip
                .child(nav(
                    "callback-question-prev",
                    IconName::ChevronLeft,
                    -1,
                    index == 0,
                    crate::tr!("userinput.previous"),
                ))
                .child(
                    div()
                        .flex_none()
                        .text_secondary(false, cx)
                        .child(crate::tr!(
                            "userinput.question_count",
                            index = index + 1,
                            total = total
                        )),
                )
                .child(nav(
                    "callback-question-next",
                    IconName::ChevronRight,
                    1,
                    index + 1 >= total,
                    crate::tr!("userinput.next_question"),
                ))
        });

    let selected = selections.get(&question.id).cloned().unwrap_or_default();
    let mut options = v_flex()
        .id(SharedString::from(format!(
            "callback-question-options-{key}"
        )))
        .role(Role::ListBox)
        .aria_label(question.question.clone())
        .w_full()
        .gap_1();
    for (option_index, option) in question.options.iter().enumerate() {
        let chosen = selected.iter().any(|label| label == &option.label);
        let mark = div()
            .size(px(16.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(if multi {
                px(5.)
            } else {
                crate::material::radius_input(cx)
            })
            .border_1()
            .border_color(if chosen {
                theme.primary
            } else {
                theme.muted_foreground
            })
            .when(chosen, |mark| {
                mark.bg(theme.primary)
                    .child(div().size(px(6.)).rounded_full().bg(gpui::white()))
            });
        let label = option.label.clone();
        let act = act.clone();
        options = options.child(
            crate::material::menu_item_frame(false, crate::material::ItemFill::Hover, cx)
                .id(SharedString::from(format!(
                    "callback-question-option-{key}-{option_index}"
                )))
                .debug_selector({
                    let selector = format!("callback-card-option-{key}-{option_index}");
                    move || selector
                })
                .role(Role::ListBoxOption)
                .aria_label(option.label.clone())
                .aria_selected(chosen)
                .flex_none()
                .w_full()
                .items_start()
                .text_body(false, cx)
                .when(chosen, |row| row.bg(theme.list_active))
                .child(div().flex_none().pt(theme.tokens.spacing.xxs).child(mark))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().child(option.label.clone()))
                        .when(!option.description.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_secondary(false, cx)
                                    .child(option.description.clone()),
                            )
                        }),
                )
                .when(enabled, |row| {
                    row.cursor_pointer().on_click(move |_, window, cx| {
                        act(QuestionAction::Choose(label.clone()), window, cx)
                    })
                }),
        );
    }

    let typed = !input.read(cx).value().trim().is_empty();
    // Sent empty, an optional question is passed over unanswered.
    let skips = question.optional && !typed;
    let typed_answer = h_flex()
        .debug_selector({
            let selector = format!("callback-card-answer-{key}");
            move || selector
        })
        .w_full()
        .items_center()
        .gap_2()
        .pl_2()
        .rounded(crate::material::radius_input(cx))
        .border_1()
        .border_color(theme.input)
        .bg(theme.background)
        .child(
            div().flex_1().min_w_0().child(
                Textarea::new(&input)
                    .placeholder(crate::tr!("userinput.custom_placeholder"))
                    .appearance(false)
                    .disabled(!enabled),
            ),
        )
        .child(div().flex_none().self_end().child({
            let act = act.clone();
            Button::new(SharedString::from(format!("callback-question-send-{key}")))
                .motion(motion)
                .ghost()
                .xsmall()
                .icon(IconName::ArrowUp)
                .disabled(!enabled || (!typed && !skips))
                .tooltip(if skips {
                    crate::tr!("userinput.skip")
                } else {
                    crate::tr!("userinput.submit_custom")
                })
                .debug_selector({
                    let selector = format!("callback-card-send-{key}");
                    move || selector
                })
                .on_click(move |_, window, cx| act(QuestionAction::SendTyped, window, cx))
        }));

    // A multi-select choice cannot say the answer is finished, so that
    // question carries the button that does.
    let confirm = (multi && crate::composer::model::user_input_answered(question, &selections))
        .then(|| {
            let last = crate::composer::model::user_input_all_answered(&questions, &selections);
            Button::new(SharedString::from(format!(
                "callback-question-confirm-{key}"
            )))
            .motion(motion)
            .primary()
            .chip()
            .disabled(!enabled)
            .label(if last {
                crate::tr!("userinput.done")
            } else {
                crate::tr!("userinput.next_question")
            })
            .debug_selector({
                let selector = format!("callback-card-confirm-{key}");
                move || selector
            })
            .on_click(move |_, window, cx| act(QuestionAction::Confirm, window, cx))
        });
    let footer = h_flex()
        .w_full()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_secondary(false, cx)
                .when(multi, |hint| hint.child(crate::tr!("userinput.multi_hint"))),
        )
        .children(confirm);

    v_flex()
        .debug_selector({
            let selector = format!("callback-card-question-{key}");
            move || selector
        })
        .w_full()
        .gap(theme.tokens.spacing.sm)
        .child(strip)
        .child(div().text_body(false, cx).child(question.question.clone()))
        .when(!question.options.is_empty(), |form| form.child(options))
        .child(typed_answer)
        .child(footer)
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

/// What the callback says of the child; a request no longer waited on is
/// told in the past.
fn event_phrase(event: &CallbackEvent, waiting: bool) -> Cow<'static, str> {
    match event {
        CallbackEvent::Ended(state) => match state.as_str() {
            "completed" => crate::tr!("chat.callback_card.completed"),
            "reported" => crate::tr!("chat.callback_card.reported"),
            "updated" => crate::tr!("chat.callback_card.updated"),
            "unreported" => crate::tr!("chat.callback_card.unreported"),
            "failed" => crate::tr!("chat.callback_card.failed"),
            other => Cow::Owned(other.to_string()),
        },
        CallbackEvent::Approval if waiting => crate::tr!("chat.callback_card.approval"),
        CallbackEvent::Approval => crate::tr!("chat.callback_card.asked_approval"),
        CallbackEvent::Question if waiting => crate::tr!("chat.callback_card.question"),
        CallbackEvent::Question => crate::tr!("chat.callback_card.asked_question"),
    }
}

/// How a settled request went, in the lines [`CallbackRequest::Settled`]
/// shows: what was chosen, or each question with its answer.
pub(crate) fn settled_lines(
    answer: &agent::ChildRequestAnswer,
    callback: &OrchestrateCallback,
) -> Vec<String> {
    match answer {
        agent::ChildRequestAnswer::Approval { decision, label } => vec![match decision {
            agent::ApprovalDecision::Option(id) => crate::tr!(
                "chat.callback_card.answered",
                answer = label.as_deref().unwrap_or(id)
            )
            .into_owned(),
            agent::ApprovalDecision::Cancel => {
                crate::tr!("chat.callback_card.cancelled").into_owned()
            }
            agent::ApprovalDecision::Feedback(text) => {
                crate::tr!("chat.callback_card.declined", feedback = text).into_owned()
            }
        }],
        agent::ChildRequestAnswer::Input { answers } if answers.is_empty() => {
            vec![crate::tr!("chat.callback_card.unanswered").into_owned()]
        }
        agent::ChildRequestAnswer::Input { answers } => {
            let questions = callback.questions().unwrap_or_default();
            let value = |value: &serde_json::Value| {
                let words = match value {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Array(items) => items
                        .iter()
                        .map(|item| match item {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    other => other.to_string(),
                };
                if words.trim().is_empty() {
                    crate::tr!("chat.callback_card.skipped").into_owned()
                } else {
                    one_line(&words)
                }
            };
            let asked = questions.iter().filter_map(|question| {
                let answer = answers.get(&question.id)?;
                let name = if question.header.trim().is_empty() {
                    question.question.trim()
                } else {
                    question.header.trim()
                };
                Some(format!("{}: {}", one_line(name), value(answer)))
            });
            let rest = answers
                .iter()
                .filter(|(id, _)| !questions.iter().any(|question| &question.id == *id))
                .map(|(id, answer)| format!("{}: {}", one_line(id), value(answer)));
            std::iter::once(crate::tr!("chat.callback_card.answered_plain").into_owned())
                .chain(asked)
                .chain(rest)
                .collect()
        }
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
    .debug_selector({
        let selector = format!("callback-card-open-{key}");
        move || selector
    })
    .child(crate::widgets::fade_text(crate::tr!("agents.open_child")))
    .into_any_element()
}
