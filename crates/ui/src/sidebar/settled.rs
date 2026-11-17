use super::*;

impl SessionsSidebar {
    pub(super) fn reveal_selected_settled(&mut self, cx: &mut Context<Self>) {
        let selected = self.current_row(cx);
        if selected == self.last_selected {
            return;
        }
        let found = selected.as_ref().and_then(|thread| {
            let store = self.store_of(&thread.host)?;
            let meta = store.read(cx).thread_meta(&thread.session_id)?.clone();
            Some((store, meta))
        });
        if selected.is_some() && found.is_none() {
            return;
        }
        let host = selected.as_ref().map(|thread| thread.host.clone());
        self.last_selected = selected;
        let (Some((store, meta)), Some(host)) = (found, host) else {
            return;
        };
        if meta.is_settled()
            && let Some(project_id) = meta.project_id.clone()
        {
            // The project view's shelf opens; the threads view shows the
            // viewed row under its closed shelf instead.
            let shelf = Shelf::Project(ProjectKey {
                host,
                project_id: project_id.clone(),
            });
            if self.expanded_settled.insert(shelf.clone()) {
                self.settled_limits.remove(&shelf);
            }
            if store.read(cx).is_project_collapsed(&project_id) {
                store.update(cx, |store, cx| {
                    store.toggle_project_collapsed(project_id, cx)
                });
            }
        }
        self.compact_model_dirty = true;
    }

    /// `row` is where the list shows the thread pinned from its menu, and
    /// whether it shows there in Pinned yet: the host's change arrives after
    /// the menu closes. A list that does not show it forgets it.
    pub(super) fn reveal_pinned_row(
        &mut self,
        list: &ListState,
        scroll: &crate::motion::ListScroll,
        row: Option<(usize, bool)>,
        cx: &App,
    ) {
        match row {
            Some((index, true)) => {
                scroll.reveal(list, index, cx);
                self.reveal_pinned = None;
            }
            Some((_, false)) => {}
            None => self.reveal_pinned = None,
        }
    }

    pub(super) fn limit_settled_rows(&self, shelf: &Shelf, rows: &mut Vec<Listed>) {
        let open = self.expanded_settled.contains(shelf);
        let limit = if self.fold_shown(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open) {
            self.settled_limits.get(shelf).copied().unwrap_or(10)
        } else {
            0
        };
        let mut index = 0;
        rows.retain(|(host, meta)| {
            let keep = index < limit || self.is_selected(host, &meta.id);
            index += 1;
            keep
        });
    }

    /// The fold factor of each row a Settled shelf shows, and of its "Show
    /// more" row after them. The selected row a shut shelf keeps stays put.
    pub(super) fn shelf_factors(
        &self,
        shelf: &Shelf,
        rows: &[Listed],
        more: bool,
    ) -> (Vec<f32>, f32) {
        let open = self.expanded_settled.contains(shelf);
        let progress = self.fold_progress(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open);
        let count = rows
            .iter()
            .filter(|(host, meta)| !self.is_selected(host, &meta.id))
            .count()
            + usize::from(more);
        let mut index = 0;
        let factors = rows
            .iter()
            .map(|(host, meta)| {
                if self.is_selected(host, &meta.id) {
                    return 1.;
                }
                index += 1;
                fold_factor(progress, index - 1, count)
            })
            .collect();
        (factors, fold_factor(progress, index, count))
    }

    pub(super) fn settled_more_count(&self, shelf: &Shelf, hidden: usize) -> Option<usize> {
        let open = self.expanded_settled.contains(shelf);
        (self.fold_shown(&fold_key(Fold::Shelf, &self.shelf_name(shelf)), open) && hidden > 0)
            .then(|| hidden.min(25))
    }

    pub(super) fn render_settled_more(
        &self,
        shelf: &Shelf,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = self.shelf_name(shelf);
        let shelf = shelf.clone();
        let compact = self.compact(cx);
        // A full-width row with its label at the leading edge: `Button`
        // centres its content.
        selectable_row(
            crate::material::accessible_clickable(
                h_flex(),
                SharedString::from(format!("settled-more-{key}")),
                Role::Button,
                crate::tr!("sidebar.show_more_settled", count = count),
                cx,
            ),
            false,
            compact,
            cx,
        )
        .debug_selector(move || format!("settled-more-{key}"))
        .w_full()
        .text_secondary(compact, cx)
        .cursor_pointer()
        .child(Icon::new(IconName::Plus))
        .child(crate::widgets::fade_text(crate::tr!(
            "sidebar.show_more_settled",
            count = count
        )))
        .on_click(cx.listener(move |this, _, _, cx| {
            *this.settled_limits.entry(shelf.clone()).or_insert(10) += 25;
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .into_any_element()
    }

    pub(super) fn render_active_empty(&self, cx: &App) -> gpui::AnyElement {
        empty_row(crate::tr!("sidebar.active_empty"), cx).into_any_element()
    }

    pub(super) fn render_settled_header(
        &self,
        shelf: &Shelf,
        count: usize,
        scope: Option<&ProjectKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let expanded = self.expanded_settled.contains(shelf);
        let key = self.shelf_name(shelf);
        let color = match &self.drag {
            None => cx.theme().muted_foreground,
            Some(drag) if drag.target(scope) == Some(ThreadSection::Settled) => cx.theme().primary,
            Some(_) => cx.theme().sidebar_foreground,
        };
        // The caption lights up as a drag takes it as its target.
        let color = transition(
            (SharedString::from(format!("settled-{key}")), "color"),
            color,
            gpui_base::motion::Transition::new(GLIDE),
            window,
            cx,
        );
        let turned = self.fold_progress(&fold_key(Fold::Shelf, &key), expanded);
        let shelf = shelf.clone();
        let compact = self.compact(cx);
        let spacing = &cx.theme().tokens.spacing;
        let inset = if compact {
            px(COMPACT_PAGE_INSET)
        } else {
            spacing.sm
        };
        // A disclosure across a whole section bar, at the bar's height and
        // inset. Not a `bar`: gpui-base's `Toolbar` fixes the toolbar role,
        // and its `Collapsible` has no trigger.
        crate::material::accessible_clickable(
            h_flex(),
            SharedString::from(format!("settled-{key}")),
            Role::Button,
            crate::tr!("sidebar.settled"),
            cx,
        )
        .aria_expanded(expanded)
        .debug_selector(move || format!("settled-{key}"))
        .w_full()
        .h(px(bar_height(Bar::Section, compact)))
        .px(inset)
        .gap(spacing.sm)
        .text_caption(cx)
        .text_color(color)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            // A shelf shuts with the rows it showed and opens at its first page.
            if !this.expanded_settled.remove(&shelf) {
                this.settled_limits.remove(&shelf);
                this.expanded_settled.insert(shelf.clone());
            }
            this.compact_model_dirty = true;
            cx.notify();
        }))
        .child(collapse_chevron(turned, cx).text_color(color))
        .child(crate::widgets::fade_text(crate::tr!("sidebar.settled")))
        .child(count.to_string())
        .into_any_element()
    }
}
