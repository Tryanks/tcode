use super::super::*;

impl Composer {
    pub(in super::super) fn menu_visible(&self) -> bool {
        self.active_trigger.is_some() && !self.menu_dismissed
    }

    /// Recompute the active trigger from the input text + cursor, resetting the
    /// highlight (and un-dismissing) when the trigger identity changes, and
    /// lazily loading the workspace listing for `@`-mentions.
    pub(in super::super) fn recompute_trigger(&mut self, cx: &mut Context<Self>) {
        if self.compact {
            self.active_trigger = None;
            return;
        }
        let (text, cursor) = {
            let state = self.input.read(cx);
            (state.value().to_string(), state.cursor())
        };
        let trigger = detect_composer_trigger(&text, cursor);
        let key = trigger
            .as_ref()
            .map(|t| format!("{:?}\u{1}{}", t.kind, t.query));
        if key != self.menu_last_key {
            self.menu_highlight = 0;
            self.menu_dismissed = false;
            self.menu_last_key = key;
        }
        if matches!(trigger.as_ref().map(|t| t.kind), Some(TriggerKind::Path)) {
            self.ensure_workspace(cx);
        }
        self.active_trigger = trigger;
    }

    /// Load the workspace file/folder listing for the active session cwd in the
    /// background (gitignore-respected), the first time a mention menu opens.
    pub(in super::super) fn ensure_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.workspace_store.read(cx).composer_state().active_cwd else {
            return;
        };
        if self.workspace_loading || self.workspace.as_ref().is_some_and(|(c, _)| *c == cwd) {
            return;
        }
        self.workspace_loading = true;
        let store = self.workspace_store.clone();
        let walked = store.update(cx, |store, cx| store.list_active_workspace(cx));
        cx.spawn(async move |this, cx| {
            let walked = walked.await;
            let _ = this.update(cx, |this, cx| {
                this.workspace = Some((cwd, walked));
                this.workspace_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Build the rows for the currently active trigger menu, plus its empty-state
    /// copy and whether it is still loading.
    pub(in super::super) fn menu_rows(&self, cx: &App) -> (Vec<MenuRow>, String, bool) {
        let Some(trigger) = self.active_trigger.as_ref() else {
            return (Vec::new(), String::new(), false);
        };
        match trigger.kind {
            TriggerKind::Path => {
                let entries = self
                    .workspace
                    .as_ref()
                    .map(|(_, e)| e.as_slice())
                    .unwrap_or(&[]);
                let rows = filter_entries(entries, &trigger.query, FILE_MENU_ROW_CAP)
                    .into_iter()
                    .map(|e| MenuRow {
                        primary: e.basename.clone(),
                        secondary: e.parent.clone(),
                        icon: if e.is_dir {
                            MenuIcon::Folder
                        } else {
                            MenuIcon::File
                        },
                        accept: MenuAccept::InsertPath(e.rel_path.clone()),
                        group: None,
                    })
                    .collect();
                let loading = self.workspace_loading && self.workspace.is_none();
                (rows, crate::tr!("composer.no_files").into_owned(), loading)
            }
            TriggerKind::Skill => {
                // Provider-native skills (Claude `skills` / Codex `skills/list`),
                // fuzzily filtered by the `$` query with no item cap.
                let commands = self
                    .workspace_store
                    .read(cx)
                    .composer_state()
                    .provider_commands;
                let rows =
                    filter_provider_commands(&commands, ProviderCommandKind::Skill, &trigger.query)
                        .into_iter()
                        .map(|c| MenuRow {
                            primary: format!("${}", c.name),
                            secondary: c
                                .description
                                .clone()
                                .unwrap_or_else(|| crate::tr!("composer.run_skill").into_owned()),
                            icon: MenuIcon::Skill,
                            accept: MenuAccept::InsertSkill(c.name.clone()),
                            group: Some("composer.group_skills"),
                        })
                        .collect();
                (rows, crate::tr!("composer.no_skills").into_owned(), false)
            }
            TriggerKind::SlashCommand | TriggerKind::SlashModel => {
                let builtins: [(&str, Option<&str>, &str, MenuAccept); 5] = [
                    (
                        "model",
                        None,
                        "composer.cmd_model_desc",
                        MenuAccept::OpenModelPicker,
                    ),
                    (
                        "plan",
                        None,
                        "composer.cmd_plan_desc",
                        MenuAccept::SetMode(InteractionMode::Plan),
                    ),
                    (
                        "default",
                        None,
                        "composer.cmd_default_desc",
                        MenuAccept::SetMode(InteractionMode::Build),
                    ),
                    (
                        "orchestrate",
                        Some("composer.cmd_orchestrate_label"),
                        "composer.cmd_orchestrate_desc",
                        MenuAccept::InsertOrchestrate,
                    ),
                    (
                        "later",
                        None,
                        "composer.cmd_later_desc",
                        MenuAccept::InsertLater,
                    ),
                ];
                let mut rows: Vec<MenuRow> = builtins
                    .into_iter()
                    .filter(|(name, _, _, _)| fuzzy_score(&trigger.query, name).is_some())
                    .map(|(name, label, desc, accept)| MenuRow {
                        primary: format!(
                            "/{}",
                            label
                                .map(|key| crate::tr!(key).into_owned())
                                .unwrap_or_else(|| name.to_string())
                        ),
                        secondary: crate::tr!(desc).into_owned(),
                        icon: MenuIcon::Command,
                        accept,
                        group: Some("composer.group_builtin"),
                    })
                    .collect();
                // Provider-native slash commands (Claude `slash_commands`), shown
                // after the built-in group, fuzzily filtered without truncation.
                let commands = self
                    .workspace_store
                    .read(cx)
                    .composer_state()
                    .provider_commands;
                rows.extend(
                    filter_provider_commands(
                        &commands,
                        ProviderCommandKind::Command,
                        &trigger.query,
                    )
                    .into_iter()
                    .map(|c| MenuRow {
                        primary: format!("/{}", c.name),
                        secondary: c.description.clone().unwrap_or_else(|| {
                            crate::tr!("composer.run_provider_command").into_owned()
                        }),
                        icon: MenuIcon::Command,
                        accept: MenuAccept::InsertCommand(c.name.clone()),
                        group: Some("composer.group_provider"),
                    }),
                );
                (rows, crate::tr!("composer.no_command").into_owned(), false)
            }
        }
    }

    /// Replace the active trigger's text range in the input with `replacement`.
    pub(in super::super) fn replace_trigger(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(trigger) = self.active_trigger.clone() else {
            return;
        };
        let replacement = replacement.to_string();
        self.input.update(cx, |state, cx| {
            state.set_selected_range(trigger.range.clone(), cx);
            state.replace(replacement.clone(), window, cx);
        });
    }

    /// Accept the trigger-menu row at `index`.
    pub(in super::super) fn accept_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (rows, _, _) = self.menu_rows(cx);
        let Some(row) = rows.get(index).cloned() else {
            return;
        };
        match &row.accept {
            MenuAccept::InsertPath(path) => {
                let link = format!("{} ", serialize_composer_file_link(path));
                self.replace_trigger(&link, window, cx);
            }
            MenuAccept::InsertSkill(name) => self.replace_trigger(&format!("${name} "), window, cx),
            MenuAccept::InsertCommand(name) => {
                self.replace_trigger(&format!("/{name} "), window, cx)
            }
            MenuAccept::InsertOrchestrate => self.replace_trigger("/orchestrate ", window, cx),
            MenuAccept::InsertLater => self.replace_trigger("/later ", window, cx),
            MenuAccept::OpenModelPicker => {
                self.replace_trigger("", window, cx);
                self.model_picker_token = self.model_picker_token.wrapping_add(1);
            }
            MenuAccept::SetMode(mode) => {
                let mode = *mode;
                self.replace_trigger("", window, cx);
                self.workspace_store
                    .update(cx, |store, _cx| store.set_interaction_mode(mode));
            }
        }
        self.active_trigger = None;
        self.menu_dismissed = true;
        cx.notify();
    }

    /// The floating `@`/`/`/`$` menu, rendered in-flow just above the composer
    /// card. `None` when no trigger is active (or it was dismissed).
    pub(in super::super) fn render_trigger_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.menu_visible() {
            return None;
        }
        let (rows, empty_text, loading) = self.menu_rows(cx);
        let muted = cx.theme().muted_foreground;
        let highlight = self.menu_highlight.min(rows.len().saturating_sub(1));

        let list = if rows.is_empty() {
            div()
                .p_1()
                .child(
                    div()
                        .px_3()
                        .py_2p5()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(if loading {
                            crate::tr!("composer.searching").into_owned()
                        } else {
                            empty_text
                        }),
                )
                .into_any_element()
        } else {
            // Group headers are not selectable: each opens the row that starts
            // its group, so keyboard selection indexes `rows` directly.
            let mut last_group: Option<&'static str> = None;
            let headers: Vec<Option<&'static str>> = rows
                .iter()
                .map(|row| {
                    let group = row.group.filter(|group| last_group != Some(*group));
                    if group.is_some() {
                        last_group = group;
                    }
                    group
                })
                .collect();
            let count = rows.len();
            let rows: Rc<[MenuRow]> = rows.into();
            crate::scroll::VirtualList::measured(
                "composer-trigger-rows",
                count,
                cx.processor(move |_, index: usize, _, cx| {
                    let muted = cx.theme().muted_foreground;
                    div().when(index + 1 < count, |row| row.pb_0p5()).child(
                        v_flex()
                            .gap_0p5()
                            .when_some(headers[index], |col, group| {
                                col.child(
                                    div()
                                        .px_2()
                                        .pt_1p5()
                                        .pb_0p5()
                                        .text_size(px(11.))
                                        .font_medium()
                                        .text_color(muted)
                                        .child(crate::tr!(group).into_owned()),
                                )
                            })
                            .child(render_menu_row(&rows[index], index, index == highlight, cx)),
                    )
                }),
            )
            .reveal(Some(highlight))
            .w_full()
            // The menu's border sits outside this cap.
            .max_h(px(286.))
            .p_1()
            .into_any_element()
        };

        Some(
            div()
                .id("composer-trigger-menu")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.trigger_results"))
                .w_full()
                .rounded(crate::material::radius_overlay())
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().popover)
                .shadow_xl()
                .child(list)
                .with_animation(
                    "composer-trigger-menu-pop-in",
                    Animation::new(Duration::from_millis(150)),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element(),
        )
    }
}

fn render_menu_row(
    row: &MenuRow,
    index: usize,
    is_active: bool,
    cx: &mut Context<Composer>,
) -> impl IntoElement + use<> {
    let muted = cx.theme().muted_foreground;
    let icon = match row.icon {
        MenuIcon::File => Icon::empty().path("icons/file.svg"),
        MenuIcon::Folder => Icon::empty().path("icons/folder-closed.svg"),
        MenuIcon::Command => Icon::empty().path("icons/box.svg"),
        MenuIcon::Skill => Icon::empty().path("icons/ruler.svg"),
    };
    let accessible_label = crate::tr!(
        "composer.trigger_option",
        primary = row.primary.clone(),
        secondary = row.secondary.clone()
    )
    .into_owned();
    h_flex()
        .id(("menu-row", index))
        .debug_selector(move || format!("menu-row-{index}"))
        .role(Role::ListBoxOption)
        .aria_label(accessible_label)
        .aria_selected(is_active)
        .when(is_active, |row| row.aria_active_descendant())
        .flex_none()
        .w_full()
        .h(px(28.))
        .px_2()
        .gap_2()
        .items_center()
        .rounded(crate::material::radius_chip())
        .cursor_pointer()
        .when(is_active, |s| s.bg(cx.theme().list_active))
        .hover(|s| s.bg(cx.theme().muted))
        .child(icon.small().text_color(muted))
        .child(
            div()
                .flex_none()
                .text_size(px(13.))
                .font_medium()
                .child(row.primary.clone()),
        )
        .when(!row.secondary.is_empty(), |this| {
            this.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(row.secondary.clone()),
            )
        })
        .on_mouse_move(cx.listener(move |this, _, _, cx| {
            if this.menu_highlight != index {
                this.menu_highlight = index;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.accept_menu(index, window, cx);
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};

    #[gpui::test]
    fn arrow_keys_keep_the_highlighted_mention_laid_out(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(std::env::temp_dir().join(format!(
                "tcode-trigger-menu-test-{}-{}",
                std::process::id(),
                tcode_services::store::now_millis()
            )))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("trigger-menu".into(), std::env::temp_dir(), cx);
            let timeline = state.residents.live.get(&id).unwrap().timeline.clone();
            (id, timeline)
        }))
        .unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(size(px(800.), px(600.)));
        composer.update_in(cx, |composer, window, cx| {
            let cwd = store.read(cx).composer_state().active_cwd.unwrap();
            let entries = (0..FILE_MENU_ROW_CAP)
                .map(|index| PathEntry::from_rel(format!("file-{index:02}.rs"), false))
                .collect();
            composer.workspace = Some((cwd, entries));
            window.focus(&composer.input.read(cx).focus_handle(cx), cx);
        });
        cx.simulate_input("@");
        cx.update(|window, cx| _ = window.draw(cx));
        let last = FILE_MENU_ROW_CAP - 1;
        let last_row: &'static str = format!("menu-row-{last}").leak();
        assert!(cx.debug_bounds("menu-row-0").is_some());
        assert!(cx.debug_bounds(last_row).is_none());

        for _ in 0..last {
            cx.simulate_keystrokes("down");
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds(last_row).is_some());
        for _ in 0..last {
            cx.simulate_keystrokes("up");
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds("menu-row-0").is_some());
    }
}
