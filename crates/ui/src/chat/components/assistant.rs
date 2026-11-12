use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use crate::icon::IconName;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::copy::{action_button, copy_button};
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

use super::super::ForkThroughTurn;
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

    /// The message drawn in `scene`'s scenes.
    pub(crate) fn in_scene(self, scene: &crate::motion::Epoch, cx: &mut App) -> Self {
        self.state.update(cx, |state, _| state.set_scene(scene));
        self
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

    /// Land the text's reveal, for a reader who did not watch it arrive.
    pub(crate) fn settle(&self, cx: &mut App) {
        // Only a reveal under way is written, as in `set_streaming`.
        if self.state.read(cx).revealing() {
            self.state.update(cx, |state, cx| state.settle(cx));
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
    pub(crate) image_host: crate::store::ImageHost,
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
    /// The fork its turn offers, when this is the turn's answer.
    pub(crate) fork: Option<TurnFork>,
    pub(crate) copied: bool,
    /// The timeline's hover, which reveals the message's actions.
    pub(crate) hover: crate::motion::ListHover,
    /// The chat's motion, which keeps the action row's fold.
    pub(crate) motion: crate::motion::owned::Owned,
}

/// The fork a finished turn offers: through which turn, whether it can start
/// now, and what asks for it.
#[derive(Clone)]
pub(crate) struct TurnFork {
    pub(crate) turn: usize,
    pub(crate) blocked: bool,
    pub(crate) on_fork: ForkHandler,
}

type ForkHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

impl PartialEq for TurnFork {
    fn eq(&self, other: &Self) -> bool {
        (self.turn, self.blocked) == (other.turn, other.blocked)
    }
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
    pub(crate) fork: Option<TurnFork>,
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
        let fork_selector = format!("fork-{}", self.id);
        let (fork_focus, fork_focused) = crate::motion::focus_ring::focus_reveal(
            &ElementId::from(SharedString::from(fork_selector)),
            true,
            window,
            cx,
        );
        let reveal = if self.compact {
            1.
        } else {
            hover
                .reveal(&hover_key(&self.id), window, cx)
                .max(focused)
                .max(fork_focused)
        };
        let copy = copy_button(&key, self.copied, self.compact, on_copy, cx)
            .touch_in_row(self.compact)
            .track_focus(&focus)
            .motion(motion)
            .debug_selector(move || selector);
        // Out by opacity alone, unlike [`reserve_action_row`]: an invisible
        // control is no tab stop, and focus is what brings it in.
        let fork = fork_action(
            &self.id,
            self.fork.clone(),
            Some(fork_focus),
            self.compact,
            motion,
            window,
            cx,
        );
        action_row(self.compact)
            .opacity(reveal)
            .child(
                h_flex()
                    .gap(ACTION_GAP)
                    .items_center()
                    .child(copy)
                    .children(fork),
            )
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

/// The turn's fork action beside Copy, folding in once it is offered.
fn fork_action(
    id: &str,
    fork: Option<TurnFork>,
    focus: Option<gpui::FocusHandle>,
    compact: bool,
    motion: &crate::motion::owned::Owned,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let selector = format!("fork-{id}");
    motion
        .shown(
            SharedString::from(format!("fork-fold-{id}")),
            fork,
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            let TurnFork {
                turn,
                blocked,
                on_fork,
            } = shown.value.clone();
            let button = action_button(
                SharedString::from(selector.clone()),
                IconName::GitBranch,
                if blocked {
                    crate::tr!("fork.blocked").into_owned()
                } else {
                    crate::tr!("fork.action").into_owned()
                },
                cx,
            )
            .disabled(blocked)
            .touch_in_row(compact)
            .motion(motion)
            .debug_selector(move || selector)
            .on_click(move |_, window, cx| on_fork(turn, window, cx));
            let button = match &focus {
                Some(focus) => button.track_focus(focus),
                None => button,
            };
            shown.fold_across_gap(ACTION_GAP, button.into_any_element())
        })
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

/// An answer's context-menu items: copy it, then its turn's fork.
fn answer_menu_items(text: Arc<str>, fork: Option<TurnFork>) -> MenuExtension {
    let copy = copy_message_items(text);
    Rc::new(move |menu: PopupMenu, window: &mut Window, cx: &mut App| {
        let menu = copy(menu, window, cx);
        match &fork {
            Some(fork) => menu.separator().menu_with_enable(
                crate::tr!("fork.action").into_owned(),
                Box::new(ForkThroughTurn { turn: fork.turn }),
                !fork.blocked,
            ),
            None => menu,
        }
    })
}

/// Between a message's text, its summary note and its action row.
const MESSAGE_GAP: f32 = 2.;

/// Between the actions of a row.
const ACTION_GAP: gpui::Pixels = px(2.);

/// An assistant message: rendered markdown plus a hover-revealed Copy action.
pub(crate) fn assistant(
    data: AssistantData<'_>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let AssistantData {
        image_host,
        id,
        text,
        copy_text,
        summarized,
        cwd,
        markdown,
        pinned,
        compact,
        actions,
        fork,
        copied,
        hover,
        motion,
    } = data;
    let menu_items = answer_menu_items(copy_text, fork.clone());
    let content = markdown.map_or_else(
        || div().child(text.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .compact_headings(true)
                .selectable(true)
                .image_host(image_host)
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
    let fork_id = id.to_owned();
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
                        .gap(ACTION_GAP)
                        .items_center()
                        .child(
                            copy_button(&key, copied, compact, on_copy, cx)
                                .touch_in_row(compact)
                                .motion(&motion)
                                .debug_selector({
                                    let selector = format!("copy-{key}");
                                    move || selector
                                }),
                        )
                        .children(fork_action(
                            &fork_id,
                            fork.clone(),
                            None,
                            compact,
                            &motion,
                            window,
                            cx,
                        ));
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
    use crate::markdown::MarkdownState;
    use gpui::{AppContext as _, Entity, TestAppContext};

    fn rendered(state: &Entity<MarkdownState>, cx: &mut TestAppContext) -> String {
        state.read_with(cx, |state, _| state.rendered_text())
    }

    #[gpui::test]
    fn plain_text_markdown_escapes_inline_and_block_syntax(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
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
    fn a_message_shows_its_latest_text_across_append_rewrite_shrink_and_clear(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::test_client::init);
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
            assert_eq!(rendered(&md.state, cx), expected);
        }
    }
}
