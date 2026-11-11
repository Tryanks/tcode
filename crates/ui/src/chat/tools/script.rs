//! The computer-use script window: a script Codex ran in its computer-use
//! or browser REPL (`cua_repl`, `node_repl`), told by the title the model
//! gave it. The script is not read for what it did: the window shows the
//! screenshots it got back, what it printed, and the script itself a click
//! away.

use std::path::Path;

use gpui::AnyElement;
use serde::Deserialize;

use super::window::{Header, Verb, first_line};
use super::{
    Bound, Call, ToolCtx, ToolView, code_section, copy, output_section_with, thumbnails_titled,
};
use crate::chat::model::one_line;
use crate::icon::IconName;

/// The REPL servers that run a model's script; an imported transcript keeps
/// only the tool's name.
const SERVERS: &[&str] = &["cua_repl", "node_repl"];

#[derive(Deserialize)]
struct Input<'a> {
    #[serde(borrow)]
    code: &'a str,
    #[serde(default, borrow)]
    title: Option<&'a str>,
}

pub(in crate::chat) fn tool_call<'a>(call: &Call<'a>) -> Option<Box<dyn ToolView + 'a>> {
    if call.served(SERVERS, &["js"], true) {
        let input = call.input::<Input<'a>>()?;
        return Some(Box::new(Script {
            code: Some(input.code),
            title: input.title.map(one_line).filter(|title| !title.is_empty()),
        }));
    }
    call.served(SERVERS, &["js_reset"], false).then(|| {
        Box::new(Script {
            code: None,
            title: None,
        }) as Box<dyn ToolView + 'a>
    })
}

/// A script, or the reset of the REPL that runs them.
struct Script<'a> {
    code: Option<&'a str>,
    title: Option<String>,
}

/// A script's result as its harness reports it: some put a header of
/// `Key: value` lines (`Wall time: …`) before an `Output:` line, then what
/// the script printed.
struct Report<'o> {
    wall_ms: Option<u64>,
    output: &'o str,
}

fn report(output: &str) -> Report<'_> {
    let mut report = Report {
        wall_ms: None,
        output,
    };
    let mut offset = 0;
    for (index, line) in output.split_inclusive('\n').enumerate() {
        let text = line.trim_end();
        if text == "Output:" {
            report.output = &output[offset + line.len()..];
            return report;
        }
        // A header is a handful of short lines; anything else is output.
        if index >= 8 || !text.contains(':') && !text.starts_with("Process ") {
            break;
        }
        if let Some(seconds) = text
            .strip_prefix("Wall time: ")
            .and_then(|rest| rest.strip_suffix(" seconds"))
        {
            report.wall_ms = seconds
                .trim()
                .parse::<f64>()
                .ok()
                .map(|seconds| (seconds * 1_000.) as u64);
        }
        offset += line.len();
    }
    Report {
        wall_ms: None,
        output,
    }
}

/// What the script printed: an imported call keeps its harness's report
/// (`Wall time: …` then `Output:`) before it.
fn printed(output: &str) -> &str {
    report(output).output
}

impl ToolView for Script<'_> {
    fn header(&self, call: &Call<'_>, _cwd: &Path) -> Header {
        let verb = if self.code.is_some() {
            Verb("script")
        } else {
            Verb("reset_script")
        };
        let header = Header::new(IconName::Monitor, verb);
        let mut header = match &self.title {
            Some(title) => header.title(title.clone()),
            None => header,
        };
        let output = call.output.unwrap_or_default();
        header.elapsed_ms = report(output).wall_ms;
        header.error(first_line(printed(output)))
    }

    fn body(&self, call: &Call<'_>, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
        let title = self.title.as_deref().unwrap_or(call.name.tool);
        let images = thumbnails_titled(call, title, ctx);
        let printed_anything = !printed(call.output.unwrap_or_default()).trim().is_empty();
        let output = (printed_anything || call.images.is_empty())
            .then(|| output_section_with(call, Bound::OUTPUT, ctx, printed))
            .flatten();
        let code = self.code.map(|code| {
            code_section(
                call,
                code,
                crate::highlight::language_name_for_path("script.js"),
                ctx,
            )
        });
        super::body(images.into_iter().chain(output).chain(code))
    }

    fn copy_items(&self, call: &Call<'_>) -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Some(code) = self.code {
            copy(&mut items, "tools.copy_code", code);
        }
        copy(
            &mut items,
            "chat.copy_output",
            printed(call.output.unwrap_or_default()),
        );
        items
    }
}
