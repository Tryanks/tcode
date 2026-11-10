use std::path::PathBuf;
use std::time::Duration;

use super::*;

const SESSION: &str = "thread-1";
/// Longer than any test waits for a command it ends itself.
const NEVER: Duration = Duration::from_secs(3600);

struct Fixture {
    root: PathBuf,
    manager: TaskManager,
    events: async_channel::Receiver<TaskEvent>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("tcode-tasks-{}", uuid::Uuid::new_v4()));
        let (manager, events) = TaskManager::open(root.clone());
        Self {
            root,
            manager,
            events,
        }
    }

    fn spawn(&self, command: &str, foreground: bool) -> TaskInfo {
        self.manager
            .spawn(
                SESSION,
                Spawn {
                    command: command.into(),
                    cwd: self.root.clone(),
                    foreground,
                    notify_on_exit: false,
                    item_id: None,
                    env: None,
                    timeout: None,
                },
            )
            .unwrap()
    }

    /// A file the command waits for, so a test decides when it ends.
    fn gate(&self) -> PathBuf {
        self.root.join(format!("gate-{}", uuid::Uuid::new_v4()))
    }

    async fn exited(&self, task_id: &str) {
        while !matches!(
            self.events.recv().await.unwrap(),
            TaskEvent::Exited { task_id: ref id, .. } if id == task_id
        ) {}
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.manager.stop_all(SESSION, false);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn gated(gate: &std::path::Path, then: &str) -> String {
    format!(
        "while [ ! -e '{}' ]; do sleep 0.02; done; {then}",
        gate.display()
    )
}

/// A command that exits within the call answers with its exit code and its
/// output, and its end, already seen, wakes nobody.
#[tokio::test]
async fn a_run_that_exits_answers_with_its_code_and_output() {
    let fixture = Fixture::new();
    let task = fixture.spawn("printf 'one\\ntwo\\nthree\\n'; echo oops >&2; exit 3", true);
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        NEVER,
        std::future::pending(),
    )
    .await
    .unwrap();
    let Followed::Exited(task) = followed else {
        panic!("{followed:?}");
    };
    assert!(matches!(
        task.phase,
        Phase::Exited {
            exit_code: Some(3),
            ..
        }
    ));
    let tail = fixture
        .manager
        .read(
            SESSION,
            &task.task_id,
            Window::Tail {
                lines: 2,
                max_bytes: 1024,
            },
        )
        .unwrap();
    assert_eq!(tail.text, "three\noops\n");
    assert_eq!(tail.end, tail.total_bytes);
    let whole = fixture
        .manager
        .read(
            SESSION,
            &task.task_id,
            Window::From {
                offset: 4,
                max_bytes: 4,
            },
        )
        .unwrap();
    assert_eq!(
        (whole.text.as_str(), whole.start, whole.end),
        ("two\n", 4, 8)
    );
    assert_eq!(fixture.manager.take_wake(SESSION, &task.task_id), None);
}

/// A command still running when the call yields goes on in the background,
/// listed there, and its exit is the thread's to hear of, once.
#[tokio::test]
async fn a_run_that_yields_goes_on_in_the_background_and_its_exit_wakes_once() {
    let fixture = Fixture::new();
    let gate = fixture.gate();
    let task = fixture.spawn(&gated(&gate, "echo built; exit 7"), true);
    assert!(fixture.manager.background(SESSION).is_empty());
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        Duration::ZERO,
        std::future::pending(),
    )
    .await
    .unwrap();
    let Followed::Backgrounded(moved) = followed else {
        panic!("{followed:?}");
    };
    assert_eq!(moved.moved_by, Some(Mover::Yield));
    assert!(moved.notify_on_exit && moved.phase.running());
    assert_eq!(
        fixture
            .manager
            .background(SESSION)
            .iter()
            .map(|task| task.task_id.as_str())
            .collect::<Vec<_>>(),
        [task.task_id.as_str()]
    );
    std::fs::write(&gate, "").unwrap();
    fixture.exited(&task.task_id).await;
    assert!(fixture.manager.background(SESSION).is_empty());
    let wake = fixture.manager.take_wake(SESSION, &task.task_id).unwrap();
    assert!(matches!(
        wake.phase,
        Phase::Exited {
            exit_code: Some(7),
            ..
        }
    ));
    assert_eq!(fixture.manager.take_wake(SESSION, &task.task_id), None);
}

/// The user moves the run a tool call waits on, found by that call's item
/// before any other foreground task; the call returns at once.
#[tokio::test]
async fn the_user_moves_the_run_of_a_call_to_the_background() {
    let fixture = Fixture::new();
    let gate = fixture.gate();
    let other = fixture.spawn(&gated(&gate, "true"), true);
    let task = fixture
        .manager
        .spawn(
            SESSION,
            Spawn {
                command: gated(&gate, "true"),
                cwd: fixture.root.clone(),
                foreground: true,
                notify_on_exit: false,
                item_id: Some("call-2".into()),
                env: None,
                timeout: None,
            },
        )
        .unwrap();
    assert_eq!(
        fixture.manager.foreground_for_item(SESSION, "call-2"),
        Some(task.task_id.clone())
    );
    assert_eq!(
        fixture.manager.foreground_for_item(SESSION, "call-unbound"),
        Some(other.task_id.clone())
    );
    let manager = fixture.manager.clone();
    let id = task.task_id.clone();
    let mover = tokio::spawn(async move {
        let mut state = manager.watch(SESSION, &id).unwrap();
        state.wait_for(|state| state.foreground).await.unwrap();
        manager
            .move_to_background(SESSION, &id, Mover::User)
            .unwrap();
    });
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        NEVER,
        std::future::pending(),
    )
    .await
    .unwrap();
    mover.await.unwrap();
    let Followed::Backgrounded(moved) = followed else {
        panic!("{followed:?}");
    };
    assert_eq!(moved.moved_by, Some(Mover::User));
    std::fs::write(&gate, "").unwrap();
}

/// A cancelled call stops its command, and so does a call dropped
/// unanswered; what the command started goes with it.
#[tokio::test]
async fn a_cancelled_or_dropped_run_stops_its_command() {
    let fixture = Fixture::new();
    let task = fixture.spawn("sleep 600 & wait", true);
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        NEVER,
        std::future::ready(()),
    )
    .await
    .unwrap();
    assert!(matches!(followed, Followed::Cancelled(_)));
    fixture.exited(&task.task_id).await;
    let stopped = fixture.manager.get(SESSION, &task.task_id).unwrap();
    assert!(stopped.stopped);
    assert!(matches!(
        stopped.phase,
        Phase::Exited {
            signal: Some(_),
            ..
        }
    ));

    let dropped = fixture.spawn("sleep 600", true);
    let call = follow(
        &fixture.manager,
        SESSION,
        &dropped.task_id,
        NEVER,
        std::future::pending(),
    );
    // Polled once, so it waits, then dropped as a closed connection drops it.
    assert!(
        tokio::time::timeout(Duration::ZERO, call).await.is_err(),
        "the call answered before its command ended"
    );
    fixture.exited(&dropped.task_id).await;
    assert!(
        fixture
            .manager
            .get(SESSION, &dropped.task_id)
            .unwrap()
            .stopped
    );
}

/// A stopped background task ends, and wakes the thread when it asked to.
#[tokio::test]
async fn a_stopped_task_ends_and_reports_it() {
    let fixture = Fixture::new();
    let task = fixture
        .manager
        .spawn(
            SESSION,
            Spawn {
                command: "sleep 600".into(),
                cwd: fixture.root.clone(),
                foreground: false,
                notify_on_exit: true,
                item_id: None,
                env: None,
                timeout: None,
            },
        )
        .unwrap();
    fixture.manager.stop(SESSION, &task.task_id).unwrap();
    fixture.exited(&task.task_id).await;
    let wake = fixture.manager.take_wake(SESSION, &task.task_id).unwrap();
    assert!(wake.stopped);
    assert!(matches!(
        wake.phase,
        Phase::Exited {
            signal: Some(15),
            ..
        }
    ));
}

/// A host that opens the data its last run left finds what ran then lost,
/// not running, and numbers new tasks after the old ones. Another thread's
/// tasks are not reachable by id.
#[tokio::test]
async fn a_restarted_host_marks_what_ran_lost() {
    let fixture = Fixture::new();
    let gate = fixture.gate();
    let ran = fixture.spawn(&gated(&gate, "true"), false);
    let done = fixture.spawn("true", false);
    fixture.exited(&done.task_id).await;
    let (reopened, _events) = TaskManager::open(fixture.root.clone());
    let listed = reopened.list(SESSION);
    assert_eq!(listed.len(), 2);
    assert_eq!(
        reopened.get(SESSION, &ran.task_id).unwrap().phase,
        Phase::Lost
    );
    assert!(matches!(
        reopened.get(SESSION, &done.task_id).unwrap().phase,
        Phase::Exited {
            exit_code: Some(0),
            ..
        }
    ));
    assert_eq!(reopened.take_wake(SESSION, &ran.task_id), None);
    assert!(reopened.get("thread-2", &ran.task_id).is_err());
    let next = reopened
        .spawn(
            SESSION,
            Spawn {
                command: "true".into(),
                cwd: fixture.root.clone(),
                foreground: false,
                notify_on_exit: false,
                item_id: None,
                env: None,
                timeout: None,
            },
        )
        .unwrap();
    assert_eq!(next.task_id, "t3");
    std::fs::write(&gate, "").unwrap();
}

/// A deadline stops the command wherever it runs, as timed out; it is not the
/// yield, which only moves it to the background. A command given its own
/// environment runs with that alone.
#[tokio::test]
async fn a_deadline_stops_the_command_and_an_environment_replaces_the_hosts() {
    let fixture = Fixture::new();
    let spawn = |command: &str, timeout: Option<Duration>, env| {
        fixture
            .manager
            .spawn(
                SESSION,
                Spawn {
                    command: command.into(),
                    cwd: fixture.root.clone(),
                    foreground: true,
                    notify_on_exit: false,
                    item_id: None,
                    env,
                    timeout,
                },
            )
            .unwrap()
    };
    let task = spawn("sleep 600", Some(Duration::from_millis(300)), None);
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        Duration::ZERO,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert!(matches!(followed, Followed::Backgrounded(ref task) if task.phase.running()));
    fixture.exited(&task.task_id).await;
    let ended = fixture.manager.take_wake(SESSION, &task.task_id).unwrap();
    assert!(ended.timed_out && ended.stopped, "{ended:?}");

    let env = [
        ("PATH", std::env::var("PATH").unwrap()),
        ("TCODE_PROBE", "from-pi".into()),
    ]
    .map(|(key, value)| (key.to_owned(), value))
    .into_iter()
    .collect();
    let task = spawn("echo \"$TCODE_PROBE ${HOME-unset}\"", None, Some(env));
    let followed = follow(
        &fixture.manager,
        SESSION,
        &task.task_id,
        NEVER,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert!(matches!(followed, Followed::Exited(ref task) if !task.timed_out));
    let output = fixture
        .manager
        .read(
            SESSION,
            &task.task_id,
            Window::Tail {
                lines: 5,
                max_bytes: 1024,
            },
        )
        .unwrap();
    assert_eq!(output.text, "from-pi unset\n");
}
