//! The window selection's touch surfaces, adapted from gpui-component's
//! Apache-2.0 `touch_selection/window_overlay.rs`.

use gpui::{
    App, ClipboardItem, Context, IntoElement, ParentElement as _, Render, Subscription, Window, div,
};
use gpui_base::TextSelection;

use super::{EditMenuItem, SelectAllTouched, TouchSelectionOverlay};

/// Draws the edit menu of the window text selection — the one a
/// [`crate::markdown::MarkdownView`] takes part in.
/// [`crate::overlay::OverlayHost`] mounts one per window, after the content,
/// so the menu floats above whatever was selected.
///
/// Read-only text offers Copy and Select All.
pub(crate) struct WindowTouchSelectionOverlay {
    _subscription: Subscription,
}

impl WindowTouchSelectionOverlay {
    pub(crate) fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        let subscription = TextSelection::observe_touch_selection(window, cx, move |cx| {
            let _ = this.update(cx, |_, cx| cx.notify());
        });
        Self {
            _subscription: subscription,
        }
    }

    fn copy(window: &mut Window, cx: &mut App) {
        let text = TextSelection::selected_text(window, cx);
        let text = text.trim();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
        TextSelection::close_edit_menu(window, cx);
    }

    /// The participant answers, not the engine's own select-all: that one
    /// spans the participant's laid-out runs, and a Markdown message
    /// virtualizes its blocks, so it would stop at the viewport.
    fn select_all(window: &mut Window, cx: &mut App) {
        window.dispatch_action(Box::new(SelectAllTouched), cx);
    }
}

impl Render for WindowTouchSelectionOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if TextSelection::touch_selection(window, cx).is_none() {
            return div();
        }
        // The handles are painted by the text that owns them, where they
        // are covered by whatever covers the text; only the menu floats.
        let overlay = TouchSelectionOverlay::new("window-touch-selection", |window, cx| {
            TextSelection::touch_selection(window, cx)
        })
        .items([
            EditMenuItem::new(
                "copy",
                crate::tr!("edit_menu.copy").into_owned(),
                Self::copy,
            ),
            EditMenuItem::new(
                "select-all",
                crate::tr!("edit_menu.select_all").into_owned(),
                Self::select_all,
            ),
        ])
        .on_paint(|bounds, window, cx| TextSelection::register_touch_ui(bounds, window, cx));
        div().children(overlay.into_elements(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, LongPressEvent, Modifiers,
        ParentElement as _, Render, ScrollDelta, ScrollWheelEvent, Styled as _, TestAppContext,
        TouchPhase, VisualTestContext, Window, div, point, px, size,
    };
    use gpui_base::TextSelection;

    use crate::{
        markdown::{MarkdownState, MarkdownView},
        overlay::OverlayHost,
        widgets::input::{Textarea, TextareaState},
    };

    struct Body {
        markdown: Entity<MarkdownState>,
    }

    impl Render for Body {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    fn tap(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is shown"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        draw(cx);
    }

    fn long_press(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>) {
        for phase in [TouchPhase::Started, TouchPhase::Ended] {
            cx.simulate_event(LongPressEvent {
                phase,
                start_position: position,
                position,
            });
            draw(cx);
        }
    }

    fn window_menu_open(cx: &mut VisualTestContext) -> Option<bool> {
        cx.update(|window, cx| {
            TextSelection::touch_selection(window, cx).map(|snapshot| snapshot.is_menu_open())
        })
    }

    /// A message above a composer, under the shell's root wheel-easing wrapper.
    struct Page {
        markdown: Entity<MarkdownState>,
        textarea: Entity<TextareaState>,
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            crate::wheel_easing::root(
                div()
                    .size_full()
                    .child(
                        div()
                            .h(px(60.))
                            .child(MarkdownView::new(&self.markdown).selectable(true)),
                    )
                    .child(div().h(px(100.)))
                    .child(div().h(px(60.)).child(Textarea::new(&self.textarea))),
            )
        }
    }

    #[gpui::test]
    fn one_touch_selection_at_a_time_and_the_menu_steps_aside_for_a_pan(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let textarea = std::rc::Rc::new(std::cell::OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let textarea = textarea.clone();
            move |window, cx| {
                let state = cx.new(|cx| TextareaState::new(window, cx).default_value("draft text"));
                textarea.set(state.clone()).ok().unwrap();
                let markdown = cx.new(|cx| MarkdownState::new("quick select value", cx));
                let page = cx.new(|_| Page {
                    markdown,
                    textarea: state,
                });
                OverlayHost::new(page, window, cx)
            }
        });
        let textarea = textarea.get().unwrap().clone();
        cx.simulate_resize(size(px(320.), px(400.)));
        draw(cx);
        draw(cx);

        long_press(cx, point(px(12.), px(8.)));
        assert_eq!(window_menu_open(cx), Some(true));

        // A finger scrolling the page moves the menu out of the way until it
        // lifts; the selection and its handles stay.
        let pan = point(px(200.), px(100.));
        for (phase, open) in [
            (TouchPhase::Started, false),
            (TouchPhase::Moved, false),
            (TouchPhase::Ended, true),
        ] {
            cx.simulate_event(ScrollWheelEvent {
                position: pan,
                delta: ScrollDelta::Pixels(point(px(0.), px(-20.))),
                modifiers: Modifiers::default(),
                touch_phase: phase,
            });
            draw(cx);
            assert_eq!(window_menu_open(cx), Some(open), "{phase:?}");
        }
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());

        // A long press in the composer takes the selection over: the message
        // loses its selection, as on a tap, and the field's menu shows.
        let draft = textarea.read_with(cx, |state, _| state.range_to_bounds(&(0..1)).unwrap());
        long_press(cx, draft.center());
        assert_eq!(window_menu_open(cx), None);
        assert_eq!(
            textarea.read_with(cx, |state, _| state.selected_text().to_string()),
            "draft"
        );
        assert!(cx.debug_bounds("edit-menu-cut").is_some());

        // And back: the message's long press focuses the message, and the
        // field's controls go with its focus.
        long_press(cx, point(px(12.), px(8.)));
        assert_eq!(window_menu_open(cx), Some(true));
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());
    }

    #[gpui::test]
    fn long_press_on_a_message_offers_copy_and_select_all_past_the_viewport(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        let text = (0..40)
            .map(|ix| format!("Paragraph {ix} of the message."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let markdown = std::rc::Rc::new(std::cell::OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let markdown = markdown.clone();
            move |window, cx| {
                let state = cx.new(|cx| MarkdownState::new(&text, cx));
                markdown.set(state.clone()).ok().unwrap();
                let body = cx.new(|_| Body { markdown: state });
                OverlayHost::new(body, window, cx)
            }
        });
        let markdown = markdown.get().unwrap().clone();
        // A window far shorter than the message: most of its blocks are
        // outside the viewport, and the virtualized list never paints them.
        cx.simulate_resize(size(px(320.), px(120.)));
        draw(cx);
        draw(cx);
        assert!(cx.debug_bounds("markdown-block-0").is_some());
        assert!(cx.debug_bounds("markdown-block-39").is_none());

        let position = point(px(12.), px(8.));
        for phase in [TouchPhase::Started, TouchPhase::Ended] {
            cx.simulate_event(LongPressEvent {
                phase,
                start_position: position,
                position,
            });
            draw(cx);
        }
        assert_eq!(cx.update(TextSelection::selected_text).trim(), "Paragraph");
        // Read-only text offers Copy and Select All, nothing that edits.
        assert!(cx.debug_bounds("edit-menu-copy").is_some());
        assert!(cx.debug_bounds("edit-menu-select-all").is_some());
        assert!(cx.debug_bounds("edit-menu-cut").is_none());
        assert!(cx.debug_bounds("edit-menu-paste").is_none());
        let menu = cx.debug_bounds("edit-menu").unwrap();
        let window_bounds = gpui::Bounds::new(point(px(0.), px(0.)), size(px(320.), px(120.)));
        assert!(window_bounds.contains(&menu.origin));
        assert!(window_bounds.contains(&menu.bottom_right()));

        // Select All takes the whole message, including the blocks that were
        // never laid out, and keeps the menu.
        tap(cx, "edit-menu-select-all");
        let rendered = markdown.read_with(cx, |markdown, _| {
            markdown.rendered_text().trim().to_string()
        });
        assert!(rendered.ends_with("Paragraph 39 of the message."));
        assert_eq!(cx.update(TextSelection::selected_text).trim(), rendered);
        assert!(cx.debug_bounds("edit-menu-copy").is_some());

        // Copy closes the menu; the selection stays.
        tap(cx, "edit-menu-copy");
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some(rendered)
        );
        assert!(cx.debug_bounds("edit-menu").is_none());
        assert!(
            cx.update(|window, cx| TextSelection::touch_selection(window, cx))
                .is_some()
        );

        // A tap on the text clears the selection, and the menu with it.
        cx.simulate_click(point(px(200.), px(8.)), Modifiers::default());
        draw(cx);
        assert!(
            cx.update(|window, cx| TextSelection::touch_selection(window, cx))
                .is_none()
        );
        assert!(cx.debug_bounds("edit-menu").is_none());
    }
}
