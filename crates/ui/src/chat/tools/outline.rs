//! A user interface as an outline: each element's role and name, indented
//! under its parent, its reference set off faintly — folded to a count of
//! its elements until asked for.

use gpui::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use super::window::section;
use super::{Call, ToolCtx};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// Elements an opened outline lays out at most.
const MAX_ROWS: usize = 400;

/// One element of an outline.
#[derive(Debug, Default, PartialEq)]
pub(in crate::chat) struct Node {
    pub(in crate::chat) depth: usize,
    pub(in crate::chat) role: String,
    pub(in crate::chat) name: Option<String>,
    pub(in crate::chat) value: Option<String>,
    /// What else its line says of it: its actions, `disabled`, `focused`.
    pub(in crate::chat) detail: Option<String>,
    /// The handle the tool knows it by: an `@e` ref, a CSS selector.
    pub(in crate::chat) reference: Option<String>,
    /// How a diff marks it: `+` added, `~` changed, `-` removed.
    pub(in crate::chat) mark: Option<char>,
}

/// A line of tcode_computer_use's outline, as its `render_line` writes it:
/// `  @e2 button "Save" value="…" [press] focused`, two spaces a level; a
/// search result leads with `score=N`, a diff entry with its mark.
pub(in crate::chat) fn parse_line(line: &str) -> Option<Node> {
    let trimmed = line.trim_start_matches(' ');
    let depth = (line.len() - trimmed.len()) / 2;
    let mut rest = trimmed;
    let mut node = Node {
        depth,
        ..Node::default()
    };
    if let Some(scored) = rest.strip_prefix("score=") {
        rest = scored.split_once(' ')?.1;
    }
    if let Some((mark, marked)) = rest
        .split_once(' ')
        .filter(|(mark, _)| matches!(*mark, "+" | "~" | "-"))
    {
        node.mark = mark.chars().next();
        rest = marked;
        // A changed element names its old ref first: `~ @e8 -> @e8 button`.
        if let Some((_, new)) = rest.split_once(" -> ") {
            rest = new;
        }
    }
    let (reference, after) = rest.split_once(' ')?;
    let digits = reference.strip_prefix("@e")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    node.reference = Some(reference.to_owned());
    let (role, mut after) = after.split_once(' ').unwrap_or((after, ""));
    if role.is_empty() {
        return None;
    }
    node.role = role.to_owned();
    if let Some((name, tail)) = quoted(after) {
        node.name = Some(name);
        after = tail.trim_start();
    }
    if let Some((value, tail)) = after.strip_prefix("value=").and_then(quoted) {
        node.value = Some(value);
        after = tail.trim_start();
    }
    node.detail = Some(after.trim().to_owned()).filter(|detail| !detail.is_empty());
    Some(node)
}

/// A quoted string at the start of `text`, unescaped, and what follows it.
fn quoted(text: &str) -> Option<(String, &str)> {
    let body = text.strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = body.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Some((value, &body[index + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                other => value.push(other),
            },
            other => value.push(other),
        }
    }
    None
}

/// The harness's notes after an observation, whose candidate lines
/// (`- @e1 window "…"`) are shaped like a diff's removals but list targets.
const NOTES_OPEN: &str = "<computer_use_harness>";
const NOTES_CLOSE: &str = "</computer_use_harness>";

/// The elements a tool's text lists, outside the harness's notes.
pub(in crate::chat) fn nodes(text: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut in_notes = false;
    for line in text.lines() {
        match line.trim() {
            NOTES_OPEN => in_notes = true,
            NOTES_CLOSE => in_notes = false,
            _ if !in_notes => nodes.extend(parse_line(line)),
            _ => {}
        }
    }
    nodes
}

/// The outline as a section of a body: shut, `count` — how many elements
/// the tool says the interface holds; open, the elements in a capped
/// scroll, under `caption`.
pub(in crate::chat) fn section_of(
    call: &Call<'_>,
    nodes: &[Node],
    caption: &str,
    count: String,
    ctx: &mut ToolCtx<'_, '_>,
) -> Option<AnyElement> {
    if nodes.is_empty() {
        return None;
    }
    let key = format!("tool-tree-{}", call.id());
    let open = ctx.is_open(&key);
    let on_click = ctx.toggle(call.entry, key.clone());
    let label = count;
    let cx = ctx.app();
    let toggle = format!("{key}-toggle");
    let control = h_flex().child(
        div()
            .debug_selector({
                let toggle = toggle.clone();
                move || toggle.clone()
            })
            .child(super::super::components::changed_files::quiet_control(
                SharedString::from(toggle),
                label.into(),
                on_click,
                cx,
            )),
    );
    let rows = open.then(|| {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let shown = &nodes[..nodes.len().min(MAX_ROWS)];
        let rows = shown.iter().enumerate().map(|(index, node)| {
            let selector = format!("{key}-{index}");
            let mark_color = match node.mark {
                Some('+') => theme.success,
                Some('-') => theme.danger,
                _ => theme.warning,
            };
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .pl(px(node.depth as f32 * 12.))
                .debug_selector(move || selector.clone())
                .when_some(node.mark, |row, mark| {
                    row.child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_color(mark_color)
                            .child(mark.to_string()),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(theme.muted_foreground)
                        .child(node.role.clone()),
                )
                .when_some(node.name.clone(), |row, name| {
                    row.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(theme.foreground.opacity(0.9))
                            .child(crate::widgets::fade_text(name)),
                    )
                })
                .when_some(node.value.clone(), |row, value| {
                    row.child(
                        div()
                            .min_w_0()
                            .max_w(px(220.))
                            .overflow_hidden()
                            .text_color(theme.muted_foreground)
                            .child(crate::widgets::fade_text(format!("= {value}"))),
                    )
                })
                .when_some(node.detail.clone(), |row, detail| {
                    row.child(
                        div()
                            .flex_none()
                            .max_w(px(200.))
                            .overflow_hidden()
                            .font_family(mono.clone())
                            .text_color(theme.muted_foreground.opacity(0.6))
                            .child(crate::widgets::fade_text(detail)),
                    )
                })
                .when_some(node.reference.clone(), |row, reference| {
                    row.child(
                        div()
                            .flex_none()
                            .max_w(px(180.))
                            .overflow_hidden()
                            .font_family(mono.clone())
                            .text_color(theme.muted_foreground.opacity(0.45))
                            .child(crate::widgets::fade_text(reference)),
                    )
                })
                .into_any_element()
        });
        let beyond = (nodes.len() > MAX_ROWS).then(|| {
            div()
                .text_color(theme.muted_foreground)
                .child(
                    crate::tr!("tools.elements_not_shown", count = nodes.len() - MAX_ROWS)
                        .into_owned(),
                )
                .into_any_element()
        });
        v_flex()
            .id(SharedString::from(key.clone()))
            .debug_selector({
                let key = key.clone();
                move || key.clone()
            })
            .w_full()
            .min_w_0()
            .max_h(px(360.))
            .overflow_y_scroll()
            .text_secondary(false, cx)
            .children(rows)
            .children(beyond)
    });
    Some(
        section(
            caption,
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .child(control)
                .children(rows),
            cx,
        )
        .into_any_element(),
    )
}
