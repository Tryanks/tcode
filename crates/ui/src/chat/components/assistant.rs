use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClickEvent, Div, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::markdown::parse::ParsedDocument;
use crate::markdown::{MarkdownState, MarkdownView};

use super::super::model::{MdSync, md_sync};

/// Markdown state mirrored from a timeline entry.
pub(crate) struct MdState {
    pub(crate) state: Entity<MarkdownState>,
    /// The text currently mirrored into `state`.
    pub(crate) synced: Arc<str>,
}

impl MdState {
    pub(crate) fn new(text: &str, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(text, cx)),
            synced: Arc::from(text),
        }
    }

    pub(crate) fn from_parsed(text: &str, parsed: ParsedDocument, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::from_parsed(text, parsed, cx)),
            synced: Arc::from(text),
        }
    }

    pub(crate) fn sync(&mut self, text: String, cx: &mut App) {
        match md_sync(&self.synced, &text) {
            MdSync::Noop => {}
            MdSync::Push(delta) => {
                self.state
                    .update(cx, |state, cx| state.push_str(&delta, cx));
                self.synced = Arc::from(text);
            }
            MdSync::Reset => {
                self.state.update(cx, |state, cx| state.set_text(&text, cx));
                self.synced = Arc::from(text);
            }
        }
    }
}

pub(crate) struct AssistantData<'a> {
    pub(crate) id: &'a str,
    pub(crate) text: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) pinned: bool,
    pub(crate) show_actions: bool,
    pub(crate) copied: bool,
}

/// An assistant message: rendered markdown plus a hover-revealed Copy action.
pub(crate) fn assistant(
    data: AssistantData<'_>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let AssistantData {
        id,
        text,
        cwd,
        markdown,
        pinned,
        show_actions,
        copied,
    } = data;
    let content = markdown.map_or_else(
        || div().child(text.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .compact_headings(true)
                .selectable(true)
                .base_dir(cwd)
                .into_any_element()
        },
    );
    let message = v_flex().w_full().items_start().gap(px(2.)).child(
        div()
            .w_full()
            .text_size(px(15.))
            .line_height(px(26.))
            .child(content),
    );

    if !show_actions {
        return message.into_any_element();
    }

    let group_key = SharedString::from(format!("assistant-{id}"));
    let actions = h_flex().gap(px(2.)).items_center().child(copy_button(
        &format!("assistant:{id}"),
        copied,
        on_copy,
        cx,
    ));
    message
        .group(group_key.clone())
        .child(
            reserve_action_row(actions, group_key, pinned).with_animation(
                SharedString::from(format!("assistant-actions-{id}")),
                Animation::new(Duration::from_millis(400)),
                |element, delta| element.opacity(delta),
            ),
        )
        .into_any_element()
}

/// Reserve action-row height so hover visibility never shifts the timeline.
pub(crate) fn reserve_action_row(actions: Div, group_key: SharedString, pinned: bool) -> Div {
    div()
        .h(px(crate::material::CHAT_ACTION_ROW_HEIGHT))
        .flex()
        .items_center()
        .when(!pinned, |this| {
            this.invisible()
                .group_hover(group_key, |style| style.visible())
        })
        .child(actions)
}

/// The geometry every message action shares: a 24px icon-only square with a
/// 6px radius, quiet until hovered. The label lives in the tooltip, so a row of
/// actions reads as icons, not as a row of little labelled controls.
///
/// The icon carries its own muted color because a Ghost button paints
/// `foreground` over any color set on the button itself.
pub(crate) fn action_button(
    id: impl Into<SharedString>,
    icon: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Button {
    Button::new(id.into())
        .ghost()
        .small()
        .rounded(crate::material::radius_chip())
        .icon(Icon::new(icon).text_color(cx.theme().muted_foreground))
        .tooltip(label.into())
}

pub(crate) fn copy_button(
    key: &str,
    copied: bool,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    action_button(
        SharedString::from(format!("copy-{key}")),
        if copied {
            IconName::Check
        } else {
            IconName::Copy
        },
        if copied {
            crate::tr!("chat.copied").into_owned()
        } else {
            crate::tr!("chat.copy").into_owned()
        },
        cx,
    )
    .on_click(on_copy)
}

#[cfg(test)]
mod tests {
    use super::MdState;
    use crate::chat::model::{MdSync, md_sync, plain_text_as_markdown};
    use crate::markdown::MarkdownState;
    use gpui::{AppContext as _, Entity, TestAppContext};

    #[gpui::test]
    fn copy_puts_raw_text_on_the_clipboard(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let raw = "Done — **bold**, `code` and:\n\n- one\n- two\n";

        let payload = gpui::ClipboardItem::new_string(raw.to_string());
        assert_eq!(payload.text().as_deref(), Some(raw));

        // The rendered document is a different (lossy) string; copying it would
        // be the bug this test exists to prevent.
        let md = cx.update(|cx| MdState::new(raw, cx));
        let rendered = rendered(&md.state, cx);
        assert_ne!(rendered, raw);
        assert!(!rendered.contains("**"));
    }

    fn rendered(state: &Entity<MarkdownState>, cx: &mut TestAppContext) -> String {
        state.read_with(cx, |state, _| state.rendered_text())
    }

    #[gpui::test]
    fn plain_text_markdown_escapes_inline_and_block_syntax(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let cases = [
            "*not italic* _nor this_ **nor bold**",
            "# not a heading",
            "- not a list",
            "1. not a list",
            "`not code`",
            "before\n``` not a fence\n``` still not a fence",
            "[not a link](https://example.com)",
            "你好，世界！（测试）",
            "&#32; literal entity",
            "<div>not html</div>",
        ];

        for input in cases {
            let markdown = plain_text_as_markdown(input);
            let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&markdown, cx)));
            assert_eq!(
                rendered(&state, cx),
                format!("{input}\n"),
                "input: {input:?}"
            );
        }
    }

    #[gpui::test]
    fn plain_text_markdown_preserves_lines_and_indentation(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let cases = [
            ("line one\nline two", "line one\nline two\n"),
            ("para one\n\npara two", "para one\npara two\n"),
            (
                "    let x = 1;\n        nested();",
                "    let x = 1;\n        nested();\n",
            ),
            ("\tindented with a tab", "\tindented with a tab\n"),
        ];

        for (input, expected) in cases {
            let markdown = plain_text_as_markdown(input);
            let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&markdown, cx)));
            assert_eq!(rendered(&state, cx), expected, "input: {input:?}");
        }
    }

    #[gpui::test]
    fn markdown_state_set_text_renders_the_new_text(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("Old **value**", cx)));

        state.update(cx, |state, cx| state.set_text("New **value**", cx));

        assert_eq!(rendered(&state, cx), "New value\n");
    }

    #[gpui::test]
    fn markdown_state_push_str_appends_after_prior_updates(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("Seed", cx)));

        state.update(cx, |state, cx| {
            state.push_str(" one", cx);
            state.set_text("Reset", cx);
            state.push_str(" two", cx);
        });

        assert_eq!(rendered(&state, cx), "Reset two\n");
    }

    #[gpui::test]
    fn md_state_synced_mirror_tracks_push_and_reset_paths(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let mut md = cx.update(|cx| MdState::new("Seed", cx));

        assert_eq!(
            md_sync(&md.synced, "Seed tail"),
            MdSync::Push(" tail".into())
        );
        cx.update(|cx| md.sync("Seed tail".into(), cx));
        assert_eq!(md.synced.as_ref(), "Seed tail");
        assert_eq!(rendered(&md.state, cx), "Seed tail\n");

        assert_eq!(md_sync(&md.synced, "Replacement"), MdSync::Reset);
        cx.update(|cx| md.sync("Replacement".into(), cx));
        assert_eq!(md.synced.as_ref(), "Replacement");
        assert_eq!(rendered(&md.state, cx), "Replacement\n");
    }

    #[gpui::test]
    fn markdown_state_rendered_text_includes_formatting_content(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("Some **bold** text", cx)));

        assert_eq!(
            state.read_with(cx, |state, _| state.rendered_text()),
            "Some bold text\n"
        );
    }
}
