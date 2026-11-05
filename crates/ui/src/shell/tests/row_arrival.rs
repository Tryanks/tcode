//! A thread chosen from its row in the threads list comes out of the row
//! into the pane it lands in: a pane that now shows it grows out of the row,
//! and a pane that showed it already takes the row's highlight as it becomes
//! current.

use super::*;

/// Two panes side by side, thread-1 in the left and thread-2 in the right,
/// which is current, with thread-3 listed beside them.
fn mount_beside(
    cx: &mut TestAppContext,
) -> (Entity<AppShell>, MountedShell, &mut VisualTestContext) {
    let (shell, host, cx) = mount_panes(cx);
    let sessions = [("thread-1", 3), ("thread-2", 2), ("thread-3", 1)].map(|(id, updated_at)| {
        let mut meta = tcode_core::project::SessionMeta::new(
            agent::ProviderKind::Codex,
            PANES_CWD.into(),
            None,
        );
        meta.id = id.into();
        meta.title = id.into();
        meta.updated_at = updated_at;
        meta
    });
    send_events(
        &host,
        vec![(
            Topic::Index,
            ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: sessions.to_vec(),
                projects: Vec::new(),
            }),
        )],
    );
    await_restore_update(&shell, cx, |store, _| {
        store.thread_meta("thread-3").is_some()
    });
    open_beside_from_list(&shell, &host, "sidebar-thread-thread-2", "thread-2", cx);
    settle_panes(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    (shell, host, cx)
}

/// Whether `a` and `b` are the same rectangle, each edge within `by`.
fn near(a: gpui::Bounds<Pixels>, b: gpui::Bounds<Pixels>, by: f32) -> bool {
    let edges = |r: gpui::Bounds<Pixels>| [r.left(), r.top(), r.right(), r.bottom()];
    edges(a)
        .iter()
        .zip(edges(b))
        .all(|(a, b)| f32::from(*a - b).abs() <= by)
}

/// Whether each edge of `now` lies between that edge of `from` and of `to`,
/// clear of both where they differ.
fn between(
    from: gpui::Bounds<Pixels>,
    to: gpui::Bounds<Pixels>,
    now: gpui::Bounds<Pixels>,
) -> bool {
    let edges = |r: gpui::Bounds<Pixels>| [r.left(), r.top(), r.right(), r.bottom()];
    edges(from)
        .into_iter()
        .zip(edges(to))
        .zip(edges(now))
        .all(|((from, to), now)| {
            let (low, high) = if from < to { (from, to) } else { (to, from) };
            if high - low < px(1.) {
                (now - low).abs() < px(1.)
            } else {
                low < now && now < high
            }
        })
}

/// Displays frames until nothing is drawn as `selector`, and returns where
/// it was last drawn.
fn last_drawn(selector: &'static str, cx: &mut VisualTestContext) -> gpui::Bounds<Pixels> {
    let mut last = cx.debug_bounds(selector).expect("drawn");
    for _ in 0..120 {
        display_frame(cx);
        match cx.debug_bounds(selector) {
            Some(now) => last = now,
            None => return last,
        }
    }
    panic!("{selector} is still drawn");
}

/// A row whose thread a pane takes in place of its own grows from the row's
/// bounds into the pane's, holding the pane's content where it rests: the
/// thread's first frame is drawn as it is at rest. Reduced motion puts the
/// pane in place at once.
#[gpui::test]
fn a_thread_chosen_from_its_row_grows_out_of_the_row_into_its_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, host, cx) = mount_beside(cx);
    let panes = pane_bounds(&shell, cx);
    let row = cx
        .debug_bounds("sidebar-thread-thread-3")
        .expect("the thread's row");

    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-3".into())],
        "the current pane takes the thread"
    );
    let first = cx.debug_bounds("pane-arrival").expect("the pane's surface");
    assert!(
        near(first, row, 0.5),
        "it leaves from the row: {first:?}, the row {row:?}"
    );
    assert_eq!(cx.debug_bounds("pane-arrival-highlight"), None);

    serve_thread(&host, "thread-3");
    await_restore_update(&shell, cx, |store, cx| {
        !store.chat_loading_for(Some("thread-3"), cx)
    });
    let title = cx.debug_bounds("chat-title").expect("thread-3's title");
    assert!(
        panes[1].contains(&title.center()),
        "{title:?} in {:?}",
        panes[1]
    );

    cx.executor().advance_clock(crate::motion::expand::GROW / 4);
    draw(cx);
    let part = cx.debug_bounds("pane-arrival").expect("on its way");
    assert!(
        between(row, panes[1], part),
        "between the row {row:?} and the pane {:?}: {part:?}",
        panes[1]
    );

    let last = last_drawn("pane-arrival", cx);
    assert!(
        near(last, panes[1], 3.),
        "it lands on the pane: {last:?}, the pane {:?}",
        panes[1]
    );
    settle_frames(cx);
    assert_eq!(
        cx.debug_bounds("chat-title"),
        Some(title),
        "at rest from its first frame"
    );
    assert_eq!(pane_bounds(&shell, cx), panes, "the layout is as it was");

    cx.update(|_, cx| cx.set_reduce_motion(true));
    let row = cx
        .debug_bounds("sidebar-thread-thread-2")
        .expect("the thread's row");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())]
    );
    assert_eq!(cx.debug_bounds("pane-arrival"), None, "in place at once");
}

/// A row whose thread a pane beside the current one shows already sends its
/// highlight from the row's bounds over that pane, which becomes current;
/// what the pane shows stays as it is.
#[gpui::test]
fn a_shown_threads_row_sends_its_highlight_to_its_pane(cx: &mut TestAppContext) {
    let _locale_guard = crate::settings::TestLocaleGuard::acquire();
    let (shell, _host, cx) = mount_beside(cx);
    let panes = pane_bounds(&shell, cx);
    let row = cx
        .debug_bounds("sidebar-thread-thread-1")
        .expect("the thread's row");

    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(
        pane_threads(&shell, cx),
        [Some("thread-1".into()), Some("thread-2".into())],
        "the layout is unchanged"
    );
    assert_eq!(keyboard_pane(&shell, cx), Some(0), "its pane is current");
    assert_eq!(cx.debug_bounds("pane-arrival"), None, "nothing new to show");
    let first = cx
        .debug_bounds("pane-arrival-highlight")
        .expect("the row's highlight");
    assert!(
        near(first, row, 0.5),
        "it leaves from the row: {first:?}, the row {row:?}"
    );

    cx.executor().advance_clock(crate::motion::expand::GROW / 4);
    draw(cx);
    let part = cx
        .debug_bounds("pane-arrival-highlight")
        .expect("on its way");
    assert!(
        between(row, panes[0], part),
        "between the row {row:?} and the pane {:?}: {part:?}",
        panes[0]
    );
    let last = last_drawn("pane-arrival-highlight", cx);
    assert!(
        near(last, panes[0], 3.),
        "it lands on the pane: {last:?}, the pane {:?}",
        panes[0]
    );

    // Its pane is current now: its row again sends nothing.
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    draw(cx);
    assert_eq!(cx.debug_bounds("pane-arrival-highlight"), None);
}
