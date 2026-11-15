//! What there is to know about the thread itself, shown while there is
//! something: the thread it was forked from, and the threads its
//! `notify_when` watches follow.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};

use super::RailView;
use crate::icon::IconName;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// A thread the card names: its title, and its id while the index holds it.
type Named = (String, Option<String>);

#[derive(Clone)]
struct Shown {
    source: Option<Named>,
    /// Each watched thread's id, with the thread as the card names it.
    watched: Vec<(String, Named)>,
}

fn shown(rail: &RailView, cx: &App) -> Option<Shown> {
    let session_id = rail.session_id.as_deref()?;
    let store = rail.workspace_store.read(cx);
    let watched: Vec<_> = store
        .watched_threads(session_id)
        .into_iter()
        .map(|id| {
            let named = match store.thread_meta(&id) {
                Some(meta) => (meta.title.clone(), Some(id.clone())),
                None => (id.clone(), None),
            };
            (id, named)
        })
        .collect();
    let source = store.fork_source(session_id);
    (source.is_some() || !watched.is_empty()).then_some(Shown { source, watched })
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, cx: &App) -> bool {
    shown(rail, cx).is_some()
}

/// What the card shows below its header: the thread this one was forked
/// from and the threads it watches, whose rows open them while they exist.
/// Each watched thread can be stopped from its row.
pub(super) fn body(
    rail: &RailView,
    _window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<Option<Shown>>>>("rail-thread-shown");
    if let Some(shown) = shown(rail, cx) {
        last.replace(Some(shown));
    }
    let Shown { source, watched } = last.borrow().clone()?;
    let fork = source.map(|(title, thread_id)| {
        let label = crate::tr!("fork.forked_from", title = title.as_str()).into_owned();
        row(
            "rail-fork-origin".into(),
            IconName::GitBranch,
            label,
            thread_id,
            cx,
        )
        .debug_selector(|| "fork-origin".into())
        .into_any_element()
    });
    let session_id = rail.session_id.clone();
    let watches = watched.into_iter().map(|(watched_id, (title, thread_id))| {
        let label = crate::tr!("chat.rail_watching", title = title.as_str()).into_owned();
        let row_id = SharedString::from(format!("rail-watching-{watched_id}"));
        let stop = session_id.clone().map(|session_id| {
            Button::new(SharedString::from(format!(
                "rail-stop-watching-{watched_id}"
            )))
            .motion(&rail.motion)
            .ghost()
            .xsmall()
            .icon(IconName::EyeOff)
            .aria_label(crate::tr!("chat.rail_stop_watching"))
            .tooltip(crate::tr!("chat.rail_stop_watching"))
            .on_click(cx.listener(move |rail, _, _, cx| {
                let (session_id, watched_id) = (session_id.clone(), watched_id.clone());
                rail.workspace_store.update(cx, |store, _| {
                    store.stop_watching_threads(session_id, vec![watched_id])
                });
            }))
        });
        h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(row(row_id, IconName::Eye, label, thread_id, cx)),
            )
            .children(stop)
            .into_any_element()
    });
    Some(
        v_flex()
            .w_full()
            .min_w_0()
            .children(fork)
            .children(watches)
            .into_any_element(),
    )
}

/// A row naming another thread, which opens it while `thread_id` says it exists.
fn row(
    id: SharedString,
    icon: IconName,
    label: String,
    thread_id: Option<String>,
    cx: &mut Context<RailView>,
) -> crate::material::Clickable<gpui::Div> {
    let radius = crate::material::radius_card(cx);
    let opens = thread_id.is_some();
    crate::material::accessible_clickable(h_flex(), id, Role::Button, label.clone(), cx)
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
                .child(crate::icon::Icon::new(icon).xsmall()),
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
        })
}
