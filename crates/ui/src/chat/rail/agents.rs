//! The thread's subagents and the agents it dispatched.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, RenderOnce, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::h_flex;

use crate::agents_panel::{AgentLine, Agents};
use crate::chat::ChatView;
use crate::typography::TextRole as _;

/// How long a row takes to fold in or out, or to move to a new place.
const ROWS: Duration = Duration::from_millis(200);

fn agents(chat: &ChatView, cx: &App) -> Option<Agents> {
    let lead = chat.session_id.as_deref()?;
    Some(Agents::of(chat.workspace_store.read(cx), lead)).filter(|agents| !agents.is_empty())
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    agents(chat, cx).is_some()
}

/// The line the card's header shows beside its title, open or shut: what
/// is running or awaits settle, or else how many agents there are.
pub(super) fn summary(chat: &ChatView, cx: &App) -> Option<SharedString> {
    let agents = agents(chat, cx)?;
    Some(
        agents
            .summary()
            .unwrap_or_else(|| match agents.count() {
                1 => crate::tr!("agents.pill_one").into_owned(),
                count => crate::tr!("agents.pill", count = count).into_owned(),
            })
            .into(),
    )
}

/// What the card shows open, laid out at the rail's width: a row for each
/// agent, with its state, its title and how long it has run, opening it on
/// a click. Agents that leave the list fold away as they stood.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<RefCell<Vec<AgentLine>>>>("rail-agents-lines");
    if let Some(agents) = agents(chat, cx) {
        last.replace(agents.lines());
    }
    let lines = last.borrow().clone();
    if lines.is_empty() {
        return None;
    }
    let store = chat.workspace_store.clone();
    let lead = chat.session_id.clone();
    let rows = lines
        .into_iter()
        .map(|line| (SharedString::from(line.key.clone()), line));
    Some(
        crate::motion::keyed(
            "rail-agents",
            rows,
            ROWS,
            window,
            cx,
            |_, line, window, cx| row(line, &store, lead.clone(), window, cx),
        )
        .render(gpui_base::v_flex().w_full().min_w_0())
        .into_any_element(),
    )
}

fn row(
    line: &AgentLine,
    store: &Entity<crate::store::WorkspaceStore>,
    lead: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let title = if line.title.trim().is_empty() {
        crate::tr!("agents.untitled").into_owned()
    } else {
        line.title.clone()
    };
    let radius = crate::material::radius_card(cx);
    let label = match line.execution {
        Some(execution) => format!(
            "{title} · {}",
            crate::agents_panel::execution_label(execution)
        ),
        None => title.clone(),
    };
    let elapsed = match (line.running_since, &line.elapsed) {
        (Some(since), _) => Some(
            Elapsed {
                id: SharedString::from(format!("rail-agent-elapsed-{}", line.key)).into(),
                since,
            }
            .into_any_element(),
        ),
        (None, Some(elapsed)) => Some(elapsed.clone().into_any_element()),
        (None, None) => None,
    };
    let key = line.key.clone();
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("rail-agent-{}", line.key)),
        Role::Button,
        label,
        cx,
    )
    .debug_selector(move || format!("rail-agent-{key}"))
    .relative()
    .w_full()
    .min_w_0()
    .px_1()
    .py_1()
    .gap_2()
    .items_center()
    .rounded(radius)
    .text_body(false, cx)
    .child(crate::material::row_hover(radius, cx))
    .child(crate::agents_panel::status_mark(
        &line.key,
        line.execution,
        window,
        cx,
    ))
    .child(
        div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(crate::widgets::fade_text(title)),
    )
    .children(elapsed.map(|elapsed| {
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_secondary(false, cx)
            .child(elapsed)
    }))
    .when_some(line.open.clone(), |row, id| {
        let store = store.clone();
        row.cursor_pointer().on_click(move |_, window, cx| {
            crate::agents_panel::open_agent(&store, id.clone(), lead.as_deref(), window, cx);
        })
    })
    .into_any_element()
}

/// How long a running agent has run, ticking while a frame draws it: the
/// clock is the element's state, so a row no longer drawn drops it.
#[derive(IntoElement)]
struct Elapsed {
    id: ElementId,
    /// When the run started (unix ms).
    since: u64,
}

impl RenderOnce for Elapsed {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let since = self.since;
        window.with_global_id(self.id, |id, window| {
            window.with_element_state(id, |clock: Option<Entity<Clock>>, _| {
                let clock = clock
                    .filter(|clock| clock.read(cx).since == since)
                    .unwrap_or_else(|| cx.new(|cx| Clock::new(since, cx)));
                (clock.clone(), clock)
            })
        })
    }
}

struct Clock {
    since: u64,
    _tick: Task<()>,
}

impl Clock {
    fn new(since: u64, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self { since, _tick: tick }
    }
}

impl Render for Clock {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let now = tcode_core::project::now_secs() * 1000;
        div().child(crate::chat::format_elapsed(now.saturating_sub(self.since)))
    }
}
