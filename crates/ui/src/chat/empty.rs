use super::*;

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

impl ChatView {
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
        let hub_projects = start_hub_projects(&projects, sessions);
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
                    if crate::shell::several_hosts(cx) {
                        crate::tr!(
                            "chat.start_hub_title_on",
                            host = self.workspace_store.read(cx).host_title()
                        )
                    } else {
                        crate::tr!("chat.start_hub_title")
                    }
                    .as_ref(),
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
                .child(crate::project_icon::artwork(
                    self.workspace_store.read(cx).image_host(),
                    project,
                    16.,
                ))
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
                    row.child(
                        div().flex_none().text_secondary(false, cx).child(
                            crate::widgets::fade_text(crate::time::humanize_ago(
                                self.workspace_store
                                    .read(cx)
                                    .host_now_secs(cx)
                                    .saturating_sub(last_activity),
                            )),
                        ),
                    )
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
}

/// How many of its threads the new-thread page offers to pick up again.
const RECENT_THREADS: usize = 3;

impl ChatView {
    /// The project this view's thread is in: one of the host's projects, or
    /// the one its quick threads share.
    pub(super) fn location(&self, cx: &App) -> Option<tcode_core::project::Project> {
        let store = self.workspace_store.read(cx);
        let id = self.session_id.as_deref()?;
        let project_id = store
            .session_status_for(Some(id), cx)
            .and_then(|status| status.project_id.clone())
            .or_else(|| store.thread_meta(id)?.project_id.clone())?;
        store.project(&project_id).cloned()
    }

    /// Open the picker of where a new thread starts, this view's place marked.
    pub(super) fn open_project_picker(&self, cx: &mut Context<Self>) {
        let from = crate::window_state::PickerFrom {
            host: self.workspace_store.read(cx).host_key(),
            project_id: self
                .location(cx)
                .filter(|project| !project.quick_chat)
                .map(|project| project.id),
        };
        self.window_state
            .update(cx, |state, cx| state.open_project_picker(from, cx));
    }

    /// The slogan's seed while the new-thread page shows, picked as it comes in.
    pub(super) fn slogan_seed(&mut self) -> u64 {
        *self.slogan_seed.get_or_insert_with(|| {
            use std::hash::{BuildHasher as _, Hasher as _};
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        })
    }

    /// A fresh draft's page: a slogan with the chip of where the thread
    /// starts, the composer, and the place's latest threads to pick up
    /// again, as one block in the middle of the body.
    pub(super) fn render_new_thread_page(
        &mut self,
        composer: AnyElement,
        shift: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let seed = self.slogan_seed();
        let project = self.location(cx);
        let image_host = self.workspace_store.read(cx).image_host();
        let muted = cx.theme().muted_foreground;
        let slogan = &cx.theme().tokens.typography.xl;
        let words = |text: &str| {
            div()
                .text_token(slogan)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(crate::widgets::fade_text(text.trim().to_owned()))
        };
        // Where the thread starts, as the chip that opens the picker.
        let picker = |id: &'static str, lead: AnyElement, label: AnyElement| {
            Button::new(id)
                .motion(&self.motion)
                .debug_selector(move || id.into())
                .ghost()
                .outline()
                .chip()
                .aria_label(crate::tr!("chat.choose_project"))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .child(lead)
                        .child(label)
                        .child(Icon::new(IconName::ChevronDown).size_3().text_color(muted)),
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_project_picker(cx)))
        };

        // The slogan, one of the place's kind; a project's has the chip
        // where its language puts the name, a quick thread's under it.
        let title = h_flex()
            .flex_wrap()
            .justify_center()
            .items_center()
            .gap_2()
            .debug_selector(|| "new-thread-title".into());
        let hero = v_flex()
            .w_full()
            .items_center()
            .gap_3()
            .pb_5()
            .px(px(CONTENT_MIN_PADDING))
            .pr(px(CONTENT_MIN_PADDING) + shift);
        let hero = match project.as_ref().filter(|project| !project.quick_chat) {
            None => hero
                .child(title.child(words(&crate::tr!(crate::i18n::variant_key(
                    "chat.slogan_quick",
                    seed
                )))))
                .child(picker(
                    "new-thread-kind",
                    Icon::empty()
                        .path("icons/message-square.svg")
                        .size_3p5()
                        .text_color(muted)
                        .into_any_element(),
                    div()
                        .text_body(false, cx)
                        .child(crate::widgets::fade_text(crate::tr!("sidebar.quick_chat")))
                        .into_any_element(),
                )),
            Some(project) => {
                let sentence =
                    crate::tr!(crate::i18n::variant_key("chat.slogan_project", seed)).into_owned();
                let (before, after) = sentence
                    .split_once("%{project}")
                    .unwrap_or((sentence.as_str(), ""));
                hero.child(
                    title
                        .when(!before.trim().is_empty(), |title| {
                            title.child(words(before))
                        })
                        .child(picker(
                            "new-thread-project",
                            crate::project_icon::artwork(image_host, project, 18.),
                            words(&project.name).into_any_element(),
                        ))
                        .when(!after.trim().is_empty(), |title| title.child(words(after))),
                )
            }
        };

        v_flex()
            .id("new-thread-page")
            .debug_selector(|| "new-thread-page".into())
            .flex_1()
            .min_h_0()
            .w_full()
            .justify_center()
            // A little above the middle, as the eye reads it.
            .pb(px(48.))
            .child(hero)
            .child(composer)
            .children(self.render_recent_threads(project.as_ref(), shift, cx))
            .into_any_element()
    }

    /// The place's latest threads, to open again; nothing when it has none.
    fn render_recent_threads(
        &self,
        project: Option<&tcode_core::project::Project>,
        shift: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let store = self.workspace_store.read(cx);
        let current = self.session_id.as_deref();
        let threads: Vec<_> = store
            .grouped_sessions()
            .into_iter()
            .find(|group| match project {
                Some(project) => group.project.id == project.id,
                None => group.project.quick_chat,
            })?
            .sessions
            .into_iter()
            .filter(|meta| Some(meta.id.as_str()) != current)
            .take(RECENT_THREADS)
            .collect();
        if threads.is_empty() {
            return None;
        }
        let now = self.workspace_store.read(cx).host_now_secs(cx);
        let rows =
            threads.into_iter().map(|meta| {
                let id = meta.id.clone();
                crate::material::accessible_clickable(
                    crate::material::selectable_row(h_flex(), false, false, cx)
                        .w_full()
                        .gap_3()
                        .cursor_pointer()
                        .debug_selector({
                            let id = meta.id.clone();
                            move || format!("new-thread-recent-{id}")
                        }),
                    SharedString::from(format!("new-thread-recent-{}", meta.id)),
                    Role::Button,
                    meta.title.clone(),
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let id = id.clone();
                    this.workspace_store
                        .update(cx, |store, cx| store.show_thread(id, cx));
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_body(false, cx)
                        .text_color(cx.theme().foreground)
                        .child(crate::widgets::fade_text(meta.title.clone())),
                )
                .child(div().flex_none().text_secondary(false, cx).child(
                    crate::widgets::fade_text(crate::time::humanize_ago(
                        now.saturating_sub(meta.updated_at),
                    )),
                ))
                .into_any_element()
            });
        Some(
            h_flex()
                .w_full()
                .justify_center()
                .px(px(CONTENT_MIN_PADDING))
                .pr(px(CONTENT_MIN_PADDING) + shift)
                .child(
                    v_flex()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .min_w_0()
                        .pt_2()
                        .gap_0p5()
                        .debug_selector(|| "new-thread-recent".into())
                        .child(
                            div()
                                .px(crate::material::selectable_row_inset(false))
                                .pb_1()
                                .text_caption(cx)
                                .child(crate::widgets::fade_text(crate::tr!(
                                    "chat.continue_recent"
                                ))),
                        )
                        .children(rows),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::{AppContext as _, Bounds, Pixels, TestAppContext, VisualTestContext, px, size};
    use tcode_core::project::Project;
    use tcode_protocol::{
        EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
    };

    use crate::chat::ChatView;
    use crate::store::WorkspaceStore;
    use crate::test_frames::draw_settled;
    use crate::window_state::WindowState;

    /// The start hub's projects follow the host's: one the host removes
    /// leaves the hub and one it adds joins it.
    #[gpui::test]
    fn start_hub_projects_fold_in_and_out_as_the_host_changes_them(cx: &mut TestAppContext) {
        cx.update(crate::test_client::init);
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
        let _pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::default());
        let (_, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(store.clone(), window_state, None, window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.run_until_parked();
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);
        let row = |name: &str, cx: &mut VisualTestContext| -> Option<Bounds<Pixels>> {
            cx.debug_bounds(format!("start-hub-project-{name}").leak())
        };
        assert!(row("gamma", cx).is_some(), "the hub lists the projects");

        for (names, beta_stays) in [
            (&["alpha", "gamma"][..], false),
            (&["alpha", "beta", "gamma"][..], true),
        ] {
            incoming.try_send(index(names)).unwrap();
            cx.run_until_parked();
            cx.run_until_parked();
            draw_settled(cx);
            assert_eq!(row("beta", cx).is_some(), beta_stays);
            assert!(row("gamma", cx).is_some(), "gamma stays");
        }
    }
}
