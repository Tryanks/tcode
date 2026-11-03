//! A subagent's window: the agent a call started and the task it was given,
//! how it stands; opened, the report it gave back and the way to its own
//! thread. The agent's own calls are that thread's — the host routes them
//! there and the lead's timeline leaves them out — so the window opens the
//! thread rather than repeating them.

use std::collections::HashMap;

use agent::{ItemContent, ItemStatus};
use gpui::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement as _, ParentElement as _, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use gpui_base::h_flex;
use tcode_core::session::{EntryContent, TimelineEntry};

use super::window::{self, Argument, Chip, Header, Verb};
use super::{Activity, Bound, OPEN_MAX_HEIGHT, ToolCtx};
use crate::chat::ChatView;
use crate::chat::components::assistant::MdState;
use crate::icon::{Icon, IconName};
use crate::markdown::{MarkdownState, MarkdownView};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// The window a subagent item shows as before its body: the agent's type in
/// the verb's weight, its task, the model and effort it ran with. A failed
/// agent's report is its error, told by its first line.
pub(in crate::chat) fn activity(
    agent_type: &str,
    description: &str,
    status: ItemStatus,
    summary: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Activity {
    let summary = summary.filter(|summary| !summary.trim().is_empty());
    let mut header = Header::new(IconName::Bot, Verb("agent"))
        .argument(Argument::plain(crate::chat::model::one_line(description)))
        .chip(model.map(|model| Chip::Tag(model.to_owned())))
        .chip(effort.map(|effort| Chip::Tag(effort.to_owned())));
    if !agent_type.is_empty() {
        header = header.subject(agent_type.to_owned());
    }
    let mut copy = Vec::new();
    super::copy(&mut copy, "tools.copy_task", description);
    if let Some(summary) = summary {
        super::copy(&mut copy, "tools.copy_summary", summary);
    }
    Activity {
        header,
        status,
        output: summary.map(str::to_owned),
        copy,
        expandable: summary.is_some(),
        path: None,
    }
}

/// The thread the host keeps a subagent's own work in, once it has one.
/// `None` for any other entry.
pub(in crate::chat) fn child_thread(
    chat: &ChatView,
    entry: &TimelineEntry,
    cx: &App,
) -> Option<String> {
    if !matches!(
        &entry.content,
        EntryContent::Item(ItemContent::Subagent { .. })
    ) {
        return None;
    }
    let lead = chat.session_id.as_deref()?;
    chat.workspace_store
        .read(cx)
        .sidebar_sessions()
        .into_iter()
        .find(|meta| {
            meta.parent_session_id.as_deref() == Some(lead)
                && meta.native_subagent.as_deref() == Some(entry.id.as_str())
        })
        .map(|meta| meta.id)
}

/// An open subagent window's body: the report, as Markdown — or, when the
/// agent failed, verbatim as its error — then the way to the agent's thread.
pub(in crate::chat) fn body(entry: &TimelineEntry, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let EntryContent::Item(ItemContent::Subagent {
        status, summary, ..
    }) = &entry.content
    else {
        return div().into_any_element();
    };
    let summary = summary
        .as_deref()
        .filter(|summary| !summary.trim().is_empty());
    let report = summary.map(|summary| {
        if *status == ItemStatus::Failed {
            failure(entry, summary, ctx)
        } else {
            report(entry, summary, ctx)
        }
    });
    let open = child_thread(ctx.chat, entry, ctx.app()).map(|child| open_thread(entry, child, ctx));
    super::body(report.into_iter().chain(open))
}

fn report(entry: &TimelineEntry, summary: &str, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let state = ctx.chat.agent_reports.state(&entry.id, summary, ctx.cx);
    let key = format!("subagent-summary-{}", entry.id);
    let cx = ctx.app();
    window::section(
        &crate::tr!("tools.summary"),
        div()
            .id(SharedString::from(key.clone()))
            .debug_selector(move || key.clone())
            .w_full()
            .min_w_0()
            .max_h(px(OPEN_MAX_HEIGHT))
            .overflow_y_scroll()
            .text_secondary(false, cx)
            .child(
                MarkdownView::new(&state)
                    .compact_headings(true)
                    .base_dir(ctx.cwd),
            ),
        cx,
    )
    .into_any_element()
}

fn failure(entry: &TimelineEntry, summary: &str, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let text = super::bounded_text(entry, "summary", summary, true, Bound::OUTPUT, ctx);
    let cx = ctx.app();
    let danger = cx.theme().danger;
    window::section(
        &crate::tr!("tools.error"),
        div()
            .w_full()
            .min_w_0()
            .pl_2()
            .border_l_2()
            .border_color(danger.opacity(0.6))
            .text_color(danger)
            .child(text),
        cx,
    )
    .into_any_element()
}

/// Opens the agent's thread beside the lead; once a pane shows it, says so,
/// and the same press moves the keyboard there.
fn open_thread(entry: &TimelineEntry, child: String, ctx: &mut ToolCtx<'_, '_>) -> AnyElement {
    let shown = crate::shell::shown_in_pane(&child, ctx.app());
    let store = ctx.chat.workspace_store.clone();
    let lead = ctx.chat.session_id.clone();
    let motion = ctx.chat.motion.clone();
    let id = entry.id.clone();
    let button = Button::new(SharedString::from(format!("subagent-open-{id}")))
        .ghost()
        .xsmall()
        .icon(IconName::Columns2)
        .label(crate::tr!("tools.open_thread"))
        .on_click(move |_, window, cx| {
            crate::agents_panel::open_agent(&store, child.clone(), lead.as_deref(), window, cx);
        });
    crate::motion::Sampled::new(move |window, cx| {
        let gap = gpui::rems(0.5).to_pixels(window.rem_size());
        let check = motion
            .shown(
                format!("subagent-shown-{id}"),
                shown.then_some(()),
                window,
                cx,
            )
            .map(|check| {
                let muted = cx.theme().muted_foreground;
                check.fold_across_gap(
                    gap,
                    h_flex()
                        .id(SharedString::from(format!("subagent-shown-{id}")))
                        .gap_1()
                        .items_center()
                        .debug_selector(|| "subagent-shown".into())
                        .role(Role::Status)
                        .text_secondary(false, cx)
                        .text_color(muted)
                        .child(Icon::new(IconName::Check).xsmall().text_color(muted))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .child(crate::tr!("panes.shown_beside")),
                        ),
                )
            });
        h_flex()
            .gap(gap)
            .items_center()
            .child(
                div()
                    .debug_selector(move || format!("subagent-open-{id}"))
                    .child(button),
            )
            .children(check)
            .into_any_element()
    })
    .into_any_element()
}

/// The Markdown of the reports open windows show, each kept while it is
/// drawn.
#[derive(Default)]
pub(in crate::chat) struct Reports {
    states: HashMap<String, (MdState, bool)>,
}

impl Reports {
    /// Call once per frame, before the rows render.
    pub(in crate::chat) fn sweep(&mut self) {
        self.states.retain(|_, (_, drawn)| std::mem::take(drawn));
    }

    fn state(&mut self, id: &str, text: &str, cx: &mut App) -> Entity<MarkdownState> {
        let (md, drawn) = self
            .states
            .entry(id.to_owned())
            .or_insert_with(|| (MdState::new(text, cx), false));
        md.sync(text.to_owned(), cx);
        *drawn = true;
        md.state.clone()
    }
}
