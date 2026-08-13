use std::borrow::Cow;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{FileChange, FileChangeKind, ItemContent, ItemStatus, TurnStatus};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Task, Window,
    WindowBounds, WindowOptions, div, px, size,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_core::session::{EntryContent, OrchestrateCallback, TimelineEntry};
use tcode_ui::overlay::OverlayHost;
use tcode_ui::theme::{self, ActiveTheme as _};
use tcode_ui::{assets, gallery_support, markdown::MarkdownState};

const ASSISTANT_MARKDOWN: &str = r#"Here is a compact result with **real markdown rendering** and a link to [the guide](https://example.com).

```rust
fn soft_wrapping_example(request: &str) -> Result<String, GalleryError> { process_request_without_truncating_the_important_context(request) }
```

| State | Result | Duration |
| --- | ---: | ---: |
| Parsed | 24 files | 1.2s |
| Verified | 238 tests | 4.8s |
"#;

struct Gallery {
    markdown: Entity<MarkdownState>,
    started_at: u64,
    _tick: Task<()>,
}

impl Gallery {
    fn new(cx: &mut Context<Self>) -> Self {
        let markdown = cx.new(|cx| MarkdownState::new(ASSISTANT_MARKDOWN, cx));
        let started_at = now_millis().saturating_sub(7_300);
        let tick = cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(Duration::from_millis(100)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        Self {
            markdown,
            started_at,
            _tick: tick,
        }
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let command_collapsed = entry(
            "gallery-s03-activity-command-collapsed",
            ItemContent::CommandExecution {
                command: "cargo clippy --workspace --all-targets -- -D warnings".into(),
                output: (1..=26)
                    .map(|line| format!("checking crate {line:02} ... ok"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                exit_code: Some(0),
                status: ItemStatus::Completed,
            },
        );
        let mut command_expanded = command_collapsed.clone();
        command_expanded.id = "gallery-s03-activity-command-expanded".into();
        let tool = entry(
            "gallery-s03-activity-tool-call",
            ItemContent::ToolCall {
                name: "read_file".into(),
                input: serde_json::json!({"path": "crates/ui/src/chat/components/activity.rs"}),
                output: Some("Loaded 341 lines".into()),
                status: ItemStatus::Completed,
            },
        );
        let thinking = entry(
            "gallery-s03-activity-settled-thinking",
            ItemContent::Reasoning {
                text: "Compared the component API with the gallery fixtures and verified that the catalog only exercises public rendering inputs.".into(),
            },
        );
        let subagents = [
            subagent_entry("gallery-s05-subagent-running", ItemStatus::InProgress, None),
            subagent_entry(
                "gallery-s05-subagent-completed",
                ItemStatus::Completed,
                Some("Mapped all component states"),
            ),
            subagent_entry(
                "gallery-s05-subagent-failed",
                ItemStatus::Failed,
                Some("Fixture validation failed"),
            ),
        ];
        let two_files = file_changes(2);
        let five_files = file_changes(5);
        let callback = OrchestrateCallback {
            child_id: "child-gallery".into(),
            title: "Audit the extracted chat components".into(),
            state: "completed".into(),
            body: "Checked the activity, work-log, and disclosure render paths.\nAll fixtures are isolated from live session state.".into(),
        };

        let sections = vec![
            section(
                1,
                "Working indicator",
                state_row(vec![state_card(
                    "ANIMATING",
                    gallery_support::working_indicator(
                        "gallery-s01-working-animating",
                        self.started_at,
                        cx,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                2,
                "User bubble",
                state_stack(vec![
                    state_card_full(
                        "SHORT",
                        gallery_support::user_bubble(
                            "gallery-s02-user-short",
                            "Ship the gallery.",
                            false,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                    state_card_full(
                        "LONG WRAPPING",
                        gallery_support::user_bubble(
                            "gallery-s02-user-long-wrapping",
                            "Please verify every extracted component against the redesign spec and keep this intentionally long message wrapping naturally inside its bubble.",
                            false,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                    state_card_full(
                        "PENDING STEER",
                        gallery_support::user_bubble(
                            "gallery-s02-user-pending-steer",
                            "Also include the failure states.",
                            true,
                            cwd,
                            window,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                3,
                "Activity rows",
                state_stack(vec![
                    state_card_full(
                        "COMMAND · COLLAPSED",
                        gallery_support::activity(&command_collapsed, false, cx),
                        cx,
                    ),
                    state_card_full(
                        "COMMAND · EXPANDED + OUTPUT TAIL",
                        gallery_support::activity(&command_expanded, true, cx),
                        cx,
                    ),
                    state_card_full("TOOL CALL", gallery_support::activity(&tool, false, cx), cx),
                    state_card_full(
                        "SETTLED THINKING",
                        gallery_support::activity(&thinking, false, cx),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                4,
                "Work-log capsule",
                state_row(vec![
                    state_card(
                        "SETTLED · COUNTS + DURATION",
                        gallery_support::work_log(
                            "gallery-s04-worklog-settled",
                            401,
                            "2 tools · 3 edits · 1 command",
                            "18.6s",
                            TurnStatus::Completed,
                            cx,
                        ),
                        cx,
                    ),
                    state_card(
                        "TURN FAILED",
                        gallery_support::work_log(
                            "gallery-s04-worklog-failed",
                            402,
                            "1 tool · 1 command",
                            "4.2s",
                            TurnStatus::Failed,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                5,
                "Subagent capsule",
                state_row(
                    subagents
                        .iter()
                        .zip(["RUNNING", "COMPLETED", "FAILED"])
                        .map(|(entry, label)| {
                            state_card(label, gallery_support::subagent(entry, cx), cx)
                        })
                        .collect(),
                ),
                cx,
            ),
            section(
                6,
                "Changed-files chips",
                state_row(vec![
                    state_card(
                        "2 FILES · FLAT",
                        gallery_support::changed_files(601, cwd, &two_files, cx),
                        cx,
                    ),
                    state_card(
                        "5 FILES · +2 MORE",
                        gallery_support::changed_files(602, cwd, &five_files, cx),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                7,
                "Assistant block",
                state_row(vec![state_card(
                    "PROSE · SOFT-WRAPPING CODE · TABLE",
                    gallery_support::assistant(
                        "gallery-s07-assistant-markdown",
                        self.markdown.clone(),
                        cwd,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                8,
                "Error card",
                state_row(vec![state_card(
                    "MULTI-LINE ERROR",
                    gallery_support::error_card(
                        "gallery-s08-error-multiline",
                        "Provider request failed after three attempts.\nThe upstream connection closed before the response body completed.\nRetry the turn or select another model.",
                        cx,
                    ),
                    cx,
                )]),
                cx,
            ),
            section(
                9,
                "Dividers",
                state_row(vec![
                    state_card(
                        "RELAY",
                        gallery_support::relay_divider("gallery-s09-divider-relay", cx),
                        cx,
                    ),
                    state_card(
                        "MODEL CHANGE",
                        gallery_support::model_change_divider(
                            "gallery-s09-divider-model-change",
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
            section(
                10,
                "Disclosure",
                state_row(vec![
                    state_card(
                        "COLLAPSED CALLBACK",
                        gallery_support::callback(
                            "gallery-s10-disclosure-collapsed",
                            &callback,
                            false,
                            cx,
                        ),
                        cx,
                    ),
                    state_card(
                        "EXPANDED CALLBACK",
                        gallery_support::callback(
                            "gallery-s10-disclosure-expanded",
                            &callback,
                            true,
                            cx,
                        ),
                        cx,
                    ),
                ]),
                cx,
            ),
        ];

        div()
            .id("gallery-scroll")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex().w_full().justify_center().child(
                    v_flex()
                        .w(px(720.))
                        .max_w_full()
                        .gap(px(56.))
                        .px_5()
                        .pt(px(64.))
                        .pb(px(96.))
                        .child(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(28.))
                                        .font_semibold()
                                        .child("Chat component gallery"),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Real extracted GPUI components rendered with deterministic fixture data."),
                                ),
                        )
                        .children(sections),
                ),
            )
    }
}

fn entry(id: &str, content: ItemContent) -> TimelineEntry {
    TimelineEntry {
        id: id.into(),
        content: EntryContent::Item(content),
        ts: None,
        turn: 0,
    }
}

fn subagent_entry(id: &str, status: ItemStatus, summary: Option<&str>) -> TimelineEntry {
    entry(
        id,
        ItemContent::Subagent {
            agent_type: "explorer".into(),
            description: "Review the extracted component fixtures".into(),
            status,
            summary: summary.map(str::to_string),
        },
    )
}

fn file_changes(count: usize) -> Vec<FileChange> {
    let paths = [
        "crates/ui/src/chat/components/activity.rs",
        "crates/ui/src/chat/components/assistant.rs",
        "crates/ui/src/chat/components/bubble.rs",
        "crates/ui/src/chat/components/disclosure.rs",
        "crates/ui/examples/gallery.rs",
    ];
    paths[..count]
        .iter()
        .enumerate()
        .map(|(index, path)| FileChange {
            path: (*path).into(),
            kind: FileChangeKind::Modify,
            diff: Some(format!(
                "@@ -1,2 +1,{} @@\n old line\n+new line\n+fixture {}\n",
                index + 3,
                index + 1
            )),
        })
        .collect()
}

fn section(number: usize, title: &str, content: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(format!("{number:02}")),
                )
                .child(div().h(px(1.)).w_5().bg(cx.theme().border))
                .child(gallery_support::section_label(title)),
        )
        .child(content)
        .into_any_element()
}

fn state_row(cards: Vec<AnyElement>) -> AnyElement {
    h_flex()
        .w_full()
        .items_stretch()
        .gap_3()
        .flex_wrap()
        .children(cards)
        .into_any_element()
}

fn state_stack(cards: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .items_start()
        .gap_3()
        .children(cards)
        .into_any_element()
}

fn state_card(label: &str, content: AnyElement, cx: &App) -> AnyElement {
    v_flex()
        .min_w(px(210.))
        .flex_1()
        .gap_3()
        .p_4()
        .rounded(px(14.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.28))
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(gallery_support::section_label(label)),
        )
        .child(content)
        .into_any_element()
}

fn state_card_full(label: &str, content: AnyElement, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .gap_3()
        .p_4()
        .rounded(px(14.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted.opacity(0.28))
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(gallery_support::section_label(label)),
        )
        .child(content)
        .into_any_element()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn main() {
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(|cx| {
            tcode_ui::markdown::init(cx);
            tcode_ui::settings::apply_locale(Some(tcode_ui::LANGUAGE_ENGLISH));

            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let fonts: Vec<Cow<'static, [u8]>> = vec![
                Cow::Borrowed(assets::DM_SANS),
                Cow::Borrowed(assets::LILEX_REGULAR),
                Cow::Borrowed(assets::LILEX_BOLD),
                Cow::Borrowed(assets::LILEX_ITALIC),
                Cow::Borrowed(assets::LILEX_BOLD_ITALIC),
            ];
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let fonts: Vec<Cow<'static, [u8]>> = vec![Cow::Borrowed(assets::DM_SANS)];
            cx.text_system()
                .add_fonts(fonts)
                .expect("failed to register gallery fonts");

            theme::init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(980.), px(820.)), cx)),
                window_min_size: Some(size(px(760.), px(600.))),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let gallery = cx.new(Gallery::new);
                cx.new(|cx| OverlayHost::new(gallery, window, cx))
            })
            .expect("failed to open component gallery window");
        });
}
