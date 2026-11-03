//! A tool result as the timeline keeps it: the text the tool returned in
//! [`ItemContent::ToolCall::output`](crate::ItemContent::ToolCall), and its
//! inline images in `image_reads`, whatever shape the provider wrapped them
//! in. That is what the harness got back, not necessarily what it gave the
//! model: an MCP result's `structuredContent` is in the text too.

use base64::Engine as _;
use serde_json::Value;

use crate::Attachment;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<Attachment>,
}

impl ToolOutput {
    /// A result's content blocks — MCP `CallToolResult` (or its `content`),
    /// Claude `tool_result.content`, Codex `contentItems` and rollout output
    /// arrays, pi and ACP content, OpenCode tool content — as text and images. Text parts are joined
    /// by newlines; a block that is neither text nor an inline image (a
    /// resource, a tool reference) stays in the text as its JSON. An MCP
    /// result's `structuredContent` follows as one line of JSON, unless the
    /// text already holds it.
    pub fn from_content(content: &Value) -> Self {
        let mut output = Self::default();
        match content {
            Value::Null => {}
            Value::String(text) => output.text.clone_from(text),
            Value::Array(blocks) => {
                let mut parts = Vec::new();
                for block in blocks {
                    match block_part(block) {
                        Part::Text(text) => parts.push(text),
                        Part::Image(image) => output.images.push(image),
                    }
                }
                output.text = parts.join("\n");
            }
            Value::Object(result) if result.get("content").is_some_and(Value::is_array) => {
                output = Self::from_content(&result["content"]);
                // A connector may put its whole payload in `structuredContent`
                // with only an acknowledgement as text, and which of the two
                // the model is given is the harness's choice; dropping either
                // would hide part of what the tool returned.
                if let Some(structured) = result
                    .get("structuredContent")
                    .filter(|value| !value.is_null())
                    && !output.text.lines().any(|line| {
                        serde_json::from_str::<Value>(line).is_ok_and(|text| &text == structured)
                    })
                    && serde_json::from_str::<Value>(&output.text).ok().as_ref() != Some(structured)
                {
                    if !output.text.is_empty() {
                        output.text.push('\n');
                    }
                    output.text.push_str(&structured.to_string());
                }
            }
            other => output.text = other.to_string(),
        }
        output
    }

    /// An `output` string recorded in an earlier provider shape, as
    /// canonical text and images: a Codex MCP result or `contentItems` JSON, a
    /// Codex rollout content array, or Claude text with image blocks inlined
    /// as JSON lines. `None` when the text is in none of those shapes.
    pub fn split(output: &str) -> Option<Self> {
        let trimmed = output.trim();
        if trimmed.starts_with(['{', '['])
            && let Ok(value) = serde_json::from_str::<Value>(trimmed)
        {
            let blocks = match &value {
                Value::Object(result) => result.get("content").and_then(Value::as_array),
                Value::Array(blocks) => Some(blocks),
                _ => None,
            };
            if blocks.is_some_and(|blocks| !blocks.is_empty() && blocks.iter().all(is_block)) {
                return Some(Self::from_content(&value));
            }
        }
        if !output.contains("{\"type\":\"image\"") {
            return None;
        }
        let mut split = Self::default();
        let mut lines = Vec::new();
        for line in output.split('\n') {
            match serde_json::from_str::<Value>(line).map(|block| block_part(&block)) {
                Ok(Part::Image(image)) => split.images.push(image),
                _ => lines.push(line),
            }
        }
        if split.images.is_empty() {
            return None;
        }
        split.text = lines.join("\n");
        Some(split)
    }
}

enum Part {
    Text(String),
    Image(Attachment),
}

fn block_part(block: &Value) -> Part {
    let text = |key: &str| block.get(key).and_then(Value::as_str).map(str::to_owned);
    let kind = block.get("type").and_then(Value::as_str);
    let part = match kind {
        Some("text" | "input_text" | "inputText" | "output_text") => text("text").map(Part::Text),
        Some("input_image") => text("image_url")
            .and_then(|url| data_url(&url))
            .map(Part::Image),
        Some("inputImage") => text("imageUrl")
            .and_then(|url| data_url(&url))
            .map(Part::Image),
        // OpenCode's file part; one that is not an image stays a block.
        Some("file") => text("uri").and_then(|uri| data_url(&uri)).map(Part::Image),
        Some("image") => {
            let source = block.get("source").unwrap_or(block);
            let declared = source
                .get("mimeType")
                .or_else(|| source.get("media_type"))
                .and_then(Value::as_str);
            source
                .get("data")
                .and_then(Value::as_str)
                .and_then(|data| image(declared, data))
                .map(Part::Image)
        }
        _ => block.as_str().map(|text| Part::Text(text.to_owned())),
    };
    part.unwrap_or_else(|| Part::Text(block.to_string()))
}

fn is_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some(
            "text"
                | "input_text"
                | "inputText"
                | "output_text"
                | "image"
                | "input_image"
                | "inputImage"
                | "audio"
                | "resource"
                | "resource_link"
        )
    )
}

pub(crate) fn data_url(url: &str) -> Option<Attachment> {
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    image(Some(mime), data)
}

/// An inline image, typed by its bytes: providers mislabel them (Codex
/// computer-use screenshots arrive as `image/png` holding JPEG bytes).
pub(crate) fn image(declared: Option<&str>, data: &str) -> Option<Attachment> {
    let media_type = sniff(data).or_else(|| declared.filter(|mime| mime.starts_with("image/")))?;
    (!data.is_empty()).then(|| Attachment {
        media_type: media_type.to_owned(),
        data_base64: data.to_owned(),
        source_path: None,
    })
}

fn sniff(data: &str) -> Option<&'static str> {
    let head = data.get(..16)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(head)
        .ok()?;
    match bytes.as_slice() {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}
