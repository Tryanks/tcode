//! The thread's subagents and the agents it dispatched.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::settlement::AgentExecution;

use crate::agents_panel::{AgentLine, AgentState, Agents};
use crate::chat::ChatView;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long a row takes to fold in or out, or to move to a new place, and
/// a line of it to fold in or out under its title.
const ROWS: Duration = Duration::from_millis(200);

/// How long an agent's state word takes to cross-fade to its next.
const CHANGE: Duration = Duration::from_millis(200);

fn agents(chat: &ChatView, cx: &App) -> Option<Agents> {
    let lead = chat.session_id.as_deref()?;
    Some(Agents::of(chat.workspace_store.read(cx), lead)).filter(|agents| !agents.is_empty())
}

/// Whether the card has something to show for `chat`'s thread now.
pub(super) fn present(chat: &ChatView, cx: &App) -> bool {
    agents(chat, cx).is_some()
}

/// The line the card's header shows beside its title: what
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

/// What the card shows below its header, laid out at the rail's width: a row
/// for each agent, with its title and how long it has run, opening it on a
/// click; under the title, its state word whenever it has a state, what of
/// its model, effort and branch is reported, and what it is doing while it
/// runs or its result once it has one. Agents that leave the list fold away
/// as they stood.
pub(super) fn body(
    chat: &ChatView,
    window: &mut Window,
    cx: &mut Context<ChatView>,
) -> Option<AnyElement> {
    let last = chat
        .motion
        .keep::<Rc<RefCell<Vec<AgentLine>>>>("rail-agents-lines");
    if let Some(agents) = agents(chat, cx) {
        let store = chat.workspace_store.read(cx);
        let lead = chat.session_id.as_deref().unwrap_or_default();
        last.replace(agents.with_results(store, lead).lines());
    }
    let lines = last.borrow().clone();
    if lines.is_empty() {
        return None;
    }
    let store = chat.workspace_store.clone();
    let host = chat.agent_host(cx);
    let origins = chat.side_origins().clone();
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
            |_, line, window, cx| row(line, &store, host.clone(), &origins, window, cx),
        )
        .render(gpui_base::v_flex().w_full().min_w_0())
        .into_any_element(),
    )
}

fn row(
    line: &AgentLine,
    store: &Entity<crate::store::WorkspaceStore>,
    host: Option<Entity<ChatView>>,
    origins: &crate::motion::expand::Origins,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let title = if line.title.trim().is_empty() {
        crate::tr!("agents.untitled").into_owned()
    } else {
        line.title.clone()
    };
    let radius = crate::material::radius_card(cx);
    let label = match line.state {
        Some(state) => format!("{title} · {}", state.label()),
        None => title.clone(),
    };
    let elapsed = match (line.running_since, &line.elapsed) {
        (Some(since), _) => Some(
            crate::widgets::clock::Clock::since(
                gpui::SharedString::from(format!("rail-agent-elapsed-{}", line.key)),
                since,
                |elapsed| crate::chat::model::format_elapsed_seconds(elapsed).into(),
            )
            .into_any_element(),
        ),
        (None, Some(elapsed)) => Some(elapsed.clone().into_any_element()),
        (None, None) => None,
    };
    let title_line = h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_center()
        .text_body(false, cx)
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
        }));
    let facts: Vec<String> = [line.model.clone(), line.effort.clone()]
        .into_iter()
        .flatten()
        .chain(line.branch.as_ref().map(|branch| format!("↳ {branch}")))
        .collect();
    let about =
        (line.state.is_some() || !facts.is_empty()).then(|| (line.state, facts.join(" · ")));
    let about = crate::motion::shown(
        SharedString::from(format!("rail-agent-about-{}", line.key)),
        about,
        ROWS,
        window,
        cx,
    )
    .map(|shown| {
        let (state, facts) = shown.value.clone();
        let muted = cx.theme().muted_foreground;
        let word = state.map(|state| {
            crate::motion::cross_fade_part(
                SharedString::from(format!("rail-agent-state-{}", line.key)),
                state,
                CHANGE,
                window,
                cx,
                |state: &AgentState, _, cx| {
                    div()
                        .whitespace_nowrap()
                        .text_color(state.color(cx))
                        .child(state.label())
                        .into_any_element()
                },
            )
        });
        let separator = word.is_some() && !facts.is_empty();
        shown.fold(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .text_secondary(false, cx)
                .text_color(muted)
                .children(word.map(|word| div().flex_none().child(word)))
                .when(separator, |line| line.child(div().flex_none().child("·")))
                .when(!facts.is_empty(), |line| {
                    line.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(facts)),
                    )
                })
                .into_any_element(),
        )
    });
    let activity = crate::agents_panel::activity_line(
        "rail-agent",
        &line.key,
        line.activity.clone(),
        window,
        cx,
    );
    let failed = line.execution == Some(AgentExecution::Failed);
    let result = crate::motion::shown(
        SharedString::from(format!("rail-agent-result-{}", line.key)),
        line.preview.clone(),
        ROWS,
        window,
        cx,
    )
    .map(|shown| {
        shown.fold(
            div()
                .w_full()
                .pt_0p5()
                .text_secondary(false, cx)
                .line_clamp(2)
                .overflow_hidden()
                .text_color(if failed {
                    cx.theme().danger
                } else {
                    cx.theme().muted_foreground
                })
                .child(crate::widgets::fade_text(shown.value.clone()))
                .into_any_element(),
        )
    });
    let key = line.key.clone();
    let origin = SharedString::from(format!("rail-agent-{}", line.key));
    let row = crate::material::accessible_clickable(
        v_flex(),
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
    .rounded(radius)
    .child(crate::material::row_hover(radius, cx))
    .child(title_line)
    .children(about)
    .children(activity)
    .children(result)
    .when_some(line.open.clone(), |row, id| {
        let store = store.clone();
        let origin = origin.clone();
        row.cursor_pointer().on_click(move |_, window, cx| {
            crate::agents_panel::open_agent(
                &store,
                id.clone(),
                host.clone(),
                Some(origin.clone()),
                window,
                cx,
            );
        })
    });
    origins.mark(origin, radius, row).into_any_element()
}
