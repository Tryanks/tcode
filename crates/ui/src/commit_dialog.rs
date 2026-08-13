//! The commit dialog (ported from T3's `GitActionsControl.tsx` commit flow):
//! a changed-files list with include/exclude checkboxes, the current branch,
//! a default-branch safeguard banner, and a commit-message textarea pre-filled
//! by AI generation (with a regenerate button).

use std::collections::HashSet;

use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::checkbox::Checkbox;
use crate::widgets::input::{Textarea, TextareaState};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use tcode_core::git::{GitAction, GitFileEntry, feature_branch_name, included_paths};
use tcode_protocol::Command;

use crate::store::WorkspaceStore;

pub struct CommitDialog {
    store: Entity<WorkspaceStore>,
    message: Entity<TextareaState>,
    files: Vec<GitFileEntry>,
    /// User-*excluded* (unchecked) paths — kept out of the commit via pathspec.
    excluded: HashSet<String>,
    branch: Option<String>,
    on_default_branch: bool,
    /// Safeguard: create a `tcode/<slug>` feature branch and commit there.
    create_feature_branch: bool,
    action: GitAction,
    generating: bool,
    scroll: ScrollHandle,
    _gen_task: Option<Task<()>>,
}

impl CommitDialog {
    pub fn new(
        store: Entity<WorkspaceStore>,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (files, branch, on_default_branch) = store.read(cx).commit_dialog_state();
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(crate::tr!("git.commit.message_placeholder"))
        });
        let mut this = Self {
            store,
            message,
            files,
            excluded: HashSet::new(),
            branch,
            on_default_branch,
            create_feature_branch: false,
            action,
            generating: false,
            scroll: ScrollHandle::new(),
            _gen_task: None,
        };
        // Pre-fill the message with an AI-generated commit message.
        this.regenerate(window, cx);
        this
    }

    /// The checked-file subset staged for the commit (`None` = all files).
    fn included(&self) -> Option<Vec<String>> {
        included_paths(&self.files, &self.excluded)
    }

    fn toggle_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.excluded.contains(path) {
            self.excluded.remove(path);
        } else {
            self.excluded.insert(path.to_string());
        }
        cx.notify();
    }

    /// (Re)generate the commit message via the current provider (headless).
    fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.generating {
            return;
        }
        self.generating = true;
        let included = self.included();
        let store = self.store.clone();
        let task = store.update(cx, |store, cx| store.generate_commit_message(included, cx));
        let message = self.message.clone();
        self._gen_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.generating = false;
                match result {
                    Ok(text) => {
                        message.update(cx, |state, cx| state.set_value(text, window, cx));
                    }
                    Err(err) => log::warn!("commit message generation failed: {err}"),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Confirm the commit. Returns `true` when the dialog should close. An empty
    /// message triggers a regeneration instead of committing (T3 semantics).
    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let message = self.message.read(cx).value().trim().to_string();
        if message.is_empty() {
            self.regenerate(window, cx);
            return false;
        }
        let included = self.included();
        let feature_branch = if self.on_default_branch && self.create_feature_branch {
            Some(feature_branch_name(
                message.lines().next().unwrap_or("update"),
            ))
        } else {
            None
        };
        let action = self.action;
        self.store.update(cx, |store, _cx| {
            store.dispatch(Command::RunGitAction {
                action,
                message: Some(message),
                included,
                feature_branch,
            });
        });
        true
    }

    pub fn confirm_label(&self, cx: &App) -> String {
        if self.on_default_branch && self.create_feature_branch {
            let branch = feature_branch_name(
                self.message
                    .read(cx)
                    .value()
                    .lines()
                    .next()
                    .unwrap_or("update"),
            );
            return crate::tr!("git.commit.confirm_feature", branch = branch).into_owned();
        }
        match self.action {
            GitAction::CommitPush => crate::tr!("git.commit.confirm_push").into_owned(),
            _ => crate::tr!("git.commit.confirm").into_owned(),
        }
    }

    fn render_file_row(
        &self,
        index: usize,
        file: &GitFileEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let included = !self.excluded.contains(&file.path);
        let path = file.path.clone();
        let path_for_toggle = file.path.clone();
        h_flex()
            .w_full()
            .py_1()
            .px_1()
            .gap_2()
            .items_center()
            .child(
                Checkbox::new(("commit-file", index))
                    .checked(included)
                    .on_click(cx.listener(move |dialog, _checked: &bool, _window, cx| {
                        dialog.toggle_file(&path_for_toggle, cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(13.))
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(path),
            )
            .when(file.insertions > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(cx.theme().success)
                        .child(format!("+{}", file.insertions)),
                )
            })
            .when(file.deletions > 0, |this| {
                this.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(cx.theme().danger)
                        .child(format!("-{}", file.deletions)),
                )
            })
    }
}

impl Render for CommitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        // Branch row.
        let branch_label = self
            .branch
            .clone()
            .unwrap_or_else(|| crate::tr!("git.commit.detached").into_owned());
        let branch_row = h_flex()
            .w_full()
            .gap_1p5()
            .items_center()
            .text_size(px(13.))
            .text_color(muted)
            .child(
                Icon::empty()
                    .path("icons/git-branch.svg")
                    .xsmall()
                    .text_color(muted),
            )
            .child(crate::tr!("git.commit.branch"))
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().foreground)
                    .child(branch_label),
            );

        let mut body = v_flex().w_full().gap_3().child(branch_row);

        // Default-branch safeguard banner.
        if self.on_default_branch {
            let create = self.create_feature_branch;
            body = body.child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .rounded(crate::material::radius_card())
                    .border_1()
                    .border_color(cx.theme().warning)
                    .bg(cx.theme().warning.opacity(0.08))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .font_medium()
                            .text_color(cx.theme().warning)
                            .child(Icon::new(IconName::TriangleAlert).xsmall())
                            .child(crate::tr!("git.commit.default_warning_title")),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(muted)
                            .child(crate::tr!("git.commit.default_warning_body")),
                    )
                    .child(
                        Checkbox::new("commit-feature-branch")
                            .checked(create)
                            .label(crate::tr!("git.commit.create_feature_branch").into_owned())
                            .on_click(cx.listener(|dialog, checked: &bool, _window, cx| {
                                dialog.create_feature_branch = *checked;
                                cx.notify();
                            })),
                    ),
            );
        }

        // Changed-files list.
        let files_header = h_flex().w_full().justify_between().items_center().child(
            div()
                .text_size(px(11.))
                .font_medium()
                .text_color(muted)
                .child(crate::tr!(
                    "git.commit.files_count",
                    count = self.files.len()
                )),
        );
        let mut file_rows = v_flex().w_full().gap_0p5();
        if self.files.is_empty() {
            file_rows = file_rows.child(
                div()
                    .p_2()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(crate::tr!("git.commit.no_changes")),
            );
        } else {
            for (index, file) in self.files.iter().enumerate() {
                file_rows = file_rows.child(self.render_file_row(index, file, cx));
            }
        }
        let file_list = div()
            .id("commit-files")
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .w_full()
            .max_h(px(180.))
            .child(file_rows);
        body = body.child(
            v_flex().w_full().gap_1().child(files_header).child(
                div()
                    .w_full()
                    .rounded(crate::material::radius_input())
                    .border_1()
                    .border_color(cx.theme().border)
                    .p_1()
                    .child(file_list),
            ),
        );

        // Commit-message textarea + regenerate control.
        let message_header = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_size(px(11.))
                    .font_medium()
                    .text_color(muted)
                    .child(crate::tr!("git.commit.message_label")),
            )
            .child(
                Button::new("commit-regenerate")
                    .rounded(crate::material::radius_button())
                    .ghost()
                    .xsmall()
                    .icon(IconName::Undo)
                    .label(if self.generating {
                        crate::tr!("git.commit.generating")
                    } else {
                        crate::tr!("git.commit.regenerate")
                    })
                    .disabled(self.generating)
                    .on_click(cx.listener(|dialog, _, window, cx| {
                        dialog.regenerate(window, cx);
                    })),
            );
        body = body.child(
            v_flex()
                .w_full()
                .gap_1()
                .child(message_header)
                .child(Textarea::new(&self.message).rounded(crate::material::radius_input())),
        );

        crate::material::overlay_contour(
            div()
                .w_full()
                .min_w(px(520.))
                .rounded(crate::material::radius_overlay()),
            cx,
        )
        .child(body)
    }
}
