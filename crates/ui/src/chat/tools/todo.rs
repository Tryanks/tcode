//! The plan window: a todo list a call wrote, as a checklist.
//!
//! A harness that reports its plan as plan updates (Codex's live
//! `update_plan`, Claude Code's `TodoWrite`, ACP plans) draws it in the plan
//! panel, not here; this window is for the calls that stayed tool calls: an
//! imported Codex `update_plan`, OpenCode's `todowrite`.

use std::path::Path;

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;

use super::window::{Argument, Chip, Header, Verb};
use super::{Call, ToolCtx, ToolView, copy};
use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const TODO: &[&str] = &["todowrite", "todo_write", "update_plan", "update_todos"];

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(borrow, alias = "plan")]
    todos: Vec<Todo<'a>>,
}

#[derive(Deserialize)]
struct Todo<'a> {
    #[serde(borrow, alias = "step", alias = "text", alias = "title")]
    content: &'a str,
    #[serde(default, borrow)]
    status: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Done,
    Doing,
    Pending,
    Dropped,
}

impl Todo<'_> {
    fn state(&self) -> State {
        match self.status.to_ascii_lowercase().as_str() {
            "completed" | "complete" | "done" => State::Done,
            "in_progress" | "in-progress" | "inprogress" | "active" => State::Doing,
            "cancelled" | "canceled" | "dropped" => State::Dropped,
            _ => State::Pending,
        }
    }
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    if !call.named(TODO) {
        return None;
    }
    let input = call.input::<Input<'a>>()?;
    (!input.todos.is_empty()).then(|| Box::new(Plan { input }) as Box<dyn ToolView + 'a>)
}

struct Plan<'a> {
    input: Input<'a>,
}

impl ToolView for Plan<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        let todos = &self.input.todos;
        let done = todos
            .iter()
            .filter(|todo| todo.state() == State::Done)
            .count();
        // The step under way names the plan; else the next one.
        let current = todos
            .iter()
            .find(|todo| todo.state() == State::Doing)
            .or_else(|| todos.iter().find(|todo| todo.state() == State::Pending));
        Header::new(IconName::ListOrdered, Verb("plan"))
            .argument(current.and_then(|todo| Argument::plain(todo.content.to_owned())))
            .chip(Some(Chip::Count(
                crate::tr!("tools.steps_done", done = done, total = todos.len()).into_owned(),
            )))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let cx = ctx.app();
        let theme = cx.theme();
        let rows = self.input.todos.iter().enumerate().map(|(index, todo)| {
            let state = todo.state();
            let (icon, color) = match state {
                State::Done => (IconName::CircleCheck, theme.success),
                State::Doing => (IconName::CircleDot, theme.primary),
                State::Pending => (IconName::CircleDashed, theme.muted_foreground),
                State::Dropped => (IconName::Ban, theme.muted_foreground),
            };
            let selector = format!("tool-todo-{}-{index}", call.id());
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .items_start()
                .debug_selector(move || selector.clone())
                .child(
                    div()
                        .flex_none()
                        .pt_0p5()
                        .child(Icon::new(icon).text_color(color)),
                )
                .child(
                    div()
                        .min_w_0()
                        .text_body(false, cx)
                        .whitespace_normal()
                        .text_color(match state {
                            State::Doing | State::Pending => theme.foreground,
                            State::Done | State::Dropped => theme.muted_foreground,
                        })
                        .when(state == State::Dropped, |text| text.line_through())
                        .child(todo.content.to_owned()),
                )
                .into_any_element()
        });
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .children(rows)
            .into_any_element()
    }

    fn copy_items(&self, _call: &Call<'_>) -> Vec<(String, String)> {
        let list = self
            .input
            .todos
            .iter()
            .map(|todo| {
                let mark = match todo.state() {
                    State::Done => "[x]",
                    State::Doing => "[-]",
                    State::Pending | State::Dropped => "[ ]",
                };
                format!("- {mark} {}", todo.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut items = Vec::new();
        copy(&mut items, "tools.copy_plan", &list);
        items
    }
}
