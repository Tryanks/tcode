//! A window resized as macOS animates it into full screen: thirty sizes
//! from 1200×800 to 1512×982, one a display frame, over a workspace with
//! many threads listed and a long thread of Markdown and code shown beside
//! its rail.

use super::scenes::serve_log;
use super::*;

const TURNS: usize = 40;
const THREADS: usize = 80;
const STEPS: usize = 30;
const FRAME: Duration = Duration::from_millis(16);
const FROM: (f32, f32) = (1200., 800.);
const TO: (f32, f32) = (1512., 982.);

fn answer(turn: usize) -> String {
    format!(
        "## Step {turn}\n\nThe build reads `Cargo.toml` and then the workspace members, \
         each of them in turn, so the change in turn {turn} reaches every crate that \
         depends on it. A longer paragraph follows so the text wraps over several lines \
         at any width the window takes on the way to full screen.\n\n\
         - the first item, with `inline code`\n- the second item, a little longer than the first\n\
         - the third\n\n```rust\nfn step_{turn}(input: &str) -> usize {{\n    let mut total = 0;\n    \
         for line in input.lines() {{\n        total += line.len();\n    }}\n    total\n}}\n```\n\n\
         That is all for this step.\n"
    )
}

fn serve_long_thread(host: &MountedShell, id: &str) {
    let mut status = session_status(id, std::path::Path::new(PANES_CWD));
    let item = |id: String, content| {
        agent::AgentEvent::ItemCompleted(agent::ThreadItem {
            id,
            parent_item_id: None,
            content,
        })
    };
    let mut events = Vec::new();
    for turn in 0..TURNS {
        events.push(agent::AgentEvent::TurnStarted {
            turn_id: format!("{id}-turn-{turn}"),
        });
        events.push(item(
            format!("{id}-ask-{turn}"),
            agent::ItemContent::UserMessage {
                text: format!("What changes in step {turn}, and which crates does it reach?"),
                attachments: vec![],
            },
        ));
        events.push(item(
            format!("{id}-run-{turn}"),
            agent::ItemContent::CommandExecution {
                command: format!("cargo test --test step-{turn}"),
                output: "ok\n".into(),
                exit_code: Some(0),
                status: agent::ItemStatus::Completed,
            },
        ));
        events.push(item(
            format!("{id}-answer-{turn}"),
            agent::ItemContent::AssistantMessage {
                text: answer(turn),
                summarized: false,
            },
        ));
        events.push(agent::AgentEvent::TurnCompleted {
            turn_id: format!("{id}-turn-{turn}"),
            status: agent::TurnStatus::Completed,
            usage: None,
        });
    }
    status.goal = Some(agent::Goal {
        objective: format!("Keep {id} going"),
        status: agent::GoalStatus::Active,
        tokens_used: Some(12_000),
        token_budget: Some(200_000),
        time_used_secs: Some(90),
        checks: Some(2),
        last_check: None,
    });
    send_events(
        host,
        vec![(
            Topic::SessionPlan {
                session_id: id.into(),
            },
            ServerEvent::SessionPlanReplaced(tcode_protocol::SessionPlan {
                session_id: id.into(),
                steps: (0..3)
                    .map(|step| agent::PlanStep {
                        step: format!("Step {step} of {id}'s plan"),
                        status: agent::PlanStepStatus::Pending,
                    })
                    .collect(),
                explanation: None,
            }),
        )],
    );
    serve_log(host, status, events);
}

fn many_threads() -> (Topic, ServerEvent) {
    let sessions = (0..THREADS).map(|index| {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            PANES_CWD.into(),
            None,
        );
        meta.id = if index == 0 {
            "thread-1".into()
        } else {
            format!("thread-{}", index + 1)
        };
        meta.title = format!("Thread number {index} about something");
        meta.updated_at = (THREADS - index) as u64;
        meta
    });
    (
        Topic::Index,
        ServerEvent::IndexSnapshot(IndexSnapshot {
            summary: Default::default(),
            sessions: sessions.collect(),
            projects: Vec::new(),
        }),
    )
}

/// The workspace at rest at [`FROM`], nothing focused so no caret blinks.
fn mount_scene(cx: &mut TestAppContext) -> &mut VisualTestContext {
    let (shell, host, cx) = mount_panes(cx);
    send_events(&host, vec![many_threads()]);
    serve_long_thread(&host, "thread-1");
    await_restore_update(&shell, cx, |store, cx| {
        store.with_timeline_for(Some("thread-1"), |timeline| timeline.turns.len(), cx)
            == Some(TURNS)
    });
    cx.simulate_resize(size(px(FROM.0), px(FROM.1)));
    cx.update(|window, cx| window.blur(cx));
    crate::test_frames::draw_settled(cx);
    cx
}

/// One display frame: the window's frame callbacks run, and it draws if
/// anything asked it to.
fn display(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(FRAME);
    cx.run_until_parked();
    cx.update(|window, cx| window.simulate_next_frame(cx));
}

/// The frames drawn and the layouts computed since the last call.
fn work(cx: &mut VisualTestContext) -> (u64, u64) {
    cx.update(|window, _| {
        let stats = window.layout_stats();
        window.reset_layout_stats();
        (stats.frames, stats.compute_layout_calls)
    })
}

/// The window resized through [`STEPS`] sizes from `from` to `to`, one a
/// display frame, and the layouts the frames each new size drew computed,
/// before that display frame.
fn sweep(from: (f32, f32), to: (f32, f32), cx: &mut VisualTestContext) -> u64 {
    work(cx);
    let mut layouts = 0;
    for step in 1..=STEPS {
        let t = step as f32 / STEPS as f32;
        cx.simulate_resize(size(
            px(from.0 + (to.0 - from.0) * t),
            px(from.1 + (to.1 - from.1) * t),
        ));
        layouts += work(cx).1;
        display(cx);
    }
    layouts
}

/// The window at `at`, its motion landed.
fn settle_at(at: (f32, f32), cx: &mut VisualTestContext) {
    cx.simulate_resize(size(px(at.0), px(at.1)));
    crate::test_frames::draw_settled(cx);
}

/// Widening the window while the thread's column keeps its widest lays out
/// what heightening it does, give or take the few sizes at which the list's
/// own width does change: the rows off screen keep their heights, rather
/// than all being laid out again at every size the window passes through.
/// Both start where the rail is in, so neither has it come in on the way.
#[gpui::test]
fn a_window_widened_into_full_screen_lays_out_what_one_heightened_does(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let cx = mount_scene(cx);
    settle_at((TO.0, FROM.1), cx);
    let heightened = sweep((TO.0, FROM.1), TO, cx);
    let from = (1300., FROM.1);
    settle_at(from, cx);
    let widened = sweep(from, TO, cx);
    let per_size = heightened / STEPS as u64;
    assert!(
        widened <= heightened + 3 * per_size,
        "widening computed {widened} layouts, heightening {heightened}"
    );
}
