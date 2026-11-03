//! The pull requests linked to the thread, as their hosts last reported
//! them.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{
    PullRequestBadgeState, PullRequestKey, PullRequestState, ThreadPullRequestLink,
};

use crate::chat::ChatView;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long a row takes to fold in or out, or to move to a new place.
const ROWS: Duration = Duration::from_millis(200);

/// How long a row's glyph takes to cross-fade to another state's.
const CHANGE: Duration = Duration::from_millis(200);

fn links<'a>(chat: &ChatView, cx: &'a App) -> impl Iterator<Item = &'a ThreadPullRequestLink> {
    chat.session_id
        .as_deref()
        .map(|id| chat.workspace_store.read(cx).pull_requests(id))
        .unwrap_or_default()
        .iter()
        .filter(|link| link.visible())
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    links(chat, cx).next().is_some()
}

/// The line the card's header shows beside its title, open or shut: the
/// one pull request's number and state, or how many are linked.
pub(super) fn summary(chat: &ChatView, cx: &App) -> Option<SharedString> {
    let mut links = links(chat, cx);
    let first = links.next()?;
    Some(
        match links.count() {
            0 => {
                let (_, _, state) = crate::pull_requests::appearance(
                    PullRequestBadgeState::of(first.snapshot.as_ref()),
                    cx,
                );
                format!("#{} · {}", first.key.number, crate::tr!(state))
            }
            more => crate::tr!("rail.pull_requests", count = more + 1).into_owned(),
        }
        .into(),
    )
}

/// One linked pull request as its row shows it.
#[derive(Clone)]
struct Line {
    key: PullRequestKey,
    title: Option<String>,
    state: PullRequestBadgeState,
    /// Checks and review, which only an open pull request's row tells.
    checks: Option<tcode_core::pull_request::ChecksState>,
    review: Option<tcode_core::pull_request::ReviewDecision>,
}

impl Line {
    fn of(link: &ThreadPullRequestLink) -> Self {
        let snapshot = link.snapshot.as_ref();
        let open = snapshot.filter(|snapshot| snapshot.state == PullRequestState::Open);
        Self {
            key: link.key.clone(),
            title: snapshot
                .map(|snapshot| snapshot.title.trim().to_owned())
                .filter(|title| !title.is_empty()),
            state: PullRequestBadgeState::of(snapshot),
            checks: open.and_then(|snapshot| snapshot.checks_state),
            review: open.and_then(|snapshot| snapshot.review_decision),
        }
    }

    fn id(&self) -> String {
        format!(
            "{}/{}#{}",
            self.key.host, self.key.repository, self.key.number
        )
    }
}

/// What the card shows open, laid out at the rail's width: a row for each
/// linked pull request, with its number, title, state, checks and review,
/// opening it in the Pull requests tab on a click. Pull requests that leave
/// fold away as they stood.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<RefCell<Vec<Line>>>>("rail-pull-request-lines");
    let lines: Vec<Line> = links(chat, cx).map(Line::of).collect();
    if !lines.is_empty() {
        last.replace(lines);
    }
    let lines = last.borrow().clone();
    if lines.is_empty() {
        return None;
    }
    let session = chat.session_id.clone()?;
    let rows = lines
        .into_iter()
        .map(|line| (SharedString::from(line.id()), line));
    Some(
        crate::motion::keyed(
            "rail-pull-requests",
            rows,
            ROWS,
            window,
            cx,
            |id, line, window, cx| row(id, line, &session, window, cx),
        )
        .render(v_flex().w_full().min_w_0())
        .into_any_element(),
    )
}

fn row(
    id: &SharedString,
    line: &Line,
    session: &str,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> AnyElement {
    let (glyph, color, state) = crate::pull_requests::appearance(line.state, cx);
    let number = format!("#{}", line.key.number);
    let title = line
        .title
        .clone()
        .unwrap_or_else(|| line.key.repository.clone());
    let detail = std::iter::once(state)
        .chain(
            line.checks
                .map(|checks| crate::pull_requests::checks_appearance(checks, cx).2),
        )
        .chain(
            line.review
                .map(|review| crate::pull_requests::review_appearance(review, cx).2),
        )
        .map(|key| crate::tr!(key).into_owned())
        .collect::<Vec<_>>()
        .join(" · ");
    let checks = signal(
        format!("rail-pr-checks-{id}"),
        line.checks,
        crate::pull_requests::checks_appearance,
        window,
        cx,
    );
    let review = signal(
        format!("rail-pr-review-{id}"),
        line.review,
        crate::pull_requests::review_appearance,
        window,
        cx,
    );
    let radius = crate::material::radius_card(cx);
    let key = line.key.clone();
    let session = session.to_owned();
    let selector = id.clone();
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("rail-pr-{id}")),
        Role::Button,
        format!("{number} {title} · {detail}"),
        cx,
    )
    .debug_selector(move || format!("rail-pr-{selector}"))
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .gap_2()
    .items_start()
    .rounded(radius)
    .cursor_pointer()
    .child(crate::material::row_hover(radius, cx))
    .child(div().flex_none().pt_0p5().child(crate::motion::status_icon(
        SharedString::from(format!("rail-pr-state-{id}")),
        glyph,
        color,
        CHANGE,
        window,
        cx,
    )))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .items_center()
                    .text_body(false, cx)
                    .child(
                        div()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .child(number),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(title)),
                    )
                    .children(checks)
                    .children(review),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .text_secondary(false, cx)
                    .child(crate::widgets::fade_text(detail)),
            ),
    )
    .on_click(cx.listener(move |chat, _, _, cx| {
        let key = key.clone();
        let session = session.clone();
        chat.workspace_store.update(cx, |store, cx| {
            crate::pull_requests::open_in_thread(store, &session, key, cx)
        });
    }))
    .into_any_element()
}

/// A signal beside a row's title, its glyph in its colour, which folds in
/// and away with the value it shows and cross-fades between values.
fn signal<T: Copy + PartialEq + 'static>(
    id: String,
    value: Option<T>,
    appearance: fn(T, &App) -> (crate::icon::IconName, gpui::Hsla, &'static str),
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let shown = crate::motion::shown(SharedString::from(id.clone()), value, CHANGE, window, cx)?;
    let (icon, color, _) = appearance(shown.value, cx);
    Some(
        shown.fold_across(div().pl_1().child(crate::motion::status_icon(
            SharedString::from(format!("{id}-icon")),
            icon,
            color,
            CHANGE,
            window,
            cx,
        ))),
    )
}
