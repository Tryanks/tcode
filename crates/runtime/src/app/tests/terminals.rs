use super::*;

/// Deleting a project kills the PTY its unsent draft left running off screen
/// instead of leaving it behind forever.
#[test]
fn deleting_a_project_ends_the_terminal_its_draft_left_running() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-delete-project-draft");
    let root = test_store.root().clone();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("doomed".into(), root.clone(), cx);
    });
    let draft_id = state.read(|state| state.selected.clone().expect("draft selected"));
    open_terminal_drawer(&state, cx, 1, &draft_id);
    let terminals: Vec<_> = state.read(|state| {
        state
            .session_status_snapshot(&draft_id)
            .unwrap()
            .terminals
            .iter()
            .map(|terminal| terminal.id)
            .collect()
    });
    assert!(!terminals.is_empty());
    state.update(cx, |state, cx| state.host.park_active(&draft_id, cx));
    state.read(|state| {
        for &terminal in &terminals {
            assert!(!state.terminal_handle(terminal).unwrap().exited());
        }
    });

    state.update(cx, |state, cx| {
        state.host.delete_project("doomed", cx).detach()
    });
    cx.run_until(|state| {
        terminals.iter().all(|&terminal| {
            state
                .terminal_handle(terminal)
                .is_none_or(|handle| handle.exited())
        })
    });
}

/// A conversation left with a terminal running comes back with its drawer as
/// it was: open, at its height, with the same terminals. One whose shells
/// have all exited comes back closed, without them.
#[test]
fn only_a_running_terminal_reopens_the_drawer_when_its_conversation_loads() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-restore");
    let id = stored_terminal_thread(&test_store);
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    load_conversation(&state, cx, 1, &id);
    open_terminal_drawer(&state, cx, 2, &id);
    state.dispatch_command(
        cx,
        3,
        Command::NewTerminal {
            session_id: id.clone(),
        },
    );
    cx.run_until(|state| {
        state
            .resident(&id)
            .is_some_and(|thread| thread.terminal_workspace.terminals.len() == 2)
    });
    state.dispatch_command(
        cx,
        4,
        Command::SetTerminalHeight {
            session_id: id.clone(),
            height: 320.,
        },
    );
    let left = load_conversation(&state, cx, 5, &id).terminals;
    leave_conversation(&state, cx, 6, &id);

    let status = load_conversation(&state, cx, 7, &id);
    assert!(status.terminal_open);
    assert_eq!(status.terminal_height, 320.);
    assert_eq!(
        status.terminals.iter().map(|t| t.id).collect::<Vec<_>>(),
        left.iter().map(|t| t.id).collect::<Vec<_>>()
    );

    for (request, terminal) in (8..).zip(&left) {
        state.dispatch_command(
            cx,
            request,
            Command::TerminalInput {
                terminal_id: terminal.id,
                bytes: b"exit\r".to_vec(),
            },
        );
    }
    cx.run_until(|state| {
        left.iter().all(|terminal| {
            state
                .terminal_handle(terminal.id)
                .is_some_and(|handle| handle.exited())
        })
    });
    leave_conversation(&state, cx, 20, &id);

    let status = load_conversation(&state, cx, 21, &id);
    assert_drawer_closed_without_terminals(&state, &status);
}

/// Closing a conversation's last terminal forgets its drawer: it comes back
/// neither open nor at the height it had.
#[test]
fn closing_the_last_terminal_forgets_the_drawer() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-close-last");
    let id = stored_terminal_thread(&test_store);
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    load_conversation(&state, cx, 1, &id);
    open_terminal_drawer(&state, cx, 2, &id);
    state.dispatch_command(
        cx,
        3,
        Command::SetTerminalHeight {
            session_id: id.clone(),
            height: 320.,
        },
    );
    let terminal_id = load_conversation(&state, cx, 4, &id).terminals[0].id;
    state.dispatch_command(
        cx,
        5,
        Command::CloseTerminal {
            session_id: id.clone(),
            terminal_id,
        },
    );
    let status = load_conversation(&state, cx, 6, &id);
    assert_drawer_closed_without_terminals(&state, &status);

    leave_conversation(&state, cx, 7, &id);
    let status = load_conversation(&state, cx, 8, &id);
    assert_drawer_closed_without_terminals(&state, &status);
}

#[test]
fn a_drafts_terminal_starts_in_its_project_directory() {
    let cx = &mut TestAppContext::default();
    let test_store = TestStore::new("tcode-terminal-open");
    let project_cwd = test_store.root().join("project");
    std::fs::create_dir_all(&project_cwd).unwrap();
    let state = cx.new_entity(TestClientState::new((*test_store).clone()));

    state.update(cx, |state, cx| {
        state.start_draft("terminal-project".into(), project_cwd.clone(), cx);
    });
    let draft_id = state.read(|state| state.selected.clone().expect("draft selected"));
    open_terminal_drawer(&state, cx, 1, &draft_id);

    state.read(|state| {
        let workspace = &state.resident(&draft_id).unwrap().terminal_workspace;
        assert_eq!(workspace.terminals.len(), 1);
        assert!(
            state
                .session_status_snapshot(&draft_id)
                .unwrap()
                .terminal_open
        );
        assert_eq!(
            std::fs::canonicalize(workspace.terminals[0].terminal.working_directory()).unwrap(),
            std::fs::canonicalize(&project_cwd).unwrap()
        );
    });
}
