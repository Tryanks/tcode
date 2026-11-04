use super::*;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

impl ChatView {
    /// The phone's "new thread" empty state: which project the thread
    /// starts in, and which provider/model the first message reaches.
    pub(super) fn render_compact_draft_empty(
        &self,
        cwd: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let project = store
            .projects()
            .into_iter()
            .find(|project| project.root == cwd)
            .map(|project| project.name)
            .unwrap_or_else(|| {
                cwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let composer = store.composer_state_for(self.session_id.as_deref());
        let model = composer
            .active_model
            .as_ref()
            .map(|active| {
                let name = composer
                    .active_model_spec
                    .as_ref()
                    .map(|spec| spec.display_name.clone())
                    .or_else(|| active.model.clone())
                    .unwrap_or_default();
                let provider = crate::settings::provider_label(active.provider);
                if name.is_empty() {
                    provider.to_string()
                } else {
                    format!("{provider} · {name}")
                }
            })
            .unwrap_or_default();
        crate::material::empty_state(
            Icon::empty().path("icons/message-square.svg"),
            crate::tr!("mobile.draft_empty_title", project = project),
            crate::tr!("mobile.draft_empty_body", model = model),
            cx,
        )
        .into_any_element()
    }

    pub(super) fn render_empty_state(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A phone reaches this state when the host has no threads. Project
        // creation and desktop shortcut hints belong to the desktop launcher.
        if !self.workspace_store.read(cx).scope().is_full() {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("member.empty_title"),
                crate::tr!("member.empty_description"),
                cx,
            )
            .into_any_element();
        }
        if self.window_state.read(cx).compact {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("mobile.projects_empty"),
                crate::tr!("mobile.projects_help"),
                cx,
            )
            .into_any_element();
        }
        let projects = self.workspace_store.read(cx).projects();
        let sessions = self.workspace_store.read(cx).sidebar_sessions();
        let hub_projects = start_hub_projects(&projects, &sessions);
        let add_project = Button::new("add-project-empty")
            .motion(&self.motion)
            .ghost()
            .small()
            .icon(
                Icon::empty()
                    .path("icons/folder-plus.svg")
                    .text_color(cx.theme().muted_foreground),
            )
            .label(crate::tr!("sidebar.add_project"))
            .on_click(cx.listener(|this, _, window, cx| {
                crate::add_project_dialog::open(this.workspace_store.clone(), window, cx);
            }));

        if projects.is_empty() {
            return crate::material::empty_state(
                Icon::new(IconName::Folder),
                crate::tr!("chat.no_projects_title"),
                crate::tr!("chat.no_projects_description"),
                cx,
            )
            .child(
                h_flex().gap_2().child(add_project).child(
                    Button::new("quick-chat-empty")
                        .motion(&self.motion)
                        .small()
                        .label(crate::tr!("sidebar.quick_chat"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.workspace_store
                                .update(cx, |store, cx| store.start_scratch_draft(cx));
                        })),
                ),
            )
            .into_any_element();
        }
        let mut content = v_flex()
            .w_full()
            .max_w(px(420.))
            .px_4()
            .items_center()
            .gap_3()
            .child(div().text_title(cx).child(crate::tr!("chat.empty_title")));
        let launcher = v_flex().w_full().gap_1().child(
            div()
                .px_3()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(crate::material::tracked_uppercase(
                    crate::tr!("chat.start_hub_title").as_ref(),
                )),
        );
        let fills = self.motion.scope("start-hub-fill");
        // A project that comes or goes folds in or out where it stands, and
        // the others glide to their new places as its last activity moves it.
        let rows = crate::motion::keyed::keyed_owned(
            &self.motion.clone(),
            "start-hub-projects",
            crate::motion::keyed::Fold::Down,
            hub_projects
                .into_iter()
                .map(|row| (SharedString::from(row.0.id.clone()), row)),
            ROWS,
            window,
            cx,
            |_, (project, last_activity), _, cx| {
                let project_id = project.id.clone();
                let cwd = project.root.clone();
                let row_label =
                    crate::tr!("sidebar.project", name = project.name.clone()).into_owned();
                crate::material::accessible_clickable(
                    crate::material::selectable_row_owned(
                        h_flex(),
                        &fills,
                        project.id.clone(),
                        false,
                        false,
                        cx,
                    ),
                    SharedString::from(format!("start-hub-project-{}", project.id)),
                    Role::Button,
                    row_label,
                    cx,
                )
                .debug_selector({
                    let id = project.id.clone();
                    move || format!("start-hub-project-{id}")
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace_store.update(cx, |store, cx| {
                        store.start_draft(project_id.clone(), cwd.clone(), cx);
                    });
                }))
                .child(crate::project_icon::artwork(project, 16.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(false, cx)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(project.name.clone())),
                )
                .when_some(*last_activity, |row, last_activity| {
                    row.child(div().flex_none().text_secondary(false, cx).child(
                        crate::time::humanize_ago(now_secs().saturating_sub(last_activity)),
                    ))
                })
                .into_any_element()
            },
        )
        .render(launcher);
        content = content.child(rows).child(
            h_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .child(add_project)
                .child(div().text_secondary(false, cx).child(crate::tr!(
                    "chat.palette_hint",
                    shortcut = format_secondary_shortcut("k", cx)
                ))),
        );

        v_flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .child(content)
            .into_any_element()
    }

    /// The jump to the latest row, rising in while the reader is away from
    /// it and sinking out once they are back.
    pub(super) fn render_scroll_pill(
        &self,
        shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let shown =
            self.motion
                .shown("scroll-to-end-pill", shown.then_some(()), SHOW, window, cx)?;
        // Absolute positioning ignores mx_auto without pinned horizontal
        // insets; span the region and center with flex instead, clear of
        // the side rail as the column is.
        let pill = floating_backing(crate::material::radius_button(cx), cx).child(
            Button::new("scroll-to-end")
                .motion(&self.motion)
                .debug_selector(|| "scroll-to-end".into())
                .outline()
                .small()
                .icon(IconName::ChevronDown)
                .label(crate::tr!("chat.scroll_end"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.list_scroll.scroll_to(
                        &this.list_state,
                        crate::motion::ScrollTarget::End,
                        cx,
                    );
                    cx.notify();
                })),
        );
        Some(
            div()
                .absolute()
                .bottom(px(12.) + self.phone_strip_height)
                .left_0()
                .right(self.body.shift())
                .flex()
                .justify_center()
                .child(shown.fade(pill))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui::{AppContext as _, Bounds, Pixels, TestAppContext, VisualTestContext, px, size};
    use tcode_core::project::Project;
    use tcode_protocol::{
        EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
    };

    use crate::chat::ChatView;
    use crate::store::WorkspaceStore;
    use crate::test_frames::{draw, draw_settled};
    use crate::window_state::WindowState;

    /// The start hub's projects follow the host's: one removed folds away
    /// where it stood, drawn as it was, while the one below it glides up
    /// into its place, and one added folds in and pushes it back down, over
    /// several frames; with motion reduced each change lands at once.
    #[gpui::test]
    fn start_hub_projects_fold_in_and_out_as_the_host_changes_them(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let project = |name: &str| {
            let mut project = Project::from_root(PathBuf::from(format!("/{name}")));
            project.id = name.into();
            project.name = name.into();
            project
        };
        let index = |names: &[&str]| {
            encode_line(&HostMessage::Event(EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions: Vec::new(),
                    projects: names.iter().map(|name| project(name)).collect(),
                }),
            }))
            .unwrap()
        };
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(Default::default()),
                }))
                .unwrap(),
            )
            .unwrap();
        incoming
            .try_send(index(&["alpha", "beta", "gamma"]))
            .unwrap();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let (_, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store.clone(), window_state, None, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let row = |name: &str, cx: &mut VisualTestContext| -> Option<Bounds<Pixels>> {
            cx.debug_bounds(format!("start-hub-project-{name}").leak())
        };
        assert!(row("gamma", cx).is_some(), "the hub lists the projects");

        for reduce in [false, true] {
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            for (names, beta_stays) in [
                (&["alpha", "gamma"][..], false),
                (&["alpha", "beta", "gamma"][..], true),
            ] {
                incoming.try_send(index(names)).unwrap();
                cx.run_until_parked();
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                draw(cx);
                let mut frames = Vec::new();
                for _ in 0..20 {
                    frames.push((
                        row("beta", cx).is_some(),
                        row("gamma", cx).expect("gamma stays").top(),
                    ));
                    cx.executor().advance_clock(Duration::from_millis(16));
                    cx.update(|window, cx| window.simulate_next_frame(cx));
                    draw(cx);
                }
                draw_settled(cx);
                assert_eq!(row("beta", cx).is_some(), beta_stays);
                let rest = row("gamma", cx).unwrap().top();
                let tops: Vec<_> = frames.iter().map(|(_, top)| *top).collect();
                // Frames beta was drawn on: leaving, it is drawn as it folds.
                let drawn = frames.iter().filter(|(beta, _)| *beta).count();
                let between = tops
                    .iter()
                    .filter(|top| **top != tops[0] && **top != rest)
                    .count();
                if reduce {
                    assert_eq!(tops[0], rest, "gamma glided with motion reduced: {tops:?}");
                    assert!(beta_stays || drawn == 0, "beta lingered");
                } else {
                    assert!(between >= 3, "gamma jumped: {tops:?}");
                    assert!(beta_stays || drawn >= 3, "beta went at once");
                }
            }
        }
    }
}
