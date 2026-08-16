use super::super::*;

impl Composer {
    pub(in super::super) fn render_checkout_row(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let checkout = self.workspace_store.read(cx).composer_state().checkout?;
        let branch = checkout.branch;
        let branches = checkout.branches;
        let turn_running = checkout.turn_running;
        let is_draft = checkout.is_draft;
        let worktree_base = checkout.worktree_base;
        let worktree = checkout.worktree;
        let muted = cx.theme().muted_foreground;
        // In worktree draft mode the right-hand picker chooses the *base* branch
        // (its current value is the chosen base, defaulting to the live branch).
        let picker_current = worktree_base.clone().unwrap_or_else(|| branch.clone());
        let worktree_mode = worktree_base.is_some();

        // The branch chip: a popover listing local branches. While a turn runs
        // the selector is disabled (it just shows a "wait" tooltip).
        let right: AnyElement = if turn_running {
            Button::new("branch-picker")
                .ghost()
                .compact()
                .tooltip(crate::tr!("composer.wait_turn"))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                        .child(picker_current.clone()),
                )
                .into_any_element()
        } else {
            let store_open = self.workspace_store.clone();
            let store_content = self.workspace_store.clone();
            let current = picker_current.clone();
            let trigger = Button::new("branch-picker").ghost().compact().child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(Icon::empty().path("icons/git-branch.svg").xsmall())
                    .child(picker_current.clone())
                    .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
            );
            crate::material::overlay_popover("branch-popover")
                .anchor(Anchor::BottomRight)
                .trigger(trigger)
                .on_open_change(move |open, _window, cx| {
                    // Load branches lazily each time the popover opens.
                    if *open {
                        store_open.update(cx, |store, _cx| store.load_branches());
                    }
                })
                .content(move |_state, _window, cx| {
                    let branches = branches.clone();
                    let current = current.clone();
                    let popover = cx.entity();
                    let muted = cx.theme().muted_foreground;
                    let mut col = v_flex().w_full().p_1().gap_0p5();
                    if worktree_mode {
                        col = col.child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1()
                                .text_size(px(11.))
                                .font_medium()
                                .text_color(muted)
                                .child(crate::tr!("composer.worktree_base")),
                        );
                    }
                    if branches.is_empty() {
                        col = col.child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1p5()
                                .text_size(px(13.))
                                .text_color(muted)
                                .child(crate::tr!("composer.loading")),
                        );
                    } else {
                        for (index, name) in branches.iter().enumerate() {
                            let is_current = *name == current;
                            let branch_name = name.clone();
                            let store_pick = store_content.clone();
                            let pop = popover.clone();
                            col = col.child(
                                h_flex()
                                    .id(("branch-row", index))
                                    .flex_none()
                                    .w_full()
                                    .px_2()
                                    .py_1p5()
                                    .gap_2()
                                    .items_center()
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .hover(|s| s.bg(cx.theme().muted))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .child(name.clone()),
                                    )
                                    .when(is_current, |this| {
                                        this.child(
                                            Icon::new(IconName::Check)
                                                .xsmall()
                                                .text_color(cx.theme().primary),
                                        )
                                    })
                                    .on_click(move |_, window, cx| {
                                        let branch_name = branch_name.clone();
                                        store_pick.update(cx, |store, _cx| {
                                            if worktree_mode {
                                                // Choose the worktree's base branch.
                                                store.set_draft_workspace(
                                                    WorkspaceMode::NewWorktree {
                                                        base: branch_name,
                                                    },
                                                );
                                            } else {
                                                store.checkout_branch(branch_name);
                                            }
                                        });
                                        pop.update(cx, |st, cx| st.dismiss(window, cx));
                                    }),
                            );
                        }
                    }
                    div()
                        .id("branch-list")
                        .w(px(220.))
                        .max_h(px(280.))
                        .overflow_y_scroll()
                        .child(col)
                        .into_any_element()
                })
                .into_any_element()
        };

        // Left: the workspace-mode chip. A draft can pick "Local checkout" vs
        // "New worktree"; a started session shows its locked workspace.
        let left =
            self.render_workspace_chip(is_draft, worktree_mode, worktree.is_some(), &branch, cx);

        Some(
            h_flex()
                .w_full()
                .px_2()
                .pt_2()
                .items_center()
                .justify_between()
                .text_size(px(13.))
                .text_color(muted)
                .child(left)
                .child(right)
                .into_any_element(),
        )
    }

    /// The left-hand workspace chip: a draft can pick current checkout vs a new
    /// dedicated worktree; a started session shows its locked workspace.
    pub(in super::super) fn render_workspace_chip(
        &self,
        is_draft: bool,
        worktree_mode: bool,
        has_worktree: bool,
        base_default: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let label = if worktree_mode || has_worktree {
            crate::tr!("composer.new_worktree")
        } else {
            crate::tr!("composer.local_checkout")
        };

        // Started sessions show a static, locked workspace label.
        if !is_draft {
            return h_flex()
                .gap_1p5()
                .items_center()
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                .child(label)
                .into_any_element();
        }

        let store_content = self.workspace_store.clone();
        let base_default = base_default.to_string();
        let trigger = Button::new("workspace-picker").ghost().compact().child(
            h_flex()
                .gap_1p5()
                .items_center()
                .text_size(px(13.))
                .text_color(muted)
                .child(Icon::empty().path("icons/folder-closed.svg").xsmall())
                .child(label)
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted)),
        );
        crate::material::overlay_popover("workspace-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(trigger)
            .content(move |_state, _window, cx| {
                let popover = cx.entity();
                let store_local = store_content.clone();
                let store_worktree = store_content.clone();
                let pop_local = popover.clone();
                let pop_worktree = popover.clone();
                let base = base_default.clone();
                let workspace_row = |label: gpui::SharedString,
                                     selected: bool,
                                     cx: &mut Context<PopoverState>|
                 -> gpui::Div {
                    h_flex()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .gap_2()
                        .items_center()
                        .rounded(px(6.))
                        .cursor_pointer()
                        .text_size(px(13.))
                        .hover(|s| s.bg(cx.theme().muted))
                        .child(div().flex_1().min_w_0().child(label))
                        .when(selected, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .xsmall()
                                    .text_color(cx.theme().primary),
                            )
                        })
                };
                v_flex()
                    .w(px(200.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!("composer.workspace")),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.local_checkout").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-local")
                        .on_click(move |_, window, cx| {
                            store_local.update(cx, |store, _cx| {
                                store.set_draft_workspace(WorkspaceMode::LocalCheckout);
                            });
                            pop_local.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        workspace_row(
                            crate::tr!("composer.new_worktree").into_owned().into(),
                            false,
                            cx,
                        )
                        .id("workspace-worktree")
                        .on_click(move |_, window, cx| {
                            let base = base.clone();
                            store_worktree.update(cx, |store, _cx| {
                                store.set_draft_workspace(WorkspaceMode::NewWorktree { base });
                            });
                            pop_worktree.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }
}
