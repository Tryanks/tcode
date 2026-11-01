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
    pub(in crate::chat) driver: crate::store::tests::LocalHostDriver,
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
        let (_, cx) =
            cx.add_window_view(|window, cx| ChatView::new(store.clone(), window_state, window, cx));
        cx.simulate_resize(size(px(width), px(1_400.)));
        draw_settled(cx);
        (
            Self {
                store,
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
        Read::of(&activity(
            &entry,
            false,
            std::path::Path::new("/Users/me/project"),
        ))
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
    pub(in crate::chat) detail: Option<String>,
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
            detail: header
                .argument
                .as_ref()
                .and_then(|argument| argument.detail.clone()),
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
        detail: None,
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

/// An edited file's window as its header reads: verb, path, line counts,
/// and whether its diff is numbered.
type FileRead = (String, String, Option<(u32, u32)>, bool);

impl StoredThread {
    /// The windows of the edit `id` made, one per file, as their headers
    /// read.
    pub(in crate::chat) fn files(&self, id: &str, cx: &mut VisualTestContext) -> Vec<FileRead> {
        let entry = self.entry(id, cx);
        let cwd = std::path::PathBuf::from("/Users/me/project");
        let edit = super::diff::edit(&entry, &cwd).expect("an edit");
        edit.files
            .iter()
            .map(|file| {
                let header = super::diff::file_header(file);
                (
                    header.verb.expect("a verb").text(edit.status).to_string(),
                    header.argument.expect("a path").text,
                    file.row.counts,
                    file.numbered,
                )
            })
            .collect()
    }
}

/// An edit reads as a window per file: what happened to it, its path and
/// the lines it added and removed, opening on its diff. A harness's own diff
/// keeps its line numbers; an edit made as a tool call — Codex's imported
/// `apply_patch`, a subagent's `Edit` — is drawn from what it asked for and
/// numbers no lines, since it never said which.
#[gpui::test]
fn an_edit_reads_as_its_files_and_opens_on_each_diff(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/diff.jsonl"), 1_024.);
    let file = |verb: &str, path: &str, counts, numbered| {
        (verb.to_owned(), path.to_owned(), counts, numbered)
    };
    let patch = "call_pRHtbnUjD1fys6vF0opYzxxb";
    let codex = "exec-f7170e74-7da8-447e-8dd1-8f9e4171a278";
    for (id, expected) in [
        (
            "toolu_011mXP4LmwByfxyE3aeyTZmG",
            vec![file(
                "Edited",
                "crates/app/src/sessions.rs",
                Some((4, 1)),
                true,
            )],
        ),
        (
            "toolu_017j2Ae9pT4MwYY5MGTQGrtR",
            vec![file(
                "Edited",
                "crates/ui/src/store/mod.rs",
                Some((1, 1)),
                false,
            )],
        ),
        (
            "toolu_01428REq3FY79RkJYwSanEU7",
            vec![file(
                "Created",
                "/Users/me/Documents/Tcode/2026-10-29/tools/extract_subs.sh",
                Some((17, 0)),
                false,
            )],
        ),
        (
            codex,
            vec![file("Edited", "docs/review.md", Some((5, 1)), true)],
        ),
        (
            "exec-c85dea68-c4be-4c0f-a7f3-c0acb537833a",
            vec![
                file("Moved", "crates/services/src/version_check.rs", None, false),
                file(
                    "Moved",
                    "crates/services/src/version_check/app_releases.rs",
                    Some((1, 1)),
                    true,
                ),
            ],
        ),
        (
            patch,
            vec![file(
                "Edited",
                "crates/sampleapp-core/src/helper_protocol/ops.rs",
                Some((4, 0)),
                false,
            )],
        ),
        (
            "toolu_01CbYY9A8YxGCykHFEFqtNq5:toolu_01XFRP8Dbt8t1tPr3LaArd15",
            vec![file(
                "Edited",
                "/tmp/sampleapp-selectors/locales/pages/selectors.yml",
                Some((0, 3)),
                false,
            )],
        ),
    ] {
        assert_eq!(thread.files(id, cx), expected, "{id}");
    }

    thread.open_work_log("toolu_011mXP4LmwByfxyE3aeyTZmG", cx);
    assert!(shows(cx, "tool-chip-toolu_011mXP4LmwByfxyE3aeyTZmG-file-0"));
    // A rename that changed no lines has nothing to open.
    click(
        cx,
        "activity-row-exec-c85dea68-c4be-4c0f-a7f3-c0acb537833a-file-0",
    );
    assert!(!shows(cx, "file-edit-diff"));

    click(cx, &format!("activity-row-{codex}-file-0"));
    assert!(shows(cx, "file-edit-diff-line-0"));
    assert!(shows(cx, "file-edit-diff-gutter-0"));
    click(cx, &format!("activity-row-{codex}-file-0"));
    assert!(!shows(cx, "file-edit-diff"));

    click(cx, &format!("activity-row-{patch}-file-0"));
    assert!(shows(cx, "file-edit-diff-line-0"));
    assert!(!shows(cx, "file-edit-diff-gutter-0"));
    thread.shutdown(cx);
}

/// A long path and its line counts stay inside an edit's header however
/// narrow the chat gets, and the path never wraps: the counts keep their
/// width and the path gives way.
#[gpui::test]
fn a_long_edit_path_and_its_counts_stay_in_the_header_at_narrow_widths(cx: &mut TestAppContext) {
    use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus};
    use tcode_core::session::{EntryContent, Timeline, TurnMeta};

    let mut timeline = Timeline::default();
    timeline.turns = vec![TurnMeta {
        running: true,
        ..TurnMeta::default()
    }];
    let path =
        "crates/ui/src/deeply/nested/module/tree/with/an/absurdly/long/name/live_file_edit_row.rs";
    timeline.entries = vec![
        crate::chat::tests::entry("user", crate::chat::tests::user_item("go")),
        crate::chat::tests::entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![FileChange {
                    path: path.into(),
                    kind: FileChangeKind::Modify,
                    diff: Some(format!(
                        "@@ -1,96 +1,128 @@\n{}{}",
                        "-old\n".repeat(96),
                        "+new\n".repeat(128)
                    )),
                }],
                status: ItemStatus::InProgress,
            }),
        ),
    ];
    let (store, window_state, _) = crate::chat::tests::seed_chat(cx, timeline);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (_, cx) = cx.add_window_view(|window, cx| ChatView::new(store, window_state, window, cx));
    cx.simulate_resize(size(px(900.), px(700.)));
    draw_settled(cx);
    let (row, path, counts) = (
        "activity-row-edit-file-0",
        "tool-argument-edit-file-0",
        "tool-chip-edit-file-0",
    );
    let baseline = cx.debug_bounds(row).expect("header");
    let baseline_counts = cx.debug_bounds(counts).expect("counts");
    for width in (280..=900).rev() {
        cx.simulate_resize(size(px(width as f32), px(700.)));
        crate::test_frames::draw(cx);
        let row = cx.debug_bounds(row).expect("header");
        let path = cx.debug_bounds(path).expect("path");
        let counts = cx.debug_bounds(counts).expect("counts");
        assert!(
            path.left() >= row.left() && path.right() <= row.right(),
            "the path escaped the header at {width}px: {row:?} {path:?}"
        );
        assert!(
            counts.left() >= path.right() && counts.right() <= row.right(),
            "the counts left the header or overlapped the path at {width}px: {path:?} {counts:?}"
        );
        assert_eq!(
            counts.size.width, baseline_counts.size.width,
            "the counts were squeezed at {width}px"
        );
        assert_eq!(
            row.size.height, baseline.size.height,
            "the header grew at {width}px, so the path wrapped"
        );
    }
}

/// A read reads as the file and the lines read: numbered as its harness
/// numbered them, or counted from the offset it asked for when the harness
/// returned bare lines; a failure by the harness's own words; an image read
/// as viewed, its image a tile above the row, whether it came live or from a
/// Codex rollout stored before images had their own field.
#[gpui::test]
fn a_read_reads_as_its_file_and_lines(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/read.jsonl"), 1_024.);
    let claude = "toolu_016biQ3Y87fKqgUWVGucqtNE";
    let pi = "tool_WZrAwFwAbvGI2G5cZra64pTS";
    let image = "toolu_01LaP7uGd5FPjA4TZEacYFKe";
    let rollout = "call_COcZJppSUODpGmo9sQXLhw6m";
    let with_detail = |read: Read, detail: Option<&str>| Read {
        detail: detail.map(str::to_owned),
        ..read
    };
    for (id, expected) in [
        (
            claude,
            with_detail(
                super::tests::read(
                    "Read",
                    None,
                    Some("/tmp/sepia-ref/sepia/skills/sepia/references/model-fingerprints.md"),
                    None,
                ),
                Some("L1–12"),
            ),
        ),
        (
            "toolu_01CHit7GAx8MHvPX8YPiiQne",
            super::tests::read(
                "Read",
                None,
                Some("/tmp/vm-3.png"),
                Some(
                    "File does not exist. Note: your current working directory is /Users/me/project",
                ),
            ),
        ),
        (
            image,
            super::tests::read("Viewed", None, Some("/tmp/tcode-xtrace/ui1.png"), None),
        ),
        (
            pi,
            with_detail(
                super::tests::read(
                    "Read",
                    None,
                    Some(
                        "/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/\
                         node_modules/@earendil-works/pi-ai/dist/api/openai-completions.js",
                    ),
                    None,
                ),
                Some("L346–380"),
            ),
        ),
        (
            rollout,
            super::tests::read(
                "Viewed",
                None,
                Some("/tmp/smart-cabinet-recognition-debug/diff_otsu_binary.png"),
                None,
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    for id in [image, rollout] {
        assert!(shows(cx, &format!("image-read-{id}-0")), "{id}");
    }
    thread.open_work_log(claude, cx);
    click(cx, &format!("activity-row-{claude}"));
    assert!(shows(cx, &format!("tool-text-{claude}-content")));
    thread.open_work_log(pi, cx);
    click(cx, &format!("activity-row-{pi}"));
    let more = format!("tool-text-{pi}-content-more");
    assert!(shows(cx, &more), "a long read shows its first lines");
    click(cx, &more);
    assert!(shows(cx, &format!("tool-text-{pi}-content")));
    thread.shutdown(cx);
}

/// A search reads as the pattern, where it looked and how much it found,
/// and opens on what it found: matches by file and line, paths, a
/// listing's names, each opening its file. The one recorded search is a
/// Claude Grep its harness refused; no recorded thread holds a search that
/// returned results, so the others are calls in the shapes OpenCode's glob
/// and grep and pi's grep and ls return.
#[gpui::test]
fn a_search_reads_as_its_pattern_and_opens_on_what_it_found(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/search.jsonl"), 1_024.);
    let refused = "toolu_013SmuYY5FGMfM9pME2Nkb6n:toolu_01Cx6kFav4s5Rsv4vhYkPark";
    let found = |verb: &str, argument: &str, detail: Option<&str>, count: Option<&str>| Read {
        detail: detail.map(str::to_owned),
        chips: count
            .map(|count| vec![Chip::Count(count.to_owned())])
            .unwrap_or_default(),
        ..read(verb, None, Some(argument), None)
    };
    for (id, expected) in [
        (
            refused,
            Read {
                detail: Some(
                    ".claude/worktrees/agent-ab2974c16489ddc88/crates/app/src/pages/nodes/chains"
                        .to_owned(),
                ),
                ..read(
                    "Searched",
                    None,
                    Some("CHAINS_TAB|DEFAULT_GROUP_ID|tab\\.set|Import|import_"),
                    Some(
                        "<tool_use_error>Error: No such tool available: Grep. Grep is not \
                         available in this session — search file contents with `grep` via the \
                         Bash tool instead.</tool_use_error>",
                    ),
                )
            },
        ),
        (
            "shape-opencode-glob",
            found("Found", "**/*.rs", Some("crates/term"), Some("3 results")),
        ),
        (
            "shape-opencode-grep",
            found(
                "Searched",
                "fn new",
                Some("crates/term/src · *.rs"),
                Some("3 results"),
            ),
        ),
        (
            "shape-pi-grep",
            found("Searched", "TODO", Some("src"), Some("20 results")),
        ),
        ("shape-pi-ls", found("Listed", ".", None, Some("4 results"))),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(refused, cx);
    assert!(shows(cx, &format!("tool-error-{refused}")));
    click(cx, "activity-row-shape-opencode-grep");
    for index in 0..3 {
        assert!(shows(
            cx,
            &format!("tool-result-shape-opencode-grep-{index}")
        ));
    }
    click(cx, "activity-row-shape-opencode-grep");
    // Twenty matches show their first twelve until asked for the rest.
    click(cx, "activity-row-shape-pi-grep");
    assert!(shows(cx, "tool-result-shape-pi-grep-11"));
    assert!(!shows(cx, "tool-result-shape-pi-grep-12"));
    click(cx, "tool-results-shape-pi-grep-more");
    assert!(shows(cx, "tool-result-shape-pi-grep-12"));
    thread.shutdown(cx);
}

/// A fetch reads as its URL and opens on the prompt it was asked with and
/// what came back; a web search as its query and the links it found, each
/// opening its page, with the text around them; a search item with no
/// query says only that it searched.
#[gpui::test]
fn a_web_call_reads_as_its_url_or_query(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/web.jsonl"), 1_024.);
    let fetch = "toolu_01AjUWd82mqBqGyPmhUMzhTS:toolu_013j1D9CWuGU1W1v9VEQaWyt";
    let search = "toolu_01ARRsmtUMKF9Jr8HaVd9DaE:toolu_01UnWqBEoDTfn52xSKRMa3yD";
    for (id, expected) in [
        (
            fetch,
            read(
                "Fetched",
                None,
                Some("https://helpx.adobe.com/animate/kb/end-of-life.html"),
                None,
            ),
        ),
        (
            search,
            Read {
                chips: vec![Chip::Count("9 links".to_owned())],
                ..read(
                    "Searched the web",
                    None,
                    Some("嘉立创 亚克力 激光切割 乳白 下单 CNC 钣金"),
                    None,
                )
            },
        ),
        (
            "exec-2f58edd8-f98d-4a06-ba20-0f55a5486be1",
            read("Searched the web", None, Some("'font'"), None),
        ),
        (
            "exec-24caa8e4-8398-460e-8cd0-d7bbda29ca96",
            read("Searched the web", None, None, None),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(fetch, cx);
    click(cx, &format!("activity-row-{fetch}"));
    assert!(shows(cx, &format!("tool-text-{fetch}-prompt")));
    assert!(shows(cx, &format!("tool-text-{fetch}-output")));
    click(cx, &format!("activity-row-{fetch}"));
    click(cx, &format!("activity-row-{search}"));
    for index in 0..9 {
        assert!(shows(cx, &format!("tool-link-{search}-{index}")), "{index}");
    }
    assert!(shows(cx, &format!("tool-text-{search}-output")));
    thread.shutdown(cx);
}

/// A plan written as a tool call reads as the step under way and how many
/// are done, and opens on the checklist, every step in its order.
#[gpui::test]
fn a_plan_written_as_a_call_reads_as_a_checklist(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/todo.jsonl"), 1_024.);
    let plan = "call_ZBnIlWgl8BTGGZ6KIUCb3Pmo";
    assert_eq!(
        thread.header(plan, cx),
        Read {
            chips: vec![Chip::Count("0/4 done".to_owned())],
            ..read(
                "Updated the plan",
                None,
                Some("审视旧环境变量兼容残留与配置命名"),
                None
            )
        }
    );
    thread.open_work_log(plan, cx);
    click(cx, &format!("activity-row-{plan}"));
    for index in 0..4 {
        assert!(shows(cx, &format!("tool-todo-{plan}-{index}")), "{index}");
    }
    thread.shutdown(cx);
}

/// A subagent keeps its pill, in the shared grammar: interrupted says so
/// in words, beside an agent that completed and one Codex left running no
/// longer.
#[gpui::test]
fn a_subagent_keeps_its_pill_and_says_how_it_stands(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/subagent.jsonl"), 1_024.);
    let claude = "toolu_01N5qbJptVEf5KH4yk3fYa9N";
    let codex = "call_vmWu7cCLM2uUfy2hX4GQhU9Y";
    let interrupted = "call_fCcc53Gl87IKZZRYJZKIisUT";
    thread.open_work_log(claude, cx);
    for id in [claude, codex, interrupted] {
        assert!(shows(cx, &format!("subagent-{id}")), "{id}");
    }
    assert!(shows(cx, &format!("tool-status-chip-{interrupted}")));
    for settled in [claude, codex] {
        assert!(
            !shows(cx, &format!("tool-status-chip-{settled}")),
            "{settled}"
        );
    }
    thread.shutdown(cx);
}

/// An output longer than the host sends with a record arrives as a preview:
/// the window says how much there is and loads the rest when asked, then
/// shows it whole behind the same bound.
#[gpui::test]
fn a_shortened_output_loads_whole_when_asked(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let output = (1..=600)
        .map(|line| format!("line {line} of the result"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(output.len() > tcode_protocol::OUTPUT_PREVIEW_BYTES);
    let call = serde_json::json!({
        "ts": 2_000,
        "event": {
            "type": "item_completed",
            "id": "long",
            "content": {
                "kind": "tool_call",
                "name": "mcp__docs__dump",
                "input": { "topic": "everything" },
                "output": output,
                "status": "completed",
            },
        },
    });
    let log = [
        serde_json::json!({ "ts": 1_000, "event": { "type": "turn_started", "turn_id": "t" } }),
        serde_json::json!({ "ts": 1_001, "event": { "type": "item_completed", "id": "user",
            "content": { "kind": "user_message", "text": "Dump it." } } }),
        call,
        serde_json::json!({ "ts": 3_000, "event": { "type": "turn_completed", "turn_id": "t",
            "status": "completed", "usage": null } }),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let (mut thread, cx) = StoredThread::open(cx, &log, 1_024.);
    thread.open_work_log("long", cx);
    click(cx, "activity-row-long");
    assert!(shows(cx, "load-output-long"), "the preview offers the rest");
    click(cx, "load-output-long");
    let store = thread.store.clone();
    thread
        .driver
        .wait_until(cx, &store, "the whole output", |_| true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while shows(cx, "load-output-long") {
        assert!(
            std::time::Instant::now() < deadline,
            "the output never loaded"
        );
        thread.driver.step(cx, &store);
        draw_settled(cx);
    }
    click(cx, "tool-text-long-output-more");
    assert!(shows(cx, "tool-text-long-output"));
    thread.shutdown(cx);
}
