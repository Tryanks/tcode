//! The pull requests linked to the thread, as their hosts last reported
//! them.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Hsla, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::pull_request::{
    PullRequestBadgeState, PullRequestKey, PullRequestState, ThreadPullRequestLink,
};

use super::RailView;
use crate::icon::IconName;
use crate::sizing::{Sizable as _, Size};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::tooltip::{Tooltip, TooltipTrigger as _};

/// How long a row takes to fold in or out, or to move to a new place.
const ROWS: Duration = Duration::from_millis(200);

/// How long a row's glyph takes to cross-fade to another state's.
const CHANGE: Duration = Duration::from_millis(200);

fn links<'a>(rail: &RailView, cx: &'a App) -> impl Iterator<Item = &'a ThreadPullRequestLink> {
    rail.session_id
        .as_deref()
        .map(|id| rail.workspace_store.read(cx).pull_requests(id))
        .unwrap_or_default()
        .iter()
        .filter(|link| link.visible())
}

/// Whether the card has something to show for `rail`'s thread now.
pub(super) fn present(rail: &RailView, cx: &App) -> bool {
    links(rail, cx).next().is_some()
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
    /// A watch follows it, which wakes the thread when it changes.
    watched: bool,
}

impl Line {
    fn of(link: &ThreadPullRequestLink, watched: &[PullRequestKey]) -> Self {
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
            watched: watched.contains(&link.key),
        }
    }

    fn id(&self) -> String {
        format!(
            "{}/{}#{}",
            self.key.host, self.key.repository, self.key.number
        )
    }
}

/// What the card shows below its header, laid out at the rail's width: a row for each
/// linked pull request on one line, its number and title then small glyphs
/// for its state, checks and review, opening it in the Pull requests tab on a click. Pull requests that leave
/// fold away as they stood.
pub(super) fn body(
    rail: &RailView,
    window: &mut Window,
    cx: &mut Context<RailView>,
) -> Option<AnyElement> {
    let last = rail
        .motion
        .keep::<Rc<RefCell<Vec<Line>>>>("rail-pull-request-lines");
    let watched = rail
        .session_id
        .as_deref()
        .and_then(|id| rail.workspace_store.read(cx).activity(id))
        .map(|activity| activity.wakes.pull_requests.clone())
        .unwrap_or_default();
    let lines: Vec<Line> = links(rail, cx)
        .map(|link| Line::of(link, &watched))
        .collect();
    if !lines.is_empty() {
        last.replace(lines);
    }
    let lines = last.borrow().clone();
    if lines.is_empty() {
        return None;
    }
    let session = rail.session_id.clone()?;
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
    cx: &mut Context<RailView>,
) -> AnyElement {
    let number = format!("#{}", line.key.number);
    let title = line
        .title
        .clone()
        .unwrap_or_else(|| line.key.repository.clone());
    let state = glyph(
        format!("rail-pr-state-{id}"),
        crate::pull_requests::appearance(line.state, cx),
        window,
        cx,
    );
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
    let watch = crate::motion::shown(
        SharedString::from(format!("rail-pr-watch-{id}")),
        line.watched.then_some(line.key.number),
        CHANGE,
        window,
        cx,
    )
    .map(|shown| {
        let number = shown.value;
        shown.fold_across(
            div()
                .id(SharedString::from(format!("rail-pr-watch-{id}-tip")))
                .pl_1()
                .text_color(cx.theme().muted_foreground)
                .child(crate::icon::Icon::new(IconName::Eye).xsmall())
                .with_tooltip(move |_, _| {
                    Tooltip::new(crate::tr!("background.keeps_watch", number = number))
                })
                .into_any_element(),
        )
    });
    let detail = std::iter::once(crate::pull_requests::appearance(line.state, cx).2)
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
    .gap_1()
    .items_center()
    .rounded(radius)
    .cursor_pointer()
    .text_body(false, cx)
    .child(crate::material::row_hover(radius, cx))
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
    .child(state)
    .children(checks)
    .children(review)
    .children(watch)
    // The panel this opens replaces the rail, and the keyboard would go with
    // the row.
    .on_mouse_down(gpui::MouseButton::Left, crate::chat::header::stop_press)
    .on_click(cx.listener(move |rail, _, _, cx| {
        let key = key.clone();
        let session = session.clone();
        rail.workspace_store.update(cx, |store, cx| {
            crate::pull_requests::open_in_thread(store, &session, key, cx)
        });
    }))
    .into_any_element()
}

/// A signal at the end of a row, which folds in and away with the value it
/// shows and cross-fades between values.
fn signal<T: Copy + PartialEq + 'static>(
    id: String,
    value: Option<T>,
    appearance: fn(T, &App) -> (IconName, Hsla, &'static str),
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let shown = crate::motion::shown(SharedString::from(id.clone()), value, CHANGE, window, cx)?;
    let appearance = appearance(shown.value, cx);
    Some(shown.fold_across(glyph(id, appearance, window, cx)))
}

/// A signal's glyph in its colour, drawn small and named in its tooltip; a
/// new glyph cross-fades over the one it replaces.
fn glyph(
    id: String,
    (icon, color, word): (IconName, Hsla, &'static str),
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    div()
        .id(SharedString::from(format!("{id}-tip")))
        .pl_1()
        .child(crate::motion::status_icon_sized(
            SharedString::from(format!("{id}-icon")),
            icon,
            color,
            Size::XSmall,
            CHANGE,
            window,
            cx,
        ))
        .with_tooltip(move |_, _| Tooltip::new(crate::tr!(word)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_core::pull_request::{PullRequestSource, PullRequestStackState};

    /// Two repositories can each have a pull request of the same number; a watch on one says
    /// nothing of the other.
    #[test]
    fn a_watch_marks_only_the_pull_request_it_follows() {
        let link = |repository: &str| ThreadPullRequestLink {
            key: PullRequestKey::new("example.test", repository, 7),
            source: PullRequestSource::Manual,
            url: String::new(),
            linked_at: None,
            snapshot: None,
            stack: PullRequestStackState::Unknown,
            sync_error: None,
            watch: None,
        };
        let (watched, other) = (link("team/app"), link("team/docs"));
        let watches = [watched.key.clone()];
        assert!(Line::of(&watched, &watches).watched);
        assert!(!Line::of(&other, &watches).watched);
    }
}
