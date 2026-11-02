use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::copy::copy_button;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, PopupMenu};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::markdown::parse::ParsedDocument;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};

use super::super::model::{MdSync, md_sync};

/// Markdown state mirrored from a timeline entry.
pub(crate) struct MdState {
    pub(crate) state: Entity<MarkdownState>,
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
    /// The provider delivered a summary of the message, not its own words.
    pub(crate) summarized: bool,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) pinned: bool,
    pub(crate) compact: bool,
    pub(crate) show_actions: bool,
    pub(crate) copied: bool,
}

/// The message's own context-menu item: the whole text, as the hover Copy
/// button copies it. Offered inside the Markdown (below the view's own
/// items) and on the margin around it.
pub(crate) fn copy_message_items(text: Arc<str>) -> MenuExtension {
    Rc::new(move |menu: PopupMenu, _: &mut Window, _: &mut App| {
        menu.menu(
            crate::tr!("chat.copy_message").into_owned(),
            Box::new(CopyText(text.to_string())),
        )
    })
}

/// Between a message's text, its summary note and its action row.
const MESSAGE_GAP: f32 = 2.;

/// An assistant message: rendered markdown plus a hover-revealed Copy action.
pub(crate) fn assistant(
    data: AssistantData<'_>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let AssistantData {
        id,
        text,
        summarized,
        cwd,
        markdown,
        pinned,
        compact,
        show_actions,
        copied,
    } = data;
    let menu_items = copy_message_items(Arc::from(text));
    let content = markdown.map_or_else(
        || div().child(text.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .compact_headings(true)
                .selectable(true)
                .base_dir(cwd)
                .menu_extension(menu_items.clone())
                .into_any_element()
        },
    );
    let message = v_flex()
        .w_full()
        .items_start()
        .gap(px(MESSAGE_GAP))
        .child(
            div()
                .w_full()
                .text_token(&cx.theme().tokens.typography.md)
                .child(content),
        )
        .when(summarized, |message| {
            message.child(
                div()
                    .text_token(&cx.theme().tokens.typography.xs)
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("chat.message_summarized").into_owned()),
            )
        });
    let margin_menu = move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
        menu_items(menu, window, cx)
    };

    // The action row folds in when the turn settles on this message, and out
    // when it moves on.
    let group_key = SharedString::from(format!("assistant-{id}"));
    let fold_id = SharedString::from(format!("assistant-actions-{id}"));
    let key = format!("assistant:{id}");
    let group = group_key.clone();
    v_flex()
        .w_full()
        .items_start()
        .group(group_key)
        .child(message)
        .child(crate::motion::Sampled::new(
            move |window, cx| match crate::motion::shown(
                fold_id,
                show_actions.then_some(()),
                window,
                cx,
            ) {
                Some(fold) => {
                    let actions = h_flex()
                        .gap(px(2.))
                        .items_center()
                        .child(copy_button(&key, copied, compact, on_copy, cx));
                    fold.fold(
                        div()
                            .pt(px(MESSAGE_GAP))
                            .child(reserve_action_row(actions, group, pinned, compact)),
                    )
                }
                None => div().into_any_element(),
            },
        ))
        .context_menu(margin_menu)
        .into_any_element()
}

/// Reserve action-row height so hover visibility never shifts the timeline.
pub(crate) fn reserve_action_row(
    actions: Div,
    group_key: SharedString,
    pinned: bool,
    compact: bool,
) -> Div {
    div()
        .h(px(if compact {
            crate::material::TOUCH_TARGET
        } else {
            crate::material::CHAT_ACTION_ROW_HEIGHT
        }))
        .flex()
        .items_center()
        .when(!pinned && !compact, |this| {
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
#[cfg(test)]
mod tests {
    use super::MdState;
    use crate::chat::model::plain_text_as_markdown;
    use crate::markdown::{FADE, MarkdownState, MarkdownView};
    use crate::test_frames::{draw, draw_settled};
    use crate::theme::ActiveTheme as _;
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px,
    };

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
            r"Literal \(x_i\), \[y^2\], $z$ and $$w$$",
        ];

        let cases = cases
            .into_iter()
            .map(|input| (input, format!("{input}\n")))
            .chain([
                ("line one\nline two", "line one\nline two\n".into()),
                ("para one\n\npara two", "para one\npara two\n".into()),
                (
                    "    let x = 1;\n        nested();",
                    "    let x = 1;\n        nested();\n".into(),
                ),
                ("\tindented with a tab", "\tindented with a tab\n".into()),
            ]);
        for (input, expected) in cases {
            let markdown = plain_text_as_markdown(input);
            let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&markdown, cx)));
            assert_eq!(rendered(&state, cx), expected, "input: {input:?}");
        }
    }

    #[gpui::test]
    fn markdown_mirror_keeps_rendered_text_coherent_across_append_rewrite_shrink_and_clear(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let mut md = cx.update(|cx| MdState::new("", cx));
        for (text, expected) in [
            ("", ""),
            ("Seed", "Seed\n"),
            ("Seed", "Seed\n"),
            ("Seed tail 文", "Seed tail 文\n"),
            ("Replacement", "Replacement\n"),
            ("Replace", "Replace\n"),
            ("new", "new\n"),
            ("new **value**", "new value\n"),
            ("", ""),
        ] {
            cx.update(|cx| md.sync(text.into(), cx));
            assert_eq!(md.synced.as_ref(), text);
            assert_eq!(rendered(&md.state, cx), expected);
        }
    }

    /// A message as the timeline draws it: its state's view in a column.
    struct Message(Entity<MarkdownState>);

    impl Render for Message {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(640.))
                .child(MarkdownView::new(&self.0).selectable(true))
        }
    }

    fn open<'a>(md: &MdState, cx: &'a mut TestAppContext) -> &'a mut VisualTestContext {
        let state = md.state.clone();
        let (_, cx) = cx.add_window_view(|_, _| Message(state));
        draw_settled(cx);
        cx
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
    }

    /// The alpha of each underline and strikethrough painted last frame on
    /// a one-line message, left to right.
    fn decoration_alphas(cx: &mut VisualTestContext) -> Vec<f32> {
        let mut lines = cx.update(|window, _| window.painted_underlines());
        lines.sort_by(|a, b| a.bounds.origin.x.0.total_cmp(&b.bounds.origin.x.0));
        lines.iter().map(|line| line.color.a).collect()
    }

    /// The alpha of each inline code background painted last frame.
    fn chip_alphas(cx: &mut VisualTestContext) -> Vec<f32> {
        cx.update(|window, cx| {
            let muted = cx.theme().tokens.colors.muted;
            window
                .painted_quads()
                .iter()
                .filter_map(|quad| quad.background.as_solid())
                .filter(|color| color.h == muted.h && color.s == muted.s && color.l == muted.l)
                .map(|color| color.a)
                .collect()
        })
    }

    fn frames_requested(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| window.simulate_next_frame(cx))
    }

    /// While a message streams, the text a delta adds fades in -- and an
    /// inline code chip with it -- while the text already shown stays as it
    /// was; once the fade has played the window asks for no more frames.
    #[gpui::test]
    fn streamed_text_fades_in_where_it_is_new(cx: &mut TestAppContext) {
        init(cx);
        let mut md = cx.update(|cx| MdState::new("Read [the docs](https://a.example)", cx));
        let cx = open(&md, cx);
        let link = decoration_alphas(cx);
        assert_eq!(link.len(), 1, "the link's underline");
        let chip = cx.update(|_, cx| cx.theme().tokens.colors.muted.a);

        cx.update(|_, cx| {
            md.sync(
                "Read [the docs](https://a.example), then run `cargo test`".into(),
                cx,
            )
        });
        draw(cx);
        assert!(frames_requested(cx) > 0, "a fade asks for frames");
        cx.executor().advance_clock(FADE / 2);
        draw(cx);
        assert_eq!(decoration_alphas(cx), link, "the shown link faded again");
        let fading = chip_alphas(cx);
        assert!(
            !fading.is_empty() && fading.iter().all(|alpha| *alpha > 0. && *alpha < chip),
            "the new chip fades in with its text: {fading:?} of {chip}"
        );

        cx.executor().advance_clock(FADE);
        draw_settled(cx);
        assert!(chip_alphas(cx).iter().all(|alpha| *alpha == chip));
        draw(cx);
        assert_eq!(frames_requested(cx), 0, "a played fade asks for frames");
    }

    /// Markup a delta completes is painted anew from where its rendered text
    /// changed: `~~bo` becoming `~~bold~~` fades the struck word in, and
    /// leaves the text before it as it was.
    #[gpui::test]
    fn completed_markup_fades_only_the_glyphs_it_changed(cx: &mut TestAppContext) {
        init(cx);
        let mut md = cx.update(|cx| MdState::new("[seen](https://a.example) ~~bo", cx));
        let cx = open(&md, cx);
        let link = decoration_alphas(cx);
        assert_eq!(link.len(), 1, "the link's underline");

        cx.update(|_, cx| md.sync("[seen](https://a.example) ~~bold~~".into(), cx));
        draw(cx);
        cx.executor().advance_clock(FADE / 2);
        draw(cx);
        let painted = decoration_alphas(cx);
        assert_eq!(painted.len(), 2, "the underline and the strikethrough");
        assert_eq!(painted[0], link[0], "the shown link faded again");
        assert!(
            painted[1] > 0. && painted[1] < link[0],
            "the struck word fades in: {painted:?}"
        );
    }

    /// Text the reader did not watch arrive is shown at once, even while
    /// earlier text still fades: what a document holds when it is first
    /// drawn, a rewrite, a burst larger than a stream delivers, and anything
    /// under reduced motion.
    #[gpui::test]
    fn text_not_watched_arriving_appears_at_once(cx: &mut TestAppContext) {
        init(cx);
        let link = "[docs](https://a.example)";
        let burst = "A paragraph that arrived whole. ".repeat(20);
        let cases = [
            ("synced before it was drawn", format!("Read on: {link}")),
            ("rewritten", format!("Final {link}")),
            ("a burst", format!("Read on: {burst}{link}")),
            ("reduced motion", format!("Read on: {link}")),
        ];
        for (case, text) in cases {
            let mut md = cx.update(|cx| MdState::new("Read", cx));
            let drawn_first = case != "synced before it was drawn";
            if !drawn_first {
                cx.update(|cx| md.sync(text.clone(), cx));
            }
            let state = md.state.clone();
            let (_, window) = cx.add_window_view(|_, _| Message(state));
            draw(window);
            if drawn_first {
                draw_settled(window);
                window.update(|_, cx| md.sync("Read on:".into(), cx));
                draw(window);
                assert!(frames_requested(window) > 0, "{case}: the delta fades");
            }
            let reduce = case == "reduced motion";
            if reduce {
                window.update(|_, cx| cx.set_reduce_motion(true));
                draw(window);
                frames_requested(window);
            }
            window.update(|_, cx| md.sync(text.clone(), cx));
            draw(window);
            let rest = window.update(|_, cx| cx.theme().link.a);
            assert_eq!(decoration_alphas(window), [rest], "{case}");
            if reduce {
                assert_eq!(frames_requested(window), 0, "{case}");
            }
            window.update(|_, cx| cx.set_reduce_motion(false));
        }
    }
}
