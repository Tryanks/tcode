use super::*;

impl ChatView {
    pub(super) fn render_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
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
        // A full workspace with nothing to show is on its way to a draft the
        // shell asked for; a page of its own here would only flash by.
        div().flex_1().into_any_element()
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

        // A thread with no folder of its own offers to give the host one.
        let add_project =
            self.workspace_store.read(cx).scope().is_full().then(|| {
                Button::new("new-thread-add-project")
                    .motion(&self.motion)
                    .ghost()
                    .chip()
                    .aria_label(crate::tr!("sidebar.add_project"))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .child(
                                Icon::empty()
                                    .path("icons/folder-plus.svg")
                                    .size_3p5()
                                    .text_color(muted),
                            )
                            .child(div().text_body(false, cx).child(crate::widgets::fade_text(
                                crate::tr!("sidebar.add_project"),
                            ))),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        crate::add_project_dialog::open(this.workspace_store.clone(), window, cx);
                    }))
            });
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
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
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
                        ))
                        .children(add_project),
                ),
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
