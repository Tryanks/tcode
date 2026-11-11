//! Tool windows driven the way the app shows them: a thread's stored log,
//! read back by a host, folded into the timeline and drawn by the chat.
//! Each log is tool items inside one turn (`fixtures/*.jsonl`): rows in the
//! shape the current build writes to tcode.db (`EventEnvelopeRef` in
//! `tcode_services::store`, items as the `agent` adapters map them now),
//! with invented paths, ids and content.

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
                cx,
            )
        });
        driver.wait_until(cx, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, "the thread's history", |cx| {
            store.read_with(cx, |store, cx| {
                !store.chat_loading_for(store.sole_thread().as_deref(), cx)
                    && store
                        .with_timeline_for(
                            store.sole_thread().as_deref(),
                            |timeline| !timeline.entries.is_empty(),
                            cx,
                        )
                        .unwrap_or(false)
            })
        });
        let window_state = cx.new(|_| WindowState::default());
        let (_, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
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
        self.store.read_with(cx, |store, cx| {
            store
                .with_timeline_for(
                    store.sole_thread().as_deref(),
                    |timeline| timeline.entries.clone(),
                    cx,
                )
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
            std::path::Path::new("/home/user/project"),
        ))
    }

    /// Opens the turn's folded Work Log, as a reader does.
    pub(in crate::chat) fn open_work_log(&self, first: &str, cx: &mut VisualTestContext) {
        click(cx, &format!("worklog-header-0-{first}"));
    }

    /// The thread's provider reports `events` while the window shows it, as
    /// a running harness does; returns once the window has `last`.
    pub(in crate::chat) fn provider(
        &mut self,
        events: Vec<agent::AgentEvent>,
        last: &str,
        cx: &mut VisualTestContext,
    ) {
        let thread = self
            .store
            .read_with(cx, |store, _| store.sole_thread())
            .expect("the thread is shown");
        smol::block_on(self.host.update_state_for_test(move |state, cx| {
            for event in events {
                state.provider_event_for_test(&thread, event, cx);
            }
        }))
        .unwrap();
        let store = self.store.clone();
        self.driver.wait_until(cx, "the provider's events", |cx| {
            store.read_with(cx, |store, cx| {
                store
                    .with_timeline_for(
                        store.sole_thread().as_deref(),
                        |timeline| timeline.entries.iter().any(|entry| entry.id == last),
                        cx,
                    )
                    .unwrap_or(false)
            })
        });
        draw_settled(cx);
    }

    pub(in crate::chat) fn shutdown(mut self, cx: &mut VisualTestContext) {
        self.driver.shutdown(cx, &self.host);
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
    pub(in crate::chat) title: Option<String>,
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
                .verb_text(activity.status)
                .map(|verb| verb.to_string()),
            subject: header.subject.as_ref().map(|subject| subject.to_string()),
            title: header.title.as_ref().map(|title| title.to_string()),
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
        title: None,
        argument: argument.map(str::to_owned),
        detail: None,
        chips: Vec::new(),
        error: error.map(str::to_owned),
    }
}

/// A call no family claims reads as its server and tool and the value that
/// names it — a Codex connector's as much as a Claude Code MCP call's.
#[gpui::test]
fn a_call_no_family_claims_names_its_server_tool_and_argument(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/generic.jsonl"), 1_024.);
    let mcp = "toolu_010000000000000000000023";
    let connector = "exec-00000018-0000-0000-0000-000000000018";
    for (id, expected) in [
        (
            mcp,
            read(
                "Called",
                Some("example-db · insert_rows"),
                Some("append"),
                None,
            ),
        ),
        (
            connector,
            read(
                "Called",
                Some("codex_apps · github.search_installed_repositories_v2"),
                Some("example-app"),
                None,
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(mcp, cx);
    assert!(!shows(cx, &format!("tool-error-{mcp}")));
    // The connector's result reads as the text and payload it returned.
    click(cx, &format!("activity-row-{connector}"));
    assert!(shows(cx, &format!("tool-text-{connector}-output")));
    click(cx, &format!("activity-row-{connector}"));

    // Open, the input reads as keys and values; the 13 rows stay folded
    // until asked for, and the output is printed as indented JSON, too long
    // to show whole.
    click(cx, &format!("activity-row-{mcp}"));
    for key in ["batch_number", "mode", "rows", "table_index"] {
        assert!(shows(cx, &format!("tool-input-{mcp}-{key}")), "{key}");
    }
    let rows = format!("tool-text-{mcp}-nested-rows");
    assert!(!shows(cx, &rows));
    click(cx, &format!("tool-nested-{mcp}-rows"));
    assert!(shows(cx, &rows));
    assert!(shows(cx, &format!("tool-text-{mcp}-output-more")));
    thread.shutdown(cx);
}

/// A command reads as the command it ran and how it exited: a failure by
/// its exit code alone, with no line of what it printed under it while
/// shut, a declined one as asked for and declined, never run.
#[gpui::test]
fn a_command_reads_as_what_it_ran_and_how_it_exited(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/shell.jsonl"), 1_024.);
    let ran = |argument: &str, chips: Vec<Chip>, error: Option<&str>| Read {
        chips,
        ..read("Ran", None, Some(argument), error)
    };
    let declined = "exec-0000000c-0000-0000-0000-00000000000c";
    for (id, expected) in [
        (
            "toolu_010000000000000000000009",
            ran(
                "git status --short && git log --oneline -3 && ls",
                vec![],
                None,
            ),
        ),
        (
            "toolu_010000000000000000000010",
            ran("ls /home/user/project/src", vec![Chip::Exit(1)], None),
        ),
        (
            "exec-0000000b-0000-0000-0000-00000000000b",
            ran(
                "/bin/zsh -lc \"sed -n '1,130p' .github/scripts/check-links.sh\"",
                vec![Chip::Exit(1)],
                None,
            ),
        ),
        (
            declined,
            read(
                "Run",
                None,
                Some(
                    "/bin/zsh -c '/usr/local/bin/docker compose -f deploy/compose.yml down \
                     --volumes --remove-orphans'",
                ),
                None,
            ),
        ),
        (
            "tool_000000000000000000000015",
            ran("ls -la /home/user/project", vec![], None),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log("toolu_010000000000000000000009", cx);
    assert!(shows(cx, &format!("tool-status-chip-{declined}")));
    let failed = "toolu_010000000000000000000010";
    assert!(shows(cx, &format!("tool-chip-{failed}")));
    assert!(!shows(cx, &format!("tool-error-{failed}")));
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
        let cwd = std::path::PathBuf::from("/home/user/project");
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
/// the lines it added and removed, opening on its diff. A diff keeps its
/// line numbers; one whose hunks say no lines numbers none.
#[gpui::test]
fn an_edit_reads_as_its_files_and_opens_on_each_diff(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/diff.jsonl"), 1_024.);
    let file = |verb: &str, path: &str, counts, numbered| {
        (verb.to_owned(), path.to_owned(), counts, numbered)
    };
    let unnumbered = "toolu_010000000000000000000002";
    let codex = "exec-00000004-0000-0000-0000-000000000004";
    for (id, expected) in [
        (
            "toolu_010000000000000000000001",
            vec![file(
                "Edited",
                "crates/app/src/metrics.rs",
                Some((4, 1)),
                true,
            )],
        ),
        (
            unnumbered,
            vec![file(
                "Edited",
                "crates/ui/src/state/mod.rs",
                Some((1, 1)),
                false,
            )],
        ),
        (
            "toolu_010000000000000000000003",
            vec![file(
                "Created",
                "/home/user/scratch/tools/make_thumbs.sh",
                Some((17, 0)),
                false,
            )],
        ),
        (
            codex,
            vec![file("Edited", "docs/review.md", Some((5, 1)), true)],
        ),
        (
            "exec-00000005-0000-0000-0000-000000000005",
            vec![
                file("Moved", "crates/services/src/update_check.rs", None, false),
                file(
                    "Moved",
                    "crates/services/src/update_check/releases.rs",
                    Some((1, 1)),
                    true,
                ),
            ],
        ),
    ] {
        assert_eq!(thread.files(id, cx), expected, "{id}");
    }

    thread.open_work_log("toolu_010000000000000000000001", cx);
    assert!(shows(cx, "tool-chip-toolu_010000000000000000000001-file-0"));
    // A rename that changed no lines has nothing to open.
    click(
        cx,
        "activity-row-exec-00000005-0000-0000-0000-000000000005-file-0",
    );
    assert!(!shows(cx, "file-edit-diff"));

    click(cx, &format!("activity-row-{codex}-file-0"));
    assert!(shows(cx, "file-edit-diff-line-0"));
    assert!(shows(cx, "file-edit-diff-gutter-0"));
    click(cx, &format!("activity-row-{codex}-file-0"));
    assert!(!shows(cx, "file-edit-diff"));

    click(cx, &format!("activity-row-{unnumbered}-file-0"));
    assert!(shows(cx, "file-edit-diff-line-0"));
    assert!(!shows(cx, "file-edit-diff-gutter-0"));
    thread.shutdown(cx);
}

/// A read reads as the file and the lines read, numbered from the line its
/// harness said the read began at; a failure by the harness's own words; an
/// image read as viewed, its image a tile above the row, whether it came as a
/// read call or as the image Codex was shown.
#[gpui::test]
fn a_read_reads_as_its_file_and_lines(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/read.jsonl"), 1_024.);
    let claude = "toolu_010000000000000000000018";
    let pi = "tool_000000000000000000000021";
    let image = "toolu_010000000000000000000020";
    let rollout = "call_000000000000000000000022";
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
                    Some("/tmp/example-docs/guides/references/style-guide.md"),
                    None,
                ),
                Some("L1–12"),
            ),
        ),
        (
            "toolu_010000000000000000000019",
            super::tests::read(
                "Read",
                None,
                Some("/tmp/shot-3.png"),
                Some(
                    "File does not exist. Note: your current working directory is /home/user/project",
                ),
            ),
        ),
        (
            image,
            super::tests::read("Viewed", None, Some("/tmp/example-shots/ui1.png"), None),
        ),
        (
            pi,
            with_detail(
                super::tests::read(
                    "Read",
                    None,
                    Some(
                        "/home/user/.local/lib/node_modules/example-cli/\
                         node_modules/example-sdk/dist/api/chat-completions.js",
                    ),
                    None,
                ),
                Some("L346–380"),
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
/// and grep and pi's grep and ls return, with what their adapters read out
/// of them.
#[gpui::test]
fn a_search_reads_as_its_pattern_and_opens_on_what_it_found(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/search.jsonl"), 1_024.);
    let refused = "toolu_010000000000000000000027:toolu_010000000000000000000028";
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
                    ".claude/worktrees/agent-a0000000000000001/crates/app/src/pages/settings/profiles"
                        .to_owned(),
                ),
                ..read(
                    "Searched",
                    None,
                    Some("PROFILES_TAB|DEFAULT_PROFILE_ID|tab\\.set|Import|import_"),
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
/// opening its page, with the harness's whole answer a click away; a
/// search item with no query says only that it searched.
#[gpui::test]
fn a_web_call_reads_as_its_url_or_query(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/web.jsonl"), 1_024.);
    let fetch = "toolu_010000000000000000000029:toolu_010000000000000000000030";
    let search = "toolu_010000000000000000000031:toolu_010000000000000000000032";
    for (id, expected) in [
        (
            fetch,
            read(
                "Fetched",
                None,
                Some("https://docs.example.com/product/kb/end-of-life.html"),
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
                    Some("示例库 中间件 跨域配置 预检 缓存 API 文档"),
                    None,
                )
            },
        ),
        (
            "exec-00000021-0000-0000-0000-000000000021",
            read("Searched the web", None, Some("'font'"), None),
        ),
        (
            "exec-00000022-0000-0000-0000-000000000022",
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
    click(cx, &format!("tool-raw-{search}-toggle"));
    assert!(shows(cx, &format!("tool-text-{search}-output")));
    thread.shutdown(cx);
}

/// A subagent is a window in the shared grammar: the agent's type and its
/// task, the model and effort it ran with as a chip, interrupted said in
/// words.
#[gpui::test]
fn a_subagent_reads_as_its_agent_and_task(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/subagent.jsonl"), 1_024.);
    let claude = "toolu_010000000000000000000036";
    let codex = "call_000000000000000000000037";
    let interrupted = "call_000000000000000000000038";
    assert_eq!(
        thread.header(claude, cx),
        read(
            "Ran subagent",
            Some("Explore"),
            Some("Classify the parser tests"),
            None
        )
    );
    assert_eq!(
        thread.header(codex, cx),
        Read {
            chips: vec![Chip::Tag("gpt-6-astra · low".to_owned())],
            ..read(
                "Ran subagent",
                Some("api_client"),
                Some("/root/api_client"),
                None
            )
        }
    );
    assert_eq!(
        thread.header(interrupted, cx),
        read(
            "Ran subagent",
            Some("cache_limits"),
            Some("/root/perf_review/cache_limits"),
            None
        )
    );
    thread.open_work_log(claude, cx);
    for id in [claude, codex, interrupted] {
        assert!(shows(cx, &format!("tool-argument-{id}")), "{id}");
    }
    assert!(shows(cx, &format!("tool-chip-{codex}")));
    assert!(shows(cx, &format!("tool-status-chip-{interrupted}")));
    for settled in [claude, codex] {
        assert!(
            !shows(cx, &format!("tool-status-chip-{settled}")),
            "{settled}"
        );
    }
    thread.shutdown(cx);
}

/// A running agent says so; a failed one tells its error by the first line
/// of what it reported, under its header. The running
/// row is the recorded Codex update; no failed agent is recorded, so that
/// one takes the shape the adapters give a failure: the error as the
/// summary.
#[gpui::test]
fn a_subagent_window_tells_a_running_and_a_failed_agent(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let running = "call_000000000000000000000037";
    let failed = "spawn-failed";
    let log = [
        serde_json::json!({ "ts": 1_000, "event": { "type": "turn_started", "turn_id": "t" } }),
        serde_json::json!({ "ts": 1_001, "event": { "type": "item_completed", "id": "user",
            "content": { "kind": "user_message", "text": "Delegate it." } } }),
        serde_json::json!({ "ts": 1_788_812_926_728_u64, "event": { "type": "item_updated",
            "id": running, "content": { "kind": "subagent", "agent_type": "api_client",
                "description": "/root/api_client", "status": "in_progress", "summary": null,
                "model": "gpt-6-astra", "effort": "low" } } }),
        serde_json::json!({ "ts": 1_788_812_926_800_u64, "event": { "type": "item_completed",
            "id": failed, "content": { "kind": "subagent", "agent_type": "general",
                "description": "Survey the store", "status": "failed",
                "summary": "ProviderModelNotFoundError: model not found\n  at resolve (session.ts:12)" } } }),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let (thread, cx) = StoredThread::open(cx, &log, 1_024.);
    assert_eq!(
        thread.header(running, cx).verb.as_deref(),
        Some("Running subagent")
    );
    assert_eq!(
        thread.header(failed, cx),
        read(
            "Ran subagent",
            Some("general"),
            Some("Survey the store"),
            Some("ProviderModelNotFoundError: model not found")
        )
    );
    thread.open_work_log(running, cx);
    assert!(shows(cx, &format!("tool-error-{failed}")));
    thread.shutdown(cx);
}

/// A call the turn waits on keeps its spinner; one that returned while the
/// work it started goes on — an agent or a command launched in the
/// background, as Claude Code reports them — says so in the verb's place
/// until that work ends, when it reads as done.
#[gpui::test]
fn a_call_whose_work_goes_on_in_the_background_says_so_until_it_ends(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let waited = "toolu_waited";
    let agent = "toolu_agent";
    let command = "toolu_command";
    let ended = "toolu_ended";
    let subagent = |id: &str, status: &str| {
        serde_json::json!({ "type": "item_updated", "id": id, "content": { "kind": "subagent",
            "agent_type": "general-purpose", "description": "Say OK", "status": status,
            "summary": null } })
    };
    let backgrounded = |id: &str| serde_json::json!({ "type": "item_backgrounded", "item_id": id });
    let log = [
        serde_json::json!({ "type": "turn_started", "turn_id": "t" }),
        serde_json::json!({ "type": "item_completed", "id": "user",
            "content": { "kind": "user_message", "text": "Start them." } }),
    ]
    .iter()
    .enumerate()
    .map(|(index, event)| serde_json::json!({ "ts": 1_000 + index, "event": event }).to_string())
    .collect::<Vec<_>>()
    .join("\n");
    // The work goes on in the background of a running harness: a log reopened
    // with no process to finish it closes it as interrupted.
    let live = [
        subagent(waited, "in_progress"),
        subagent(agent, "in_progress"),
        backgrounded(agent),
        serde_json::json!({ "type": "item_updated", "id": command, "content": {
            "kind": "command_execution", "command": "sleep 8 && echo BGDONE",
            "output": "Command running in background with ID: bhfnwk091.",
            "exit_code": null, "status": "in_progress" } }),
        backgrounded(command),
        subagent(ended, "in_progress"),
        backgrounded(ended),
        subagent(ended, "completed"),
    ]
    .into_iter()
    .map(|event| serde_json::from_value(event).unwrap())
    .collect();
    let (mut thread, cx) = StoredThread::open(cx, &log, 1_024.);
    thread.provider(live, ended, cx);
    let verb = |id: &str, cx: &mut VisualTestContext| thread.header(id, cx).verb;
    assert_eq!(verb(waited, cx).as_deref(), Some("Running subagent"));
    for id in [agent, command] {
        assert_eq!(
            verb(id, cx).as_deref(),
            Some("Running in background"),
            "{id}"
        );
    }
    assert_eq!(verb(ended, cx).as_deref(), Some("Ran subagent"));
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
    step_until(&mut thread, "the whole output", cx, |cx| {
        !shows(cx, "load-output-long")
    });
    click(cx, "tool-text-long-output-more");
    assert!(shows(cx, "tool-text-long-output"));
    thread.shutdown(cx);
}

/// `lines` numbered lines, as a command prints them.
fn numbered(lines: usize) -> String {
    (1..=lines)
        .map(|line| format!("line {line} of the run"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A log of one turn whose command `id` printed `lines` numbered lines, a
/// command execution whose screen the host renders.
fn shell_log(id: &str, lines: usize) -> String {
    let content = serde_json::json!({
        "kind": "command_execution",
        "command": format!("seq 1 {lines}"),
        "output": numbered(lines),
        "exit_code": 0,
        "status": "completed",
    });
    let call = serde_json::json!({
        "ts": 2_000,
        "event": { "type": "item_completed", "id": id, "content": content },
    });
    [
        serde_json::json!({ "ts": 1_000, "event": { "type": "turn_started", "turn_id": "t" } }),
        serde_json::json!({ "ts": 1_001, "event": { "type": "item_completed", "id": "user",
            "content": { "kind": "user_message", "text": "Count." } } }),
        call,
        serde_json::json!({ "ts": 3_000, "event": { "type": "turn_completed", "turn_id": "t",
            "status": "completed", "usage": null } }),
    ]
    .map(|line| line.to_string())
    .join("\n")
}

/// What `id`'s output grid shows, as the reader takes it: dragged across
/// from its first cell to its last and copied.
fn copied_output(id: &str, cx: &mut VisualTestContext) -> String {
    let output = cx
        .debug_bounds(format!("command-output-{id}").leak())
        .expect("the output's grid");
    let from = output.origin + gpui::point(px(1.), px(1.));
    let to = output.bottom_right() - gpui::point(px(1.), px(1.));
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(to, Some(gpui::MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, Modifiers::default());
    draw_settled(cx);
    cx.dispatch_action(gpui_base::input::Copy);
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap_or_default()
}

/// The wheel over `id`'s output, `lines` rows' worth, up into what scrolled
/// off for a positive count.
fn scroll_output(id: &str, lines: f32, cx: &mut VisualTestContext) {
    let output = cx
        .debug_bounds(format!("command-output-{id}").leak())
        .expect("the output's grid");
    cx.simulate_event(gpui::ScrollEvent {
        position: output.center(),
        delta: gpui::ScrollDelta::Lines(gpui::point(0., lines)),
        ..Default::default()
    });
    draw_settled(cx);
}

/// Steps the host until the window drawn at rest shows `what`.
fn step_until(
    thread: &mut StoredThread,
    what: &str,
    cx: &mut VisualTestContext,
    done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    thread.driver.settle_until(cx, what, done);
}

/// A command that printed five thousand lines, its host's screen of them in
/// the window's grid, scrolls back to the first line it printed. The record
/// carried only a preview of them, but the host's screen holds them all, so
/// the window offers nothing to load.
#[gpui::test]
fn a_five_thousand_line_command_scrolls_to_its_first_line(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let log = shell_log("count", 5_000);
    assert!(numbered(5_000).len() > tcode_protocol::OUTPUT_PREVIEW_BYTES);
    let (mut thread, cx) = StoredThread::open(cx, &log, 1_024.);
    thread.open_work_log("count", cx);
    click(cx, "activity-row-count");
    step_until(&mut thread, "the host's screen", cx, |cx| {
        copied_output("count", cx).ends_with("line 5000 of the run")
    });
    assert!(!shows(cx, "load-output-count"), "nothing left to load");
    scroll_output("count", 6_000., cx);
    let start = copied_output("count", cx);
    assert!(
        start.starts_with("line 1 of the run\n"),
        "it scrolls back to the first line: {start}"
    );
    thread.shutdown(cx);
}

/// A command whose record holds a preview of its output copies all of it
/// from its window's Copy output, loading it first.
#[gpui::test]
fn a_shortened_commands_copy_output_copies_whole(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let lines = tcode_protocol::STORED_OUTPUT_HISTORY + 5_000;
    let (mut thread, cx) = StoredThread::open(cx, &shell_log("long", lines), 1_024.);
    thread.open_work_log("long", cx);
    let header = cx.debug_bounds("activity-row-long").expect("the window");
    cx.simulate_mouse_down(
        header.center(),
        gpui::MouseButton::Right,
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        header.center(),
        gpui::MouseButton::Right,
        Modifiers::default(),
    );
    draw_settled(cx);
    click(cx, "menu-item-1");
    let whole = numbered(lines);
    step_until(&mut thread, "the whole output copied", cx, |cx| {
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
            .as_deref()
            == Some(whole.as_str())
    });
    thread.shutdown(cx);
}

fn titled(title: &str, error: Option<&str>) -> Read {
    Read {
        verb: None,
        subject: None,
        title: Some(title.to_owned()),
        argument: None,
        detail: None,
        chips: Vec::new(),
        error: error.map(str::to_owned),
    }
}

/// A computer-use script reads as the title the model gave it, verbatim in
/// the user's language, never as its code; a failure by the server's own
/// line. Opened, it shows the screenshot it got back, what it printed,
/// and its code a click away.
#[gpui::test]
fn a_computer_use_script_reads_as_its_title_and_shows_what_it_got_back(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/script.jsonl"), 1_024.);
    let first = "call_000000000000000000000054";
    let shot = "call_000000000000000000000053";
    let failed = "call_000000000000000000000057";
    for (id, expected) in [
        (first, titled("Open the seeded example desktop app", None)),
        (shot, titled("核对设置页当前内容", None)),
        (
            failed,
            titled(
                "点击画布放置 Button",
                Some("Computer Use server error -10005: noWindowsAvailable"),
            ),
        ),
        (
            "call_000000000000000000000062",
            read("Reset the script kernel", None, None, None),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    let entries = thread.entries(cx);
    let refs: Vec<&TimelineEntry> = entries.iter().map(Arc::as_ref).collect();
    let counts = crate::chat::model::work_log_counts(&refs);
    assert_eq!((counts.computer, counts.tools), (7, 0));

    thread.open_work_log(first, cx);
    click(cx, &format!("activity-row-run-{first}"));
    click(cx, &format!("activity-row-{shot}"));
    assert!(shows(cx, &format!("image-read-{shot}-0")));
    assert!(shows(cx, &format!("tool-text-{shot}-output")));
    let code = format!("tool-code-{shot}");
    assert!(!shows(cx, &code), "the code stays shut until asked for");
    click(cx, &format!("{code}-toggle"));
    assert!(shows(cx, &code));
    thread.shutdown(cx);
}

/// Tcode's own computer-use server reads in the formats it writes: an
/// observation as the app it saw and how many elements, its outline folded
/// to that count and its screenshot; a transaction as its steps with the
/// outcome of each; a wait that timed out says so; a refusal is the
/// server's message, not its JSON.
#[gpui::test]
fn a_desktop_call_reads_as_the_window_it_saw_and_the_steps_it_took(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/desktop.jsonl"), 1_024.);
    let roots = "toolu_010000000000000000000039";
    let observe = "toolu_010000000000000000000041:toolu_010000000000000000000042";
    let codex_observe = "exec-00000028-0000-0000-0000-000000000028";
    let act = "exec-00000030-0000-0000-0000-000000000030";
    let with = |read: Read, detail: Option<&str>, chips: Vec<Chip>| Read {
        detail: detail.map(str::to_owned),
        chips,
        ..read
    };
    let count = |text: &str| vec![Chip::Count(text.to_owned())];
    for (id, expected) in [
        (
            roots,
            with(
                read("Found windows", None, Some("pid 77739"), None),
                None,
                count("0 windows"),
            ),
        ),
        (
            observe,
            with(
                read("Observed", None, Some("example-app"), None),
                Some("Example App"),
                count("6 elements"),
            ),
        ),
        (
            codex_observe,
            with(
                read("Observed", None, Some("example-app"), None),
                None,
                count("6 elements"),
            ),
        ),
        (
            "toolu_010000000000000000000044",
            with(
                read("Searched", None, Some("终端"), None),
                None,
                count("0 matches"),
            ),
        ),
        (
            "toolu_010000000000000000000047",
            read("Moved the pointer", None, Some("(220, 250)"), None),
        ),
        (act, read("Clicked", None, Some("(625, 432)"), None)),
        (
            "toolu_010000000000000000000049",
            with(
                read("Waited for", None, Some("\"Threads\""), None),
                None,
                vec![Chip::Warning("timed out".to_owned())],
            ),
        ),
        (
            "toolu_010000000000000000000051",
            read(
                "Observed",
                None,
                None,
                Some("no AX window matched root @r20"),
            ),
        ),
        (
            "toolu_010000000000000000000052",
            read(
                "Read",
                None,
                None,
                Some("failed to deserialize parameters: missing field `ref`"),
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(roots, cx);
    click(cx, &format!("activity-row-run-{roots}"));
    click(cx, &format!("activity-row-{observe}"));
    assert!(shows(cx, &format!("image-read-{observe}-0")));
    let tree = format!("tool-tree-{observe}");
    assert!(!shows(cx, &tree), "the outline stays folded to its count");
    click(cx, &format!("{tree}-toggle"));
    for row in 0..6 {
        assert!(shows(cx, &format!("{tree}-{row}")), "element {row}");
    }
    assert!(!shows(cx, &format!("{tree}-6")));
    // The text the outline was read from is not printed again, but stays
    // one click away.
    let raw = format!("tool-text-{observe}-output");
    assert!(!shows(cx, &raw));
    click(cx, &format!("tool-raw-{observe}-toggle"));
    assert!(shows(cx, &raw));
    click(cx, &format!("activity-row-{observe}"));
    // With nothing to read a structure from, the text shows open.
    click(cx, &format!("activity-row-{roots}"));
    assert!(shows(cx, &format!("tool-text-{roots}-output")));
    assert!(!shows(cx, &format!("tool-raw-{roots}-toggle")));
    click(cx, &format!("activity-row-{roots}"));
    // A transaction lists its one step; its successor diff is an outline
    // of the element that changed.
    click(cx, &format!("activity-row-{act}"));
    assert!(shows(cx, &format!("tool-step-{act}-0")));
    assert!(!shows(cx, &format!("tool-step-{act}-1")));
    click(cx, &format!("tool-tree-{act}-toggle"));
    assert!(shows(cx, &format!("tool-tree-{act}-0")));
    assert!(!shows(cx, &format!("tool-tree-{act}-1")));
    click(cx, &format!("activity-row-{act}"));
    // A read shows the page it read; the facts the server put before it
    // (where to continue) stay one click away.
    let read_text = "toolu_010000000000000000000050";
    click(cx, &format!("activity-row-{read_text}"));
    assert!(shows(cx, &format!("tool-text-{read_text}-page")));
    click(cx, &format!("tool-raw-{read_text}-toggle"));
    assert!(shows(cx, &format!("tool-text-{read_text}-output")));
    thread.shutdown(cx);
}

/// The preview browser reads as what the agent did to the page: the URL it
/// opened, the selector it clicked, the text it typed and where, the keys,
/// the scroll, the page a snapshot read and how many elements it found;
/// a screenshot opens on the picture, a failed one on the browser's words.
#[gpui::test]
fn a_preview_call_reads_as_what_it_did_to_the_page(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/preview.jsonl"), 1_024.);
    let status = "toolu_010000000000000000000063:toolu_010000000000000000000064";
    let snapshot = "exec-00000043-0000-0000-0000-000000000043";
    let evaluate = "toolu_010000000000000000000063:toolu_010000000000000000000075";
    let screenshot = "toolu_010000000000000000000076";
    for (id, expected) in [
        (
            status,
            read("Checked the page", None, Some("about:blank"), None),
        ),
        (
            "toolu_010000000000000000000065",
            read("Opened", None, Some("http://localhost:3110/"), None),
        ),
        (
            snapshot,
            Read {
                chips: vec![Chip::Count("12 elements".to_owned())],
                ..read(
                    "Read the page",
                    None,
                    Some("登录 - Example - 示例应用演示站点，仅用于测试登录流程！"),
                    None,
                )
            },
        ),
        (
            "toolu_010000000000000000000068",
            read("Clicked", None, Some("header button"), None),
        ),
        (
            "toolu_010000000000000000000069",
            Read {
                detail: Some("#auth input".to_owned()),
                ..read("Typed", None, Some("\"testpass123\""), None)
            },
        ),
        (
            "toolu_010000000000000000000070",
            read("Pressed keys", None, Some("Escape"), None),
        ),
        (
            "toolu_010000000000000000000071",
            read("Scrolled", None, Some("y+1500"), None),
        ),
        (screenshot, read("Took a screenshot", None, None, None)),
        (
            "exec-0000004e-0000-0000-0000-00000000004e",
            read(
                "Took a screenshot",
                None,
                None,
                Some("preview browser has no visible area"),
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(status, cx);
    click(cx, &format!("activity-row-run-{status}"));
    click(cx, &format!("activity-row-{screenshot}"));
    assert!(shows(cx, &format!("image-read-{screenshot}-0")));
    click(cx, &format!("activity-row-{snapshot}"));
    click(cx, &format!("tool-tree-{snapshot}-toggle"));
    for row in 0..12 {
        assert!(shows(cx, &format!("tool-tree-{snapshot}-{row}")), "{row}");
    }
    let raw = format!("tool-text-{snapshot}-output");
    assert!(!shows(cx, &raw), "the snapshot's JSON stays folded");
    click(cx, &format!("tool-raw-{snapshot}-toggle"));
    assert!(shows(cx, &raw));
    click(cx, &format!("activity-row-{evaluate}"));
    assert!(shows(cx, &format!("tool-text-{evaluate}-output")));
    assert!(shows(cx, &format!("tool-code-{evaluate}-toggle")));
    thread.shutdown(cx);
}

/// Consecutive computer calls, with the thoughts between them, are one
/// window, "Used computer · N actions", telling the latest step and keeping
/// the latest screenshot in view; a command between two runs keeps them
/// apart, and a thought after the last call stays out. Opened, it holds
/// each call's own window under the id it has alone, so a window keeps its
/// identity as its run grows.
#[gpui::test]
fn consecutive_computer_calls_share_one_window_with_the_latest_screenshot(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/run.jsonl"), 1_024.);
    let observe = "toolu_010000000000000000000041:toolu_010000000000000000000042";
    let act = "toolu_010000000000000000000047";
    let script = "call_000000000000000000000053";

    let entries = thread.entries(cx);
    let refs: Vec<&TimelineEntry> = entries.iter().map(Arc::as_ref).collect();
    let counts = crate::chat::model::work_log_counts(&refs);
    assert_eq!((counts.computer, counts.commands), (3, 1));

    thread.open_work_log(observe, cx);
    let run = format!("activity-row-run-{observe}");
    assert!(shows(cx, &run));
    assert!(shows(cx, &format!("activity-row-run-{script}")));
    assert!(shows(cx, "activity-row-thought-after"));
    assert!(shows(cx, "activity-row-command-between"));
    // Shut, the run tells its latest step and shows the observation's
    // screenshot, the latest it got.
    assert!(shows(cx, &format!("image-read-{observe}-0")));
    assert!(!shows(cx, &format!("activity-row-{act}")));
    click(cx, &run);
    for inside in [observe, "thought-between", act] {
        assert!(shows(cx, &format!("activity-row-{inside}")), "{inside}");
    }
    thread.shutdown(cx);
}

/// A call about Tcode's threads reads as what it did to which thread, by
/// the title the call gave it or the head of its id, and the entries it
/// names: a refusal by its own words. Opened, every thread and entry it
/// names is a link.
#[gpui::test]
fn a_thread_call_reads_as_what_it_did_to_which_thread(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/threads.jsonl"), 1_024.);
    let outline = "toolu_010000000000000000000079:toolu_010000000000000000000080";
    let notify = "toolu_010000000000000000000100";
    let with = |read: Read, detail: Option<&str>, chips: Vec<Chip>| Read {
        detail: detail.map(str::to_owned),
        chips,
        ..read
    };
    for (id, expected) in [
        (
            outline,
            read("Outlined", None, Some("打包 Go 与 Rust SDK 发布包"), None),
        ),
        (
            "toolu_010000000000000000000082",
            read("Read", None, Some("00000053 #1855–#1903"), None),
        ),
        (
            "toolu_010000000000000000000084",
            with(
                read("Searched", None, Some("抽象"), None),
                Some("00000055"),
                vec![Chip::Count("0 hits".to_owned())],
            ),
        ),
        (
            "toolu_010000000000000000000086",
            read(
                "Searched",
                None,
                Some("心智"),
                Some("refused: scope host is wider than your grant's project"),
            ),
        ),
        (
            "toolu_010000000000000000000087",
            with(
                read("Searched", None, Some("git_commit"), None),
                Some("00000058 #33670"),
                Vec::new(),
            ),
        ),
        (
            "toolu_010000000000000000000092",
            with(
                read("Listed threads", None, Some("多窗格"), None),
                None,
                vec![Chip::Count("1 thread".to_owned())],
            ),
        ),
        (
            "toolu_010000000000000000000095",
            with(
                read(
                    "Started a thread",
                    None,
                    Some("工作包 8：设置页与导出"),
                    None,
                ),
                Some("claude · claude-fable-5-1"),
                Vec::new(),
            ),
        ),
        (
            "toolu_010000000000000000000098",
            read("Updated threads", None, Some("interrupt 00000063"), None),
        ),
        (
            notify,
            read("Set a watch on", None, Some("00000053, 00000065"), None),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }

    thread.open_work_log(outline, cx);
    click(cx, &format!("activity-row-{notify}"));
    for link in 0..2 {
        assert!(shows(cx, &format!("tool-thread-{notify}-{link}")), "{link}");
    }
    assert!(!shows(cx, &format!("tool-thread-{notify}-2")));
    thread.shutdown(cx);
}

/// The harness's calls about the session read as what they did: the tools
/// a search loaded, the skill launched, a question asked and the answer it
/// got, the reviewer's verdict with its
/// risk and, when it denied, its rationale. Codex's wait and generated image
/// read as such, and the image it viewed and the one it generated show as
/// their images.
#[gpui::test]
fn a_harness_call_reads_as_what_it_did_to_the_session(cx: &mut TestAppContext) {
    let _locale = crate::settings::TestLocaleGuard::acquire();
    crate::set_locale("en");
    let (thread, cx) = StoredThread::open(cx, include_str!("fixtures/harness.jsonl"), 1_024.);
    let review = "0000006b-0000-0000-0000-00000000006b";
    let ask = "toolu_010000000000000000000112";
    let codex_ask = "call_000000000000000000000113";
    let viewed = "exec-00000073-0000-0000-0000-000000000073";
    let generated = "exec-00000074-0000-0000-0000-000000000074";
    let rationale = "该脚本通过页面脚本直接改写表单状态，违反用户明确要求只用可见控件完成操作、\
                     不得绕过页面校验的限制；应改用点击和输入重新操作。";
    for (id, expected) in [
        (
            review,
            Read {
                chips: vec![Chip::Count("Risk: low".to_owned())],
                ..read(
                    "Denied",
                    None,
                    Some("MCP tool: tcode_preview/preview_evaluate"),
                    Some(rationale),
                )
            },
        ),
        (
            "toolu_010000000000000000000109",
            Read {
                chips: vec![Chip::Count("1 tool".to_owned())],
                ..read("Loaded tools", None, Some("select:Monitor"), None)
            },
        ),
        (
            "toolu_010000000000000000000110",
            read("Launched skill", None, Some("pr"), None),
        ),
        (
            ask,
            read(
                "Asked",
                None,
                Some("Which release scope should this publish use?"),
                None,
            ),
        ),
        (
            "call_000000000000000000000114",
            read("Waited", None, Some("20.0s"), None),
        ),
        (
            generated,
            read(
                "Generated an image",
                None,
                Some(
                    "Edit the reference image: keep the subject, make the background transparent.",
                ),
                None,
            ),
        ),
    ] {
        assert_eq!(thread.header(id, cx), expected, "{id}");
    }
    for id in [viewed, generated] {
        assert!(shows(cx, &format!("image-read-{id}-0")), "{id}");
    }

    thread.open_work_log(review, cx);
    assert!(shows(cx, &format!("tool-error-{review}")));
    click(cx, "activity-row-toolu_010000000000000000000109");
    assert!(shows(cx, "tool-loaded-toolu_010000000000000000000109-0"));
    for id in [ask, codex_ask] {
        click(cx, &format!("activity-row-{id}"));
        assert!(shows(cx, &format!("tool-question-{id}-0")), "{id}");
    }
    thread.shutdown(cx);
}
