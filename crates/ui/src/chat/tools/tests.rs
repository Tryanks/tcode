//! Tool windows driven the way the app shows them: a thread's stored log,
//! read back by a host, folded into the timeline and drawn by the chat.
//! Each log is recorded tool items (`fixtures/*.jsonl`, rows of tcode.db,
//! scrubbed) inside one turn.

use std::sync::Arc;

use gpui::{AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, px, size};
use tcode_core::session::TimelineEntry;

use super::window::Chip;
use super::{Activity, activity};
use crate::chat::ChatView;
use crate::store::WorkspaceStore;
use crate::test_frames::draw_settled;
use crate::window_state::WindowState;

/// A chat open on a thread whose stored log is `log`.
pub(in crate::chat) struct StoredThread {
    pub(in crate::chat) store: Entity<WorkspaceStore>,
    pub(in crate::chat) view: Entity<ChatView>,
    driver: crate::store::tests::LocalHostDriver,
    host: tcode_runtime::pipe::SpawnedHost,
}

impl StoredThread {
    pub(in crate::chat) fn open<'a>(
        cx: &'a mut TestAppContext,
        log: &str,
        width: f32,
    ) -> (Self, &'a mut VisualTestContext) {
        use tcode_core::project::SessionMeta;
        use tcode_services::store::{Mutation, SessionStore};

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        // Motion settles at once, so a running call's spinner rests.
        cx.update(|cx| cx.set_reduce_motion(true));
        let root = crate::store::tests::scratch_root("tool-window-test");
        let sessions = SessionStore::open_at(root.clone()).expect("test session store");
        let meta = SessionMeta::new(agent::ProviderKind::ClaudeCode, root, None);
        let session_id = meta.id.clone();
        sessions.upsert_meta(&meta).expect("persist the thread");
        sessions
            .apply(&[Mutation::replace_event_log(&session_id, log.into())])
            .expect("store the thread's log");
        let host =
            tcode_runtime::pipe::spawn_host(sessions, tcode_runtime::pipe::HostServices::default())
                .expect("spawn test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.select_session(session_id.clone(), cx));
        driver.wait_until(cx, &store, "the thread's history", |cx| {
            store.read_with(cx, |store, _| {
                !store.chat_loading()
                    && store
                        .with_active_timeline(|timeline| !timeline.entries.is_empty())
                        .unwrap_or(false)
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (view, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(size(px(width), px(1_400.)));
        draw_settled(cx);
        (
            Self {
                store,
                view,
                driver,
                host,
            },
            cx,
        )
    }

    /// The thread's entries, as the chat draws them.
    pub(in crate::chat) fn entries(&self, cx: &mut VisualTestContext) -> Vec<Arc<TimelineEntry>> {
        self.store.read_with(cx, |store, _| {
            store
                .with_active_timeline(|timeline| timeline.entries.clone())
                .unwrap_or_default()
        })
    }

    pub(in crate::chat) fn entry(
        &self,
        id: &str,
        cx: &mut VisualTestContext,
    ) -> Arc<TimelineEntry> {
        self.entries(cx)
            .into_iter()
            .find(|entry| entry.id == id)
            .unwrap_or_else(|| panic!("no entry {id}"))
    }

    /// The window of `id`, as its header reads.
    pub(in crate::chat) fn header(&self, id: &str, cx: &mut VisualTestContext) -> Read {
        let entry = self.entry(id, cx);
        Read::of(&activity(&entry, false))
    }

    /// Opens the turn's folded Work Log, as a reader does.
    pub(in crate::chat) fn open_work_log(&self, first: &str, cx: &mut VisualTestContext) {
        click(cx, &format!("worklog-header-0-{first}"));
    }

    pub(in crate::chat) fn shutdown(mut self, cx: &mut VisualTestContext) {
        self.driver.shutdown(cx, &self.store, &self.host);
    }
}

/// Clicks the element `selector` names and lets the window settle.
pub(in crate::chat) fn click(cx: &mut VisualTestContext, selector: &str) {
    let bounds = cx
        .debug_bounds(selector.to_owned().leak())
        .unwrap_or_else(|| panic!("nothing to click at {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    draw_settled(cx);
}

/// Whether the element `selector` names was painted in the last frame.
pub(in crate::chat) fn shows(cx: &mut VisualTestContext, selector: &str) -> bool {
    cx.debug_bounds(selector.to_owned().leak()).is_some()
}

/// A header as text, the way it reads.
#[derive(Debug, PartialEq)]
pub(in crate::chat) struct Read {
    pub(in crate::chat) verb: Option<String>,
    pub(in crate::chat) subject: Option<String>,
    pub(in crate::chat) argument: Option<String>,
    pub(in crate::chat) chips: Vec<Chip>,
    pub(in crate::chat) error: Option<String>,
}

impl Read {
    fn of(activity: &Activity) -> Self {
        let header = &activity.header;
        Self {
            verb: header
                .verb
                .map(|verb| verb.text(activity.status).to_string()),
            subject: header.subject.as_ref().map(|subject| subject.to_string()),
            argument: header
                .argument
                .as_ref()
                .map(|argument| argument.text.clone()),
            chips: header.chips.clone(),
            error: activity.failure(),
        }
    }
}

pub(in crate::chat) fn read(
    verb: &str,
    subject: Option<&str>,
    argument: Option<&str>,
    error: Option<&str>,
) -> Read {
    Read {
        verb: Some(verb.to_owned()),
        subject: subject.map(str::to_owned),
        argument: argument.map(str::to_owned),
        chips: Vec::new(),
        error: error.map(str::to_owned),
    }
}

/// A call no family claims reads as its server and tool and the value that
/// names it; a failure as its own first line, which a Codex result stored
/// as MCP JSON has once the host reads the log back; an item canonicalization
/// does not model as its kind and the path in its summary, never as JSON.
#[gpui::test]
fn a_call_no_family_claims_names_its_server_tool_and_argument(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/generic.jsonl"), 1_024.);
    let mcp = "toolu_01D3QR5wqdMAMUrcBoPhrb1u";
    for (id, expected) in [
        (
            mcp,
            read(
                "Called",
                Some("fl-studio · piano_roll_write_notes"),
                Some("append"),
                None,
            ),
        ),
        (
            "toolu_01GYbQaLznEUcjv91J9ZFWNn",
            read("Called", Some("ToolSearch"), Some("select:Monitor"), None),
        ),
        (
            "toolu_01FQ8S6KvXR7rVJXYYVMHhp3",
            read(
                "Called",
                Some("tcode_control · search"),
                Some("心智"),
                Some("refused: scope host is wider than your grant's project"),
            ),
        ),
        (
            "exec-d4bae9da-f150-466c-a7e0-3a3fc1bd167c",
            read(
                "Called",
                Some("tcode_preview · preview_screenshot"),
                None,
                Some("preview browser has no visible area"),
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }
    for (id, kind, argument) in [
        (
            "exec-5bd102db-8280-4811-a3c3-6146bd58a20c",
            "imageView",
            "/tmp/tcode-before.png",
        ),
        ("call_wmvrnLAr0GTnBoYQog6Gvjq0", "sleep", "20000"),
    ] {
        let header = thread.header(id, cx);
        assert_eq!(
            (header.verb, header.subject, header.argument),
            (None, Some(kind.to_owned()), Some(argument.to_owned())),
            "{id}"
        );
    }

    thread.open_work_log(mcp, cx);
    for failed in [
        "toolu_01FQ8S6KvXR7rVJXYYVMHhp3",
        "exec-d4bae9da-f150-466c-a7e0-3a3fc1bd167c",
    ] {
        assert!(shows(cx, &format!("tool-error-{failed}")), "{failed}");
    }
    assert!(!shows(cx, &format!("tool-error-{mcp}")));

    // Open, the input reads as keys and values; the 13 notes stay folded
    // until asked for, and the output is printed as indented JSON, too long
    // to show whole.
    click(cx, &format!("activity-row-{mcp}"));
    for key in ["channel_index", "mode", "notes", "pattern_number"] {
        assert!(shows(cx, &format!("tool-input-{mcp}-{key}")), "{key}");
    }
    let notes = format!("tool-text-{mcp}-nested-notes");
    assert!(!shows(cx, &notes));
    click(cx, &format!("tool-nested-{mcp}-notes"));
    assert!(shows(cx, &notes));
    assert!(shows(cx, &format!("tool-text-{mcp}-output-more")));

    // An item canonicalization does not model has nothing more to open.
    let other = "exec-5bd102db-8280-4811-a3c3-6146bd58a20c";
    click(cx, &format!("activity-row-{other}"));
    assert!(!shows(cx, &format!("tool-detail-{other}")));
    thread.shutdown(cx);
}

/// A command reads as the command it ran and how it exited: a failure by
/// its exit code and the first line it printed past the harness's own
/// `Exit code N`, a declined one as asked for and declined, never run. A
/// shell run as a tool — Codex's `exec_command` and `write_stdin`, a
/// subagent's `Bash` — reads the same, its exit code taken from the report
/// its harness wraps the output in, and opens on that output alone.
#[gpui::test]
fn a_command_reads_as_what_it_ran_and_how_it_exited(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/shell.jsonl"), 1_024.);
    let ran = |argument: &str, chips: Vec<Chip>, error: Option<&str>| Read {
        chips,
        ..read("Ran", None, Some(argument), error)
    };
    let declined = "exec-1886810a-4a55-4708-8ae0-6005349e79e8";
    let exec = "call_SwgyPy31gqrEcphvMeyBJsST";
    let stdin = "call_HODo1V5SY8J2DrCNPTJqiTc7";
    for (id, expected) in [
        (
            "toolu_01RQ9kdsaRsD7Vh9QGznNJwT",
            ran(
                "git status --short && git log --oneline -3 && ls",
                vec![],
                None,
            ),
        ),
        (
            "toolu_01RNnVjDHfMoqLkL2otV6JiC",
            ran(
                "ls /Users/me/project/src",
                vec![Chip::Exit(1)],
                Some("ls: /Users/me/project/src: No such file or directory"),
            ),
        ),
        (
            "exec-a4d9d295-690b-4905-9427-20acfcd6d992",
            ran(
                "/bin/zsh -lc \"sed -n '1,130p' .github/scripts/check-architecture.sh\"",
                vec![Chip::Exit(1)],
                Some("sed: .github/scripts/check-architecture.sh: No such file or directory"),
            ),
        ),
        (
            declined,
            read(
                "Run",
                None,
                Some(
                    "/bin/zsh -c '/opt/homebrew/bin/adb -t 1 shell cmd appops set \
                     org.koreader.launcher MANAGE_EXTERNAL_STORAGE allow'",
                ),
                None,
            ),
        ),
        (exec, ran("git status --short", vec![], None)),
        (
            stdin,
            Read {
                chips: vec![Chip::Exit(-1)],
                ..read("Sent input", None, Some("session 17856"), None)
            },
        ),
        (
            "tool_mJeTs4Cc0I6gPcCx5W0BlgpT",
            ran("ls -la /Users/me/project", vec![], None),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }
    let child = "toolu_01GZcgPu9J81FC9d5KiL2geW:toolu_015f228ibAN81Gb9q9mdmffs";
    let header = thread.header(child, cx);
    assert_eq!(header.verb.as_deref(), Some("Ran"));
    assert!(
        header
            .argument
            .is_some_and(|argument| argument.starts_with("cd /Users/me/project/SampleGame/")),
    );

    thread.open_work_log("toolu_01RQ9kdsaRsD7Vh9QGznNJwT", cx);
    assert!(shows(cx, &format!("tool-status-chip-{declined}")));
    assert!(shows(cx, "tool-error-toolu_01RNnVjDHfMoqLkL2otV6JiC"));
    // A shell call opens on its command line and what it printed; one that
    // printed nothing says so instead of showing its harness's report.
    click(cx, &format!("activity-row-{exec}"));
    assert!(shows(cx, &format!("tool-shell-{exec}")));
    assert!(!shows(cx, &format!("tool-text-{exec}-output")));
    click(cx, &format!("activity-row-{child}"));
    assert!(shows(cx, &format!("tool-text-{child}-output")));
    thread.shutdown(cx);
}
