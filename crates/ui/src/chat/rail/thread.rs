//! What there is to know about the thread itself, shown while there is
//! something: the thread it was forked from.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::h_flex;

use super::RailView;
use crate::icon::IconName;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

fn source(rail: &RailView, cx: &App) -> Option<(String, Option<String>)> {
    rail.workspace_store
        .read(cx)
        .fork_source(rail.session_id.as_deref()?)
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, cx: &App) -> bool {
    source(rail, cx).is_some()
}

/// What the card shows below its header: the thread this one was forked
/// from, which its row opens while that thread exists.
pub(super) fn body(
    rail: &RailView,
    _window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<Option<(String, Option<String>)>>>>("rail-fork-source");
    if let Some(source) = source(rail, cx) {
        last.replace(Some(source));
    }
    let (title, thread_id) = last.borrow().clone()?;
    let radius = crate::material::radius_card(cx);
    let opens = thread_id.is_some();
    let label = crate::tr!("fork.forked_from", title = title.as_str()).into_owned();
    let row = crate::material::accessible_clickable(
        h_flex(),
        SharedString::from("rail-fork-origin"),
        Role::Button,
        label.clone(),
        cx,
    )
    .debug_selector(|| "fork-origin".into())
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .gap_1p5()
    .items_center()
    .rounded(radius)
    .text_body(false, cx)
    .when(!opens, |row| row.text_color(cx.theme().muted_foreground))
    .when(opens, |row| {
        row.cursor_pointer()
            .child(crate::material::row_hover(radius, cx))
    })
    .child(
        div()
            .flex_none()
            .text_color(cx.theme().muted_foreground)
            .child(crate::icon::Icon::new(IconName::GitBranch).xsmall()),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(crate::widgets::fade_text(SharedString::from(label))),
    )
    .when_some(thread_id, |row, thread_id| {
        row.on_click(cx.listener(move |rail, _, _, cx| {
            let thread_id = thread_id.clone();
            rail.workspace_store
                .update(cx, |store, cx| store.show_thread(thread_id, cx));
        }))
    });
    Some(row.into_any_element())
}
