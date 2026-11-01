//! Items canonicalization does not model (`ItemContent::Other`) whose
//! harness kind and summary say plainly what they were: a wait, an image
//! viewed before views carried their image, an image generated. Any other
//! kind reads as its kind and the value that names it.

use std::path::Path;

use serde_json::Value;

use super::window::{Argument, Header, Verb};
use super::{copy, generic};
use crate::chat::model::{format_elapsed, one_line};
use crate::icon::IconName;

/// The summary's JSON object, when it is one.
fn summary_of(summary: &str) -> Option<Value> {
    serde_json::from_str::<Value>(summary)
        .ok()
        .filter(Value::is_object)
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key)?.as_str().filter(|text| !text.is_empty())
}

pub(in crate::chat) fn header(provider_kind: &str, summary: &str, cwd: &Path) -> Header {
    let value = summary_of(summary);
    let value = value.as_ref();
    match (provider_kind, value) {
        ("sleep", Some(value)) => {
            let waited = value
                .get("durationMs")
                .and_then(Value::as_u64)
                .map(format_elapsed);
            Header::new(IconName::Hourglass, Verb("wait")).argument(waited.and_then(Argument::mono))
        }
        ("imageView", Some(value)) => Header::new(IconName::Image, Verb("view")).argument(
            text(value, "path").and_then(|path| Argument::mono(super::display_path(path, cwd))),
        ),
        ("imageGeneration", Some(value)) => Header::new(IconName::Image, Verb("generate_image"))
            .argument(
                text(value, "revisedPrompt")
                    .map(one_line)
                    .and_then(Argument::plain),
            ),
        _ => generic::other_header(provider_kind, summary),
    }
}

/// Whether the item read an image, as a read does.
pub(in crate::chat) fn is_image_view(provider_kind: &str) -> bool {
    provider_kind == "imageView"
}

pub(in crate::chat) fn copy_items(provider_kind: &str, summary: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    match (provider_kind, summary_of(summary)) {
        ("imageView", Some(value)) => {
            copy(
                &mut items,
                "chat.copy_path",
                text(&value, "path").unwrap_or(""),
            );
        }
        // Its summary holds the whole picture as base64.
        ("imageGeneration", Some(value)) => {
            copy(
                &mut items,
                "tools.copy_prompt",
                text(&value, "revisedPrompt").unwrap_or(""),
            );
            copy(
                &mut items,
                "chat.copy_path",
                text(&value, "savedPath").unwrap_or(""),
            );
        }
        _ => copy(&mut items, "chat.copy_text", summary),
    }
    items
}
