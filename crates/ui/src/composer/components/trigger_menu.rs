use super::super::*;

impl Composer {
    /// Whether a trigger menu should currently be shown.
    pub(in super::super) fn menu_visible(&self) -> bool {
        self.active_trigger.is_some() && !self.menu_dismissed
    }

    /// Recompute the active trigger from the input text + cursor, resetting the
    /// highlight (and un-dismissing) when the trigger identity changes, and
    /// lazily loading the workspace listing for `@`-mentions.
    pub(in super::super) fn recompute_trigger(&mut self, cx: &mut Context<Self>) {
        let (text, cursor) = {
            let state = self.input.read(cx);
            (
                state.value().to_string(),
                state.base_state().read(cx).cursor(),
            )
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
            state.base_state().update(cx, |state, cx| {
                state.set_selected_range(trigger.range.clone(), cx)
            });
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

        let mut list = v_flex().w_full().p_1().gap_0p5();
        if rows.is_empty() {
            list = list.child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2p5()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(if loading {
                        crate::tr!("composer.searching").into_owned()
                    } else {
                        empty_text
                    }),
            );
        } else {
            // T3 §5: the `/` menu groups rows under `Built-in` / `Provider`
            // headers, and `$` under `Skills`. A header is emitted whenever the
            // group changes; headers are not selectable, so row indices (and the
            // arrow-key highlight) still index `rows` directly.
            let mut last_group: Option<&'static str> = None;
            for (index, row) in rows.iter().enumerate() {
                if let Some(group) = row.group
                    && last_group != Some(group)
                {
                    last_group = Some(group);
                    list = list.child(
                        div()
                            .flex_none()
                            .px_2()
                            .pt_1p5()
                            .pb_0p5()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(muted)
                            .child(crate::tr!(group).into_owned()),
                    );
                }
                let is_active = index == highlight;
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
                list = list.child(
                    h_flex()
                        .id(("menu-row", index))
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
                        })),
                );
            }
        }

        Some(
            div()
                .id("composer-trigger-menu")
                .role(Role::ListBox)
                .aria_label(crate::tr!("composer.trigger_results"))
                .w_full()
                .max_h(px(288.))
                .overflow_y_scroll()
                // T3 overlay contour: popover fill + hairline border + shadow_xl
                // at the 14px overlay radius (this menu floats over the card).
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
