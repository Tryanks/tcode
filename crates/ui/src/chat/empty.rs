use super::*;

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
        _window: &mut Window,
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
        let mut launcher = v_flex().w_full().gap_1().child(
            div()
                .px_3()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(crate::material::tracked_uppercase(
                    crate::tr!("chat.start_hub_title").as_ref(),
                )),
        );
        for (project, last_activity) in hub_projects {
            let project_id = project.id.clone();
            let cwd = project.root.clone();
            let row_label = crate::tr!("sidebar.project", name = project.name.clone()).into_owned();
            launcher = launcher.child(
                crate::material::accessible_clickable(
                    crate::material::selectable_row(h_flex(), false, false, cx),
                    SharedString::from(format!("start-hub-project-{}", project.id)),
                    Role::Button,
                    row_label,
                    cx,
                )
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.workspace_store.update(cx, |store, cx| {
                        store.start_draft(project_id.clone(), cwd.clone(), cx);
                    });
                }))
                .child(crate::project_icon::artwork(&project, 16.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(false, cx)
                        .text_color(cx.theme().foreground)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(project.name)),
                )
                .when_some(last_activity, |row, last_activity| {
                    row.child(div().flex_none().text_secondary(false, cx).child(
                        crate::time::humanize_ago(now_secs().saturating_sub(last_activity)),
                    ))
                }),
            );
        }
        content = content.child(launcher).child(
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
        let shown = crate::motion::shown("scroll-to-end-pill", shown.then_some(()), window, cx)?;
        // Absolute positioning ignores mx_auto without pinned horizontal
        // insets; span the region and center with flex instead.
        let pill = floating_backing(crate::material::radius_button(cx), cx).child(
            Button::new("scroll-to-end")
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
                .right_0()
                .flex()
                .justify_center()
                .child(shown.fade(pill))
                .into_any_element(),
        )
    }
}
