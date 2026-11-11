//! Windows for calls about neither the workspace nor the session: a wait,
//! an image generated.

use std::path::Path;

use agent::ToolFamily;
use gpui::AnyElement;

use super::window::{Argument, Header, Verb};
use super::{Bound, Call, ToolCtx, ToolView, copy, output_section, thumbnails};
use crate::chat::model::{format_elapsed, one_line};
use crate::icon::IconName;

enum Kind<'a> {
    Wait(u64),
    GenerateImage(Option<&'a str>),
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    let kind = match call.family {
        Some(ToolFamily::Wait { duration_ms }) => Kind::Wait(*duration_ms),
        Some(ToolFamily::ImageGeneration { prompt }) => Kind::GenerateImage(prompt.as_deref()),
        _ => return None,
    };
    Some(Box::new(Other { kind }))
}

struct Other<'a> {
    kind: Kind<'a>,
}

impl ToolView for Other<'_> {
    fn header(&self, _call: &Call<'_>, _cwd: &Path) -> Header {
        match self.kind {
            Kind::Wait(waited) => Header::new(IconName::Hourglass, Verb("wait"))
                .argument(Argument::mono(format_elapsed(waited))),
            Kind::GenerateImage(prompt) => Header::new(IconName::Image, Verb("generate_image"))
                .argument(prompt.map(one_line).and_then(Argument::plain)),
        }
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let output = output_section(call, Bound::OUTPUT, ctx);
        let images = thumbnails(call, ctx);
        super::body(output.into_iter().chain(images))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Kind::GenerateImage(prompt) = self.kind {
            copy(&mut items, "tools.copy_prompt", prompt.unwrap_or_default());
            for image in call.images {
                copy(
                    &mut items,
                    "chat.copy_path",
                    image.source_path.as_deref().unwrap_or_default(),
                );
            }
        }
        copy(
            &mut items,
            "chat.copy_output",
            call.output.unwrap_or_default(),
        );
        items
    }
}
