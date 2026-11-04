//! The window for a call no family claims: "server · tool" and the argument
//! that identifies the call, then the input as readable keys and values and
//! the output as text.

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde_json::Value;
use tcode_core::session::{identifying_argument, other_argument};

use super::window::{Argument, Header, Verb, section};
use super::{Bound, Call, ToolCtx, ToolView, bounded_text, copy, output_section, thumbnails};
use crate::chat::model::one_line;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

const ICON: &str = "icons/wrench.svg";

pub(in crate::chat) struct Generic;

impl Generic {
    pub(in crate::chat) fn of(_call: &Call<'_>) -> Self {
        Self
    }
}

impl ToolView for Generic {
    fn header(&self, call: &Call<'_>, _cwd: &std::path::Path) -> Header {
        Header::with_icon(ICON, Verb("call"))
            .subject(super::subject(call.name))
            .argument(Argument::plain(
                identifying_argument(call.input)
                    .map(|argument| one_line(&argument))
                    .unwrap_or_default(),
            ))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let input = input_section(call, ctx);
        let output = output_section(call, Bound::OUTPUT, ctx);
        let images = thumbnails(call, ctx);
        super::body(input.into_iter().chain(output).chain(images))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        if !is_empty(call.input) {
            let input =
                serde_json::to_string_pretty(call.input).unwrap_or_else(|_| call.input.to_string());
            copy(&mut items, "tools.copy_arguments", &input);
        }
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::String(text) => text.is_empty(),
        _ => false,
    }
}

/// The input as its keys and values. A nested value shows folded, by its
/// size, until opened; a long string shows its first lines.
fn input_section(call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> Option<AnyElement> {
    if is_empty(call.input) {
        return None;
    }
    let label = crate::tr!("tools.input");
    let content = match call.input {
        Value::Object(map) => {
            let rows: Vec<AnyElement> = map
                .iter()
                .map(|(key, value)| {
                    let value = value_element(call, key, value, ctx);
                    let selector = format!("tool-input-{}-{key}", call.id());
                    key_value_row(selector, key, value, ctx.app())
                })
                .collect();
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .debug_selector({
                    let id = call.id().to_owned();
                    move || format!("tool-input-{id}")
                })
                .children(rows)
                .into_any_element()
        }
        Value::String(text) => bounded_text(call.entry, "input", text, true, Bound::HEAD, ctx),
        other => {
            let text = serde_json::to_string_pretty(other).unwrap_or_default();
            bounded_text(call.entry, "input", &text, true, Bound::HEAD, ctx)
        }
    };
    Some(section(&label, content, ctx.app()).into_any_element())
}

fn key_value_row(selector: String, key: &str, value: AnyElement, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .debug_selector(move || selector.clone())
        .min_w_0()
        .gap_2()
        .items_start()
        .child(
            div()
                .flex_none()
                .max_w(px(180.))
                .overflow_hidden()
                .text_meta_mono(false, cx)
                .child(crate::widgets::fade_text(key.to_owned())),
        )
        .child(div().flex_1().min_w_0().child(value))
        .into_any_element()
}

fn value_element(
    call: &Call<'_>,
    key: &str,
    value: &Value,
    ctx: &mut ToolCtx<'_, '_>,
) -> AnyElement {
    let text = |text: String, mono: bool, cx: &App| {
        div()
            .w_full()
            .min_w_0()
            .text_secondary(false, cx)
            .text_color(cx.theme().foreground.opacity(0.85))
            .when(mono, |el| {
                el.font_family(cx.theme().mono_font_family.clone())
            })
            .whitespace_normal()
            .child(text)
            .into_any_element()
    };
    match value {
        Value::String(string) => bounded_text(
            call.entry,
            &format!("input-{key}"),
            string,
            false,
            Bound::HEAD,
            ctx,
        ),
        Value::Number(_) | Value::Bool(_) | Value::Null => text(value.to_string(), true, ctx.app()),
        Value::Array(_) | Value::Object(_) => {
            let fold_key = format!("tool-nested-{}-{key}", call.id());
            let open = ctx.is_open(&fold_key);
            let summary = match value {
                Value::Array(items) => crate::tr!("tools.items", count = items.len()),
                Value::Object(map) => crate::tr!("tools.keys", count = map.len()),
                _ => unreachable!(),
            }
            .into_owned();
            let on_click = ctx.toggle(call.entry, fold_key.clone());
            let control = super::super::components::changed_files::quiet_control(
                SharedString::from(fold_key.clone()),
                summary.into(),
                on_click,
                ctx.app(),
            )
            .font_family(ctx.app().theme().mono_font_family.clone());
            let control = h_flex().child(
                div()
                    .debug_selector({
                        let fold_key = fold_key.clone();
                        move || fold_key.clone()
                    })
                    .child(control),
            );
            if !open {
                return control.into_any_element();
            }
            let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
            v_flex()
                .w_full()
                .min_w_0()
                .items_start()
                .child(control)
                .child(bounded_text(
                    call.entry,
                    &format!("nested-{key}"),
                    &pretty,
                    true,
                    Bound::HEAD,
                    ctx,
                ))
                .into_any_element()
        }
    }
}

/// An item canonicalization does not model, as a header alone: its kind and
/// the value that names it, when its summary is a JSON object.
pub(in crate::chat) fn other_header(provider_kind: &str, summary: &str) -> Header {
    Header::bare(crate::icon::IconName::Bot)
        .subject(provider_kind.to_owned())
        .argument(
            other_argument(summary)
                .map(|argument| one_line(&argument))
                .and_then(Argument::plain),
        )
}
