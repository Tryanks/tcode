use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::copy::copy_button;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, PopupMenu};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, ElementId, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::markdown::parse::ParsedDocument;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};
use tcode_core::session::TimelineEntry;

use super::super::model::{MdSync, md_sync};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// Markdown state mirrored from a timeline entry.
pub(crate) struct MdState {
    pub(crate) state: Entity<MarkdownState>,
    pub(crate) synced: Arc<str>,
    /// The entry `synced` was last taken from. An entry is never changed
    /// in place while a weak reference to it is held (the fold's
    /// `Arc::make_mut` moves it to a new allocation instead), so the same
    /// allocation means the same text.
    synced_from: Weak<TimelineEntry>,
    streaming: Cell<bool>,
}

impl MdState {
    pub(crate) fn new(text: &str, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(text, cx)),
            synced: Arc::from(text),
            synced_from: Weak::new(),
            streaming: Cell::new(false),
        }
    }

    pub(crate) fn from_parsed(text: &str, parsed: ParsedDocument, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::from_parsed(text, parsed, cx)),
            synced: Arc::from(text),
            synced_from: Weak::new(),
            streaming: Cell::new(false),
        }
    }

    /// Whether the entry's text is still arriving.
    pub(crate) fn set_streaming(&self, streaming: bool, cx: &mut App) {
        // Updating the state while the chat renders counts as writing it,
        // which draws the message again; only a change is written.
        if self.streaming.replace(streaming) != streaming {
            self.state
                .update(cx, |state, cx| state.set_streaming(streaming, cx));
        }
    }

    /// Whether `entry` is the one the text was last taken from.
    pub(crate) fn synced_from(&self, entry: &Arc<TimelineEntry>) -> bool {
        std::ptr::eq(self.synced_from.as_ptr(), Arc::as_ptr(entry))
    }

    /// Take `entry`'s text, `text`.
    pub(crate) fn sync_from(&mut self, entry: &Arc<TimelineEntry>, text: String, cx: &mut App) {
        self.sync(text, cx);
        self.synced_from = Arc::downgrade(entry);
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
    /// What Copy puts on the clipboard: the text, shared rather than copied
    /// on every render.
    pub(crate) copy_text: Arc<str>,
    /// The provider delivered a summary of the message, not its own words.
    pub(crate) summarized: bool,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) pinned: bool,
    pub(crate) compact: bool,
    pub(crate) actions: MessageActions,
    pub(crate) copied: bool,
    /// The timeline's hover, which reveals the message's actions.
    pub(crate) hover: crate::motion::ListHover,
    /// The chat's motion, which keeps the action row's fold.
    pub(crate) motion: crate::motion::owned::Owned,
}

/// Where a message's actions are drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageActions {
    /// It has none: its turn runs, or a later message of the turn has them.
    None,
    /// In a row under the message, for one its turn goes on after.
    Below,
    /// On the leading edge of its turn's footer, beside the turn's timing,
    /// for the message that ends its turn ([`EndingActions`]).
    InFooter,
}

/// The hover key of message `id`, which reveals its actions wherever they
/// are drawn.
fn hover_key(id: &str) -> SharedString {
    SharedString::from(format!("assistant-{id}"))
}

/// The actions of a message that ends its turn, as its turn's footer draws
/// them: at the message's hover reveal, which the footer takes too, or while
/// the keyboard is on one of them.
#[derive(Clone)]
pub(crate) struct EndingActions {
    pub(crate) id: String,
    pub(crate) copied: bool,
    pub(crate) compact: bool,
}

impl EndingActions {
    pub(crate) fn render(
        &self,
        hover: &crate::motion::ListHover,
        motion: &crate::motion::owned::Owned,
        on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let key = format!("assistant:{}", self.id);
        let selector = format!("copy-{key}");
        let (focus, focused) = crate::motion::focus_ring::focus_reveal(
            &ElementId::from(SharedString::from(selector.clone())),
            true,
            window,
            cx,
        );
        let reveal = if self.compact {
            1.
        } else {
            hover.reveal(&hover_key(&self.id), window, cx).max(focused)
        };
        let copy = copy_button(&key, self.copied, self.compact, on_copy, cx)
            .track_focus(&focus)
            .motion(motion)
            .debug_selector(move || selector);
        // Out by opacity alone, unlike [`reserve_action_row`]: an invisible
        // control is no tab stop, and focus is what brings it in.
        action_row(self.compact)
            .opacity(reveal)
            .child(h_flex().gap(px(2.)).items_center().child(copy))
            .into_any_element()
    }

    /// The footer's `on_hover`, when its actions wait for one.
    pub(crate) fn on_hover(
        &self,
        hover: &crate::motion::ListHover,
    ) -> Option<impl Fn(&bool, &mut Window, &mut App) + 'static> {
        (!self.compact).then(|| hover.on_hover(hover_key(&self.id)))
    }
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
        copy_text,
        summarized,
        cwd,
        markdown,
        pinned,
        compact,
        actions,
        copied,
        hover,
        motion,
    } = data;
    let menu_items = copy_message_items(copy_text);
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
    let hover_key = hover_key(id);
    let fold_id = SharedString::from(format!("assistant-actions-{id}"));
    let key = format!("assistant:{id}");
    let below = actions == MessageActions::Below;
    // Only a message with actions takes the timeline's hover, so a row
    // moving under a still pointer redraws nothing it need not.
    let reveals = !compact
        && match actions {
            MessageActions::None => false,
            MessageActions::Below => !pinned,
            MessageActions::InFooter => true,
        };
    let on_hover = hover.on_hover(hover_key.clone());
    v_flex()
        .id(hover_key.clone())
        .w_full()
        .items_start()
        .when(reveals, |message| message.on_hover(on_hover))
        .child(message)
        .child(crate::motion::Sampled::new(
            move |window, cx| match motion.shown(fold_id, below.then_some(()), SHOW, window, cx) {
                Some(fold) => {
                    let reveal = hover.reveal(&hover_key, window, cx);
                    let actions = h_flex()
                        .gap(px(2.))
                        .items_center()
                        .child(copy_button(&key, copied, compact, on_copy, cx).motion(&motion));
                    fold.fold(
                        div()
                            .pt(px(MESSAGE_GAP))
                            .child(reserve_action_row(actions, reveal, pinned, compact)),
                    )
                }
                None => div().into_any_element(),
            },
        ))
        .context_menu(margin_menu)
        .into_any_element()
}

/// Reserve action-row height so hover visibility never shifts the timeline.
/// Unpinned on a pointer, the row is drawn at `reveal`, the message's hover
/// reveal from the timeline's [`crate::motion::ListHover`], and takes no
/// input while it is out.
pub(crate) fn reserve_action_row(
    actions: impl IntoElement,
    reveal: f32,
    pinned: bool,
    compact: bool,
) -> Div {
    action_row(compact)
        .when(!pinned && !compact, |this| {
            this.opacity(reveal)
                .when(reveal <= 0., |this| this.invisible())
        })
        .child(actions)
}

/// The height every message's action row keeps, its actions centred in it.
fn action_row(compact: bool) -> Div {
    div()
        .h(px(if compact {
            crate::material::TOUCH_TARGET
        } else {
            crate::material::CHAT_ACTION_ROW_HEIGHT
        }))
        .flex()
        .items_center()
}

#[cfg(test)]
mod tests {
    use super::MdState;
    use crate::chat::model::plain_text_as_markdown;
    use crate::markdown::{MarkdownState, MarkdownView};
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

    fn frames_requested(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| window.simulate_next_frame(cx))
    }

    /// A message drawn and streaming, as the timeline holds the live tail of
    /// a running turn.
    fn streaming<'a>(md: &MdState, cx: &'a mut TestAppContext) -> &'a mut VisualTestContext {
        cx.update(|cx| md.set_streaming(true, cx));
        open(md, cx)
    }

    /// One display frame later.
    fn next_frame(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(16));
        draw(cx);
    }

    /// The alpha of each inline code chip painted last frame, in reading
    /// order. A chip is painted as several quads on one line.
    fn chips_in_reading_order(cx: &mut VisualTestContext) -> Vec<f32> {
        cx.update(|window, cx| {
            let muted = cx.theme().tokens.colors.muted;
            let mut quads: Vec<_> = window
                .painted_quads()
                .into_iter()
                .filter_map(|quad| Some((quad.bounds.origin, quad.background.as_solid()?)))
                .filter(|(_, color)| color.h == muted.h && color.s == muted.s && color.l == muted.l)
                .collect();
            quads.sort_by(|(a, _), (b, _)| a.y.0.total_cmp(&b.y.0).then(a.x.0.total_cmp(&b.x.0)));
            let mut chips: Vec<(f32, f32)> = Vec::new();
            for (origin, color) in quads {
                match chips.last_mut() {
                    Some((y, alpha)) if (origin.y.0 - *y).abs() < 1. => *alpha = alpha.max(color.a),
                    _ => chips.push((origin.y.0, color.a)),
                }
            }
            chips.into_iter().map(|(_, alpha)| alpha).collect()
        })
    }

    /// One delta that spans a long paragraph, a CJK one, a list and a last
    /// line lights up strictly in reading order: on every frame no chip is
    /// more opaque than one before it, though the later ones lag the
    /// earlier; the burst is whole within the fade, and then the window asks
    /// for no more frames.
    #[gpui::test]
    fn a_streamed_burst_lights_up_in_reading_order(cx: &mut TestAppContext) {
        init(cx);
        let seed = "A first line.\n\n";
        let mut md = cx.update(|cx| MdState::new(seed, cx));
        let cx = streaming(&md, cx);
        let rest = cx.update(|_, cx| cx.theme().tokens.colors.muted.a);
        let delta = "The first paragraph runs on for a good while before it ends with `alpha`.\n\n\
                     中文段落先写几个字然后才是 `beta` 再继续写完。\n\n\
                     - `gamma` opens the first item\n- `delta` opens the second\n\n\
                     `omega` opens the last line.";
        cx.update(|_, cx| md.sync(format!("{seed}{delta}"), cx));
        draw(cx);
        assert!(frames_requested(cx) > 0, "the delta fades in");
        let mut lagged = false;
        for frame in 0..600 {
            let chips = chips_in_reading_order(cx);
            assert_eq!(chips.len(), 5, "every chip is painted: {chips:?}");
            assert!(
                chips.windows(2).all(|pair| pair[1] <= pair[0] + 1e-4),
                "a later chip is ahead of an earlier one at frame {frame}: {chips:?}"
            );
            lagged |= chips[4] + 0.05 < chips[0];
            if chips.iter().all(|alpha| *alpha == rest) {
                break;
            }
            next_frame(cx);
        }
        assert!(lagged, "the chips lit up together, not in order");
        assert!(
            chips_in_reading_order(cx)
                .iter()
                .all(|alpha| *alpha == rest),
            "the burst comes whole once the fade has played"
        );
        // The words after the last chip are still fading in; the display
        // keeps delivering frames until the window stops asking.
        draw_settled(cx);
        draw(cx);
        assert_eq!(frames_requested(cx), 0, "a played fade asks for frames");
    }

    /// Markup a delta completes never fades the text the reader has seen
    /// again: `~~bo` becoming struck `bold` keeps its glyphs, and the link
    /// before it, at full colour on every frame while the words the delta
    /// adds after them fade in.
    #[gpui::test]
    fn completed_markup_does_not_fade_shown_text_again(cx: &mut TestAppContext) {
        init(cx);
        let mut md = cx.update(|cx| MdState::new("[seen](https://a.example) ~~bo", cx));
        let cx = streaming(&md, cx);
        let link = decoration_alphas(cx);
        assert_eq!(link.len(), 1, "the link's underline");
        let rest = cx.update(|_, cx| cx.theme().tokens.colors.muted.a);

        cx.update(|_, cx| {
            md.sync(
                "[seen](https://a.example) ~~bold~~ and then `more` words".into(),
                cx,
            )
        });
        draw(cx);
        let mut faded = false;
        for _ in 0..40 {
            let painted = decoration_alphas(cx);
            assert_eq!(painted.len(), 2, "the underline and the strikethrough");
            assert_eq!(painted[0], link[0], "the shown link faded again");
            assert_eq!(painted[1], link[0], "the shown struck glyphs faded again");
            faded |= chips_in_reading_order(cx)[0] < rest;
            next_frame(cx);
        }
        assert!(faded, "the new words fade in");
    }

    /// Text the reader did not watch stream in is shown at once, and stops
    /// what still fades: what a document holds when it is first drawn, a
    /// rewrite, an append while the message is not the live tail of a
    /// running turn (a thread opening, a client catching up, history), and
    /// anything under reduced motion.
    #[gpui::test]
    fn text_not_watched_streaming_in_appears_at_once(cx: &mut TestAppContext) {
        init(cx);
        let link = "[docs](https://a.example)";
        let cases = [
            ("synced before it was drawn", format!("Read on: {link}")),
            ("rewritten", format!("Final {link}")),
            ("not streaming", format!("Read on: {link}")),
            ("reduced motion", format!("Read on: {link}")),
        ];
        for (case, text) in cases {
            let mut md = cx.update(|cx| MdState::new("Read", cx));
            cx.update(|cx| md.set_streaming(true, cx));
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
            if case == "not streaming" {
                window.update(|_, cx| md.set_streaming(false, cx));
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
