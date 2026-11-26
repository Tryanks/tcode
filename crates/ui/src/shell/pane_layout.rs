use super::*;

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

impl AppShell {
    /// The keyboard has nowhere left to be, or a thread was opened without
    /// naming a pane: it goes to the current pane, into its composer as soon
    /// as that is drawn. A route without the conversation keeps out of it.
    pub(super) fn return_keyboard(&self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.window_state.read(cx);
        let page = !state.compact || state.destination() == Destination::Thread;
        if state.route() != Route::Chat || !page || crate::window_seam::is_mobile(cx) {
            return;
        }
        let Some(column) = &self.column else {
            return;
        };
        let pane = column.current_pane.clone();
        if pane.read(cx).session_id().is_some() {
            let chat = pane.read(cx).chat();
            chat.update(cx, |chat, cx| chat.focus_composer(window, cx));
        } else {
            window.focus(&pane.focus_handle(cx), cx);
        }
    }

    /// The pane a window-level command acts on.
    /// The pane commands `pane` offers now; none in the compact layout.
    pub(super) fn pane_controls(
        &self,
        pane: &Entity<ThreadPanel>,
        window: &Window,
        cx: &App,
    ) -> Option<PaneControls> {
        if self.compact(cx) {
            return None;
        }
        let column = self.column.as_ref()?;
        let panes = column.panes(cx);
        let closable = pane
            .read(cx)
            .group()
            .is_some_and(|group| group.read(cx).is_closable(cx));
        let zoomed = column.zoom.is_zoomed();
        let split = |placement| self.split_allowed(pane, placement, window, cx);
        // The panes beneath a zoomed one, or one on its way back to its slot,
        // are covered, so it has no neighbours.
        let covered = column.zoom.pane().is_some();
        let neighbours =
            Side::ALL.map(|side| !covered && self.neighbour_of(pane, side, cx).is_some());
        Some(PaneControls {
            split_right: split(gpui_base::Placement::Right),
            split_down: split(gpui_base::Placement::Bottom),
            close: panes.len() > 1 && (closable || zoomed),
            close_others: panes.len() > 1,
            zoom: panes.len() > 1,
            zoomed,
            neighbours,
            moves: pane.read(cx).session_id().is_some(),
        })
    }

    pub(super) fn acting_pane(&self) -> Option<Entity<ThreadPanel>> {
        Some(self.column.as_ref()?.current_pane.clone())
    }

    /// The thread of the pane last in use: the compact page's, and what the
    /// window as a whole is about.
    pub(super) fn page_thread(&self, cx: &App) -> Option<ThreadKey> {
        self.column.as_ref()?.page().read(cx).thread(cx)
    }

    /// The pane showing `thread`.
    pub(super) fn pane_showing(&self, thread: &ThreadKey, cx: &App) -> Option<Entity<ThreadPanel>> {
        self.column
            .as_ref()?
            .panes(cx)
            .into_iter()
            .find(|pane| pane.read(cx).thread(cx).as_ref() == Some(thread))
    }

    /// A pane over `host` on `placement`'s side of `from`, with the keyboard
    /// and waiting for a thread.
    pub(super) fn add_pane(
        &mut self,
        from: &Entity<ThreadPanel>,
        host: &HostKey,
        placement: gpui_base::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ThreadPanel>> {
        if self.compact(cx) || !self.split_allowed(from, placement, window, cx) {
            return None;
        }
        let column = self.column.as_ref()?;
        let node = from.read(cx).group()?.read(cx).node();
        let link = self.link_of(host)?;
        let (store, preview) = (link.store.clone(), link.preview.clone());
        let area = column.thread_panes.clone();
        let pane = self.new_pane(&store, &preview, window, cx);
        self.column.as_mut()?.zoom.zoom_out();
        let before = self.glide_before_edit(window, cx);
        area.update(cx, |area, cx| {
            // `DockArea::split_at` splits only a panel the area already
            // holds, and base has no other way to register one: the pane
            // joins the area, then moves beside `node`.
            area.add_panel(pane.clone(), DockPlacement::Center, None, window, cx);
            area.move_panel(
                PanelId::from(pane.entity_id()),
                InsertTarget::Split {
                    node,
                    placement,
                    size: None,
                },
                window,
                cx,
            );
        });
        self.glide_after_edit(before, cx);
        self.set_current_pane(&pane, cx);
        window.focus(&pane.focus_handle(cx), cx);
        cx.notify();
        Some(pane)
    }

    /// Split `from`: the new pane shows a fresh draft on `from`'s host, in
    /// the same project, and the thread split from stays open.
    pub(super) fn split_pane(
        &mut self,
        from: &Entity<ThreadPanel>,
        placement: gpui_base::Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (store, beside) = {
            let from = from.read(cx);
            (from.store().clone(), from.session_id().map(str::to_owned))
        };
        let host = store.read(cx).host_key();
        if self.add_pane(from, &host, placement, window, cx).is_none() {
            return;
        }
        store.update(cx, |store, cx| {
            store.start_draft_beside(beside.as_deref(), cx);
        });
    }

    /// Show `thread` in a pane of its own, on the first of `placements`
    /// beside `from` that the column has room for; the pane already showing
    /// it takes the keyboard instead. False when no pane shows it: there was no
    /// room, or the layout has no panes.
    pub(super) fn open_in_new_pane(
        &mut self,
        thread: ThreadKey,
        from: &Entity<ThreadPanel>,
        placements: &[gpui_base::Placement],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.compact(cx) {
            return false;
        }
        let Some(store) = self.link_of(&thread.host).map(|link| link.store.clone()) else {
            return false;
        };
        if self.pane_showing(&thread, cx).is_some() {
            store.update(cx, |store, cx| store.show_thread(thread.session_id, cx));
            return true;
        }
        let added = placements
            .iter()
            .find_map(|placement| self.add_pane(from, &thread.host, *placement, window, cx));
        if let Some(pane) = &added {
            self.show_in_pane(pane, &thread, true, window, cx);
        }
        added.is_some()
    }

    /// A thread dragged from the threads list landed on a pane: an edge opens
    /// it in a new pane there, the pane itself shows it in place of its own.
    pub(super) fn drop_thread(
        &mut self,
        thread: ThreadKey,
        target: gpui_base::dock::DropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = self.column.as_ref() else {
            return;
        };
        let Some(pane) = column.panes(cx).into_iter().find(|pane| {
            pane.read(cx)
                .group()
                .is_some_and(|group| group.read(cx).node() == target.node())
        }) else {
            return;
        };
        // The thread's ghost goes into the drop's placeholder, which shows
        // where the thread opens.
        let into = column.drop_landing.take();
        match target.placement() {
            Some(placement) => {
                if !self.open_in_new_pane(thread, &pane, &[placement], window, cx) {
                    return;
                }
            }
            None => {
                let pane = self.pane_showing(&thread, cx).unwrap_or(pane);
                self.show_in_pane(&pane, &thread, true, window, cx);
            }
        }
        if let Some(into) = into {
            crate::motion::drag_ghost::land_in(into, window, cx);
        }
    }

    /// Where a thread dragged over the column would land, in window
    /// coordinates: the part of the hovered pane the dock's drop names, if it
    /// lands.
    pub(super) fn drop_target(&self, window: &Window, cx: &App) -> Option<gpui::Bounds<Pixels>> {
        self.column.as_ref()?.panes(cx).iter().find_map(|pane| {
            let indicator = pane
                .read(cx)
                .group()?
                .read(cx)
                .context(cx)
                .drop_indicator()?;
            let (bounds, to) = (indicator.bounds(), indicator.to());
            self.drop_lands(bounds, indicator.placement(), window, cx)
                .then(|| gpui::Bounds::new(bounds.origin + to.origin(), to.size()))
        })
    }

    /// Whether a thread dragged over the pane painted at `bounds` would land:
    /// one a pane already shows only takes the focus, and an edge needs room
    /// to split.
    pub(super) fn drop_lands(
        &self,
        bounds: gpui::Bounds<Pixels>,
        placement: Option<gpui_base::Placement>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(column) = self.column.as_ref() else {
            return false;
        };
        let Some(thread) = self.sidebar.read(cx).dragged_session() else {
            return false;
        };
        // The panes beneath a zoomed one are covered, and it is in no tab
        // group to drop on.
        if column.zoom.pane().is_some() || self.pane_showing(thread, cx).is_some() {
            return false;
        }
        column
            .panes(cx)
            .into_iter()
            .find(|pane| pane.read(cx).bounds().contains(&bounds.center()))
            .is_some_and(|pane| {
                placement.is_none_or(|placement| self.split_allowed(&pane, placement, window, cx))
            })
    }

    /// Close `pane`; its thread closes and stays in the threads list. The
    /// keyboard moves to a neighbour at once and the pane glides shut. The
    /// last pane stays.
    pub(super) fn close_pane(
        &mut self,
        pane: &Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        self.close_pane_into(
            pane,
            |panes, at| {
                // Base hands the keyboard on only when it removes the group,
                // which the glide puts off; this is the order it would pick in:
                // the panes after this one in the layout, then those before it,
                // nearest first.
                panes[at + 1..]
                    .first()
                    .or_else(|| panes[..at].last())
                    .cloned()
            },
            window,
            cx,
        );
    }

    /// Close `pane` as [`Self::close_pane`] does, handing the keyboard to the
    /// pane `next` picks from the layout's panes and `pane`'s place among
    /// them. False where `pane` cannot close: it is the last one, or its
    /// group will not close.
    pub(super) fn close_pane_into(
        &mut self,
        pane: &Entity<ThreadPanel>,
        next: impl FnOnce(&[Entity<ThreadPanel>], usize) -> Option<Entity<ThreadPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(column) = self.column.as_mut() else {
            return false;
        };
        let Some(group) = pane.read(cx).group() else {
            return false;
        };
        column.zoom.zoom_out();
        let panes = column.panes(cx);
        if panes.len() < 2 || !group.read(cx).is_closable(cx) {
            return false;
        }
        let at = panes.iter().position(|held| held == pane).unwrap_or(0);
        let next = next(&panes, at);
        self.glide_out(pane, cx);
        if let Some(next) = next {
            window.focus(&next.focus_handle(cx), cx);
            self.pane_entered(next, cx);
        }
        true
    }

    /// Start `pane` leaving the column: it turns inert and its slot glides
    /// shut, after which [`Self::remove_closed`] takes it out.
    pub(super) fn glide_out(&mut self, pane: &Entity<ThreadPanel>, cx: &mut Context<Self>) {
        pane.update(cx, |pane, cx| pane.set_closing(cx));
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let area = column.thread_panes.read(cx);
        let size = area.bounds().size;
        if let Some(tree) = area.layout(DockPlacement::Center).cloned() {
            column
                .glides
                .close(&tree, size, PanelId::from(pane.entity_id()));
        }
        cx.notify();
    }

    /// Take out the closing panes whose split has stopped gliding, closing
    /// the threads they showed unless a pane shows them again by now.
    pub(super) fn remove_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.column.as_ref() else {
            return;
        };
        let area = column.thread_panes.clone();
        let panes = column.panes(cx);
        for pane in column.closed_panes(cx) {
            let (store, thread) = {
                let pane = pane.read(cx);
                (pane.store().clone(), pane.thread(cx))
            };
            area.update(cx, |area, cx| area.remove_panel(pane, window, cx));
            if let Some(thread) = thread
                && store.read(cx).is_thread_open(&thread.session_id)
                && self.pane_showing(&thread, cx).is_none()
                && !drawn_beside(&panes, &thread, cx)
            {
                store.update(cx, |store, cx| store.close_thread(&thread.session_id, cx));
            }
        }
    }

    /// Before an edit that adds panes: hand the dock the sizes the glides are
    /// heading for, which is what it divides, and note the layout the new
    /// panes glide out of.
    pub(super) fn glide_before_edit(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::panes::Before> {
        let column = self.column.as_ref()?;
        let area = column.thread_panes.clone();
        let (targets, before) = {
            let area = area.read(cx);
            let tree = area.layout(DockPlacement::Center)?;
            let size = area.bounds().size;
            (
                column.glides.targets(tree, size),
                column.glides.before(tree, size),
            )
        };
        area.update(cx, |area, cx| {
            for (node, sizes) in targets {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        Some(before)
    }

    /// After an edit that added panes: glide them in out of `before`.
    pub(super) fn glide_after_edit(
        &mut self,
        before: Option<crate::panes::Before>,
        cx: &mut Context<Self>,
    ) {
        let Some(before) = before else {
            return;
        };
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let area = column.thread_panes.read(cx);
        let size = area.bounds().size;
        if let Some(tree) = area.layout(DockPlacement::Center).cloned() {
            column.glides.begin(&before, &tree, size);
        }
        cx.notify();
    }

    /// This frame of the column's glides: each gliding split's sizes go to
    /// the dock, each pane's content is held at the size its slot glides
    /// toward, and the closing panes whose split has landed are taken out
    /// before the dock draws them. A split's handle under the pointer stops
    /// that split's glide where it is, and the compact layout, which draws no
    /// panes, lands them. A window that jumped to another size since the
    /// last frame glides the seams there (see [`crate::panes::Glides::resize`]);
    /// one whose edge is dragged is followed at once.
    pub(super) fn glide_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let column_shown = !compact && self.window_state.read(cx).route() == Route::Chat;
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let handle_held = column.split_handle_held.take();
        if handle_held.is_none() {
            column.held_split = None;
        }
        let seen = (
            window.viewport_size(),
            (window.is_maximized(), window.is_fullscreen()),
        );
        let jumped = std::mem::replace(&mut column.window_seen, column_shown.then_some(seen))
            .filter(|_| column_shown)
            .filter(|(size, state)| crate::panes::window_jumped(*size, seen.0, *state != seen.1))
            .map(|(size, _)| (seen.0.width - size.width, seen.0.height - size.height));
        let panes = column.all_panes(cx);
        let closing = panes.iter().any(|pane| pane.read(cx).is_closing());
        if column.glides.is_idle() && !closing && jumped.is_none() {
            return;
        }
        let area = column.thread_panes.clone();
        let (tree, size) = {
            let area = area.read(cx);
            (
                area.layout(DockPlacement::Center).cloned(),
                area.bounds().size,
            )
        };
        let Some(tree) = tree else {
            return;
        };
        // The column takes what the window gained or lost; it is laid out
        // at its new size only later in this frame.
        if let Some((wider, taller)) = jumped
            && size.width > px(0.)
        {
            let now = gpui::size(size.width + wider, size.height + taller);
            column.glides.resize(&tree, size, now);
        }
        if let Some((axis, at)) = handle_held
            && column.held_split.is_none()
        {
            let painted = |panel| {
                panes
                    .iter()
                    .find(|pane| PanelId::from(pane.entity_id()) == panel)
                    .map(|pane| pane.read(cx).bounds())
            };
            column.held_split = crate::panes::held_split(tree.root(), axis, at, &painted);
        }
        if let Some(split) = column.held_split {
            column.glides.stop(split);
        }
        let frame = if compact {
            column.glides.land(&tree, size)
        } else {
            column.glides.sample(&tree, size, window, cx)
        };
        area.update(cx, |area, cx| {
            for (node, sizes) in frame.sizes {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        for pane in &panes {
            let held = frame
                .holds
                .get(&PanelId::from(pane.entity_id()))
                .copied()
                .unwrap_or_default();
            ThreadPanel::set_held(pane, held, cx);
        }
        if closing {
            self.remove_closed(window, cx);
        }
    }

    /// Share the split whose handle is at `at`, along `axis`, evenly among
    /// its slots, which is how the column divides before any drag.
    pub(super) fn even_split(
        &mut self,
        axis: gpui::Axis,
        at: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let area = column.thread_panes.clone();
        let (tree, size) = {
            let area = area.read(cx);
            (
                area.layout(DockPlacement::Center).cloned(),
                area.bounds().size,
            )
        };
        let Some(tree) = tree else {
            return;
        };
        let panes = column.all_panes(cx);
        let painted = |panel| {
            panes
                .iter()
                .find(|pane| PanelId::from(pane.entity_id()) == panel)
                .map(|pane| pane.read(cx).bounds())
        };
        let Some(split) = crate::panes::held_split(tree.root(), axis, at, &painted) else {
            return;
        };
        let Some(sizes) = column.glides.even(&tree, size, split) else {
            return;
        };
        column.glides.stop(split);
        area.update(cx, |area, cx| {
            area.set_split_sizes(split, sizes, window, cx)
        });
    }

    /// Land every glide at once and take out the panes closing, for an edit
    /// that does not divide what they leave: a zoom, a layout put in.
    pub(super) fn land_glides(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let area = column.thread_panes.clone();
        let (tree, size) = {
            let area = area.read(cx);
            (
                area.layout(DockPlacement::Center).cloned(),
                area.bounds().size,
            )
        };
        let Some(tree) = tree else {
            return;
        };
        let frame = column.glides.land(&tree, size);
        let panes = column.all_panes(cx);
        area.update(cx, |area, cx| {
            for (node, sizes) in frame.sizes {
                area.set_split_sizes(node, sizes, window, cx);
            }
        });
        for pane in panes {
            ThreadPanel::set_held(&pane, Default::default(), cx);
        }
        self.remove_closed(window, cx);
    }

    /// Zoom the acting pane over the column (see [`crate::panes::Zoom`]), or
    /// turn the zoomed one back into its slot from where it is; running
    /// glides land first.
    pub(super) fn toggle_zoom(
        &mut self,
        pane: Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        let Some(column) = self.column.as_ref() else {
            return;
        };
        let zoomed = column.zoom.pane() == Some(&pane);
        if !zoomed && column.panes(cx).len() < 2 {
            return;
        }
        self.land_glides(window, cx);
        let Some(column) = self.column.as_mut() else {
            return;
        };
        if zoomed {
            column.zoom.toggle();
        } else {
            column.zoom.zoom_in(pane.clone(), cx);
            self.set_current_pane(&pane, cx);
        }
        cx.notify();
    }

    /// This frame of the zoom. A zoomed pane that left the column, or is the
    /// only one left in it, is back in its slot at once, and the compact
    /// layout, which draws no panes, zooms none.
    pub(super) fn zoom_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let Some(column) = self.column.as_mut() else {
            return;
        };
        let gone = column.zoom.pane().is_some_and(|pane| {
            let panes = column.all_panes(cx);
            panes.len() < 2 || !panes.contains(pane)
        });
        if compact || gone {
            column.zoom.land();
        }
        column.zoom.sample(window, cx);
    }

    /// The chat column: the dock of panes on the reading surface, faded
    /// while a layout goes in, under the pane zoomed over it. The panes
    /// fading out to be replaced take no pointer. The column runs along the
    /// window's top, bottom and right, and its left once the sidebar is
    /// collapsed; the zoomed pane covers the dock, not the seam around it.
    pub(super) fn render_chat_column(
        &self,
        collapsed: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Some(column) = self.column.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let area = column.thread_panes.read(cx).bounds();
        let surface = div()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .shadow_sm();
        crate::window_seam::inset_from_seam(
            surface,
            "chat-column-seam",
            gpui::Edges {
                left: collapsed,
                ..gpui::Edges::all(true)
            },
            window,
            cx,
        )
        .when_some(column.layout_swap.opacity(), |surface, opacity| {
            surface.opacity(opacity)
        })
        .child(
            div()
                .relative()
                .size_full()
                .child(column.thread_panes.clone())
                .children(column.zoom.render(area, cx))
                .when(column.layout_swap.leaving(), |panes| {
                    panes.child(crate::motion::shield())
                }),
        )
        .into_any_element()
    }

    /// Spatial moves go to the nearest pane painted on that side; Next and
    /// Previous follow the layout's order and wrap. While a pane is drawn
    /// over the column the others are covered, so nothing moves.
    ///
    /// Written here because gpui-base's dock has no keyboard navigation
    /// between groups; it goes once the dock offers one.
    pub(super) fn focus_pane(
        &mut self,
        direction: FocusPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        let Some(column) = self.column.as_ref() else {
            return;
        };
        if column.zoom.pane().is_some() {
            return;
        }
        let Some(from) = self.acting_pane() else {
            return;
        };
        let panes = column.panes(cx);
        let Some(at) = panes.iter().position(|pane| *pane == from) else {
            return;
        };
        let target = match direction.side() {
            None if direction == FocusPane::Next => panes.get((at + 1) % panes.len()).cloned(),
            None => panes.get((at + panes.len() - 1) % panes.len()).cloned(),
            Some(side) => self.neighbour_of(&from, side, cx),
        };
        if let Some(target) = target
            && target != from
        {
            window.focus(&target.focus_handle(cx), cx);
        }
    }

    /// The pane nearest `pane` on `side`, as they were last painted.
    pub(super) fn neighbour_of(
        &self,
        pane: &Entity<ThreadPanel>,
        side: Side,
        cx: &App,
    ) -> Option<Entity<ThreadPanel>> {
        let panes = self.column.as_ref()?.panes(cx);
        let bounds: Vec<_> = panes.iter().map(|pane| pane.read(cx).bounds()).collect();
        let at = panes.iter().position(|held| held == pane)?;
        crate::panes::neighbour(bounds[at], &bounds, side).map(|ix| panes[ix].clone())
    }

    /// Close every pane but `pane`, as [`Self::close_pane`] closes one: each
    /// glides shut and its thread closes once it is taken out.
    pub(super) fn close_other_panes(
        &mut self,
        pane: Entity<ThreadPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx) {
            return;
        }
        let Some(column) = self.column.as_mut() else {
            return;
        };
        column.zoom.zoom_out();
        for other in column.panes(cx) {
            if other != pane {
                self.glide_out(&other, cx);
            }
        }
        window.focus(&pane.focus_handle(cx), cx);
        self.pane_entered(pane, cx);
    }

    /// `from` and its neighbour on `side` exchange their conversations, each with its own composer, scroll and expanded rows,
    /// so a half-written message stays with its thread, and, of different
    /// hosts, their drawers; the panes and their slots stay where they are. The keyboard stays in `from`, or with
    /// `follow` goes with its thread to the neighbour.
    /// Neither conversation closes. Each conversation shown travels from
    /// where it was drawn into its new pane (see
    /// [`ThreadPanel::replace_chat`]); a waiting pane's has nothing shown to
    /// carry.
    pub(super) fn swap_pane(
        &mut self,
        from: Entity<ThreadPanel>,
        side: Side,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.compact(cx)
            || self
                .column
                .as_ref()
                .is_none_or(|column| column.zoom.pane().is_some())
        {
            return;
        }
        let Some(to) = self.neighbour_of(&from, side, cx) else {
            return;
        };
        let drawn = |pane: &Entity<ThreadPanel>| {
            let pane = pane.read(cx);
            pane.session_id().is_some().then(|| pane.chat_drawn(cx))
        };
        let (ours_at, theirs_at) = (drawn(&from), drawn(&to));
        let hosts_differ = from.read(cx).store() != to.read(cx).store();
        let chat = from.read(cx).chat();
        let theirs = to.update(cx, |to, cx| to.replace_chat(chat, ours_at, cx));
        from.update(cx, |from, cx| from.replace_chat(theirs, theirs_at, cx));
        if hosts_differ {
            ThreadPanel::exchange_drawers(&from, &to, cx);
        }
        let keeper = if follow { to } else { from };
        window.focus(&keeper.focus_handle(cx), cx);
        self.pane_entered(keeper, cx);
    }

    /// What a pane command does to `pane`.
    pub(super) fn run_on_pane(
        &mut self,
        pane: Entity<ThreadPanel>,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaneCommand::Split(placement) => self.split_pane(&pane, placement, window, cx),
            PaneCommand::Close => self.close_pane(&pane, window, cx),
            PaneCommand::CloseOthers => self.close_other_panes(pane, window, cx),
            PaneCommand::Zoom => self.toggle_zoom(pane, window, cx),
            PaneCommand::Swap(side) => self.swap_pane(pane, side, false, window, cx),
            // `pane`'s thread trades places with its neighbour's and the
            // keyboard goes with it. A pane waiting for a thread has none.
            PaneCommand::Move(side) if pane.read(cx).session_id().is_some() => {
                self.swap_pane(pane, side, true, window, cx)
            }
            PaneCommand::Move(_) => {}
        }
    }

    /// The thread the pane `panel` shows.
    pub(super) fn panel_thread(&self, panel: PanelId, cx: &App) -> Option<ThreadKey> {
        self.column
            .as_ref()?
            .thread_panes
            .read(cx)
            .panel(panel)?
            .view()
            .downcast::<ThreadPanel>()
            .ok()?
            .read(cx)
            .thread(cx)
    }

    /// The narrowest the chat column can be laid out as `tree`, whose panels
    /// show the threads `thread_of` names: its panes' headers at their
    /// narrowest, side by side, and never narrower than a pane alone. Only
    /// the pane in the column's top-left corner clears the traffic lights
    /// over a collapsed sidebar.
    pub(super) fn column_min_width(
        &self,
        tree: &gpui_base::dock::PaneTree,
        thread_of: &dyn Fn(PanelId) -> Option<ThreadKey>,
        collapsed: bool,
        fullscreen: bool,
        cx: &App,
    ) -> f32 {
        let Some(current) = self.current_link(cx) else {
            return CHAT_MIN_WIDTH;
        };
        let floor = if tree.panels().nth(1).is_some() {
            PANE_MIN_WIDTH
        } else {
            CHAT_MIN_WIDTH
        };
        let traffic_lights =
            collapsed && !fullscreen && crate::window_caption::has_traffic_lights(cx);
        let pane_min = |panel: PanelId, edges: crate::chat::WindowEdges| {
            let thread = thread_of(panel);
            let link = thread
                .as_ref()
                .and_then(|thread| self.link_of(&thread.host))
                .unwrap_or(current);
            floor
                + crate::chat::header_extra_width_for(
                    link.store.read(cx),
                    thread.as_ref().map(|thread| thread.session_id.as_str()),
                    traffic_lights && edges.left,
                    false,
                )
        };
        crate::panes::min_width(tree.root(), crate::chat::WindowEdges::BOTH, &pane_min)
            .max(CHAT_MIN_WIDTH)
    }

    /// Whether `pane` may split along `placement`: below the cap, and with
    /// the column's minimum after the split still fitting beside the sidebar.
    pub(super) fn split_allowed(
        &self,
        pane: &Entity<ThreadPanel>,
        placement: gpui_base::Placement,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(column) = self.column.as_ref() else {
            return false;
        };
        if self.compact(cx) || column.panes(cx).len() >= crate::panes::MAX_PANES {
            return false;
        }
        let Some(mut tree) = column.final_layout(cx) else {
            return false;
        };
        // From the tree rather than the pane's group: the drop indicator asks
        // while the group is rendering.
        let Some(node) = tree.find_panel_node(PanelId::from(pane.entity_id())) else {
            return false;
        };
        tree.split(node, PanelId::from_u64(u64::MAX), placement, None);
        self.column_fits(&tree, &|panel| self.panel_thread(panel, cx), window, cx)
    }

    /// Whether `tree`'s minimum width, its panels showing the threads
    /// `thread_of` names, fits beside the sidebar.
    pub(super) fn column_fits(
        &self,
        tree: &gpui_base::dock::PaneTree,
        thread_of: &dyn Fn(PanelId) -> Option<ThreadKey>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let Some(column) = self.column.as_ref() else {
            return false;
        };
        let collapsed = DevicePreferences::sidebar_collapsed(cx);
        let sidebar = if collapsed {
            px(0.)
        } else {
            column.sidebar_width.get()
        };
        px(self.column_min_width(tree, thread_of, collapsed, window.is_fullscreen(), cx))
            <= window.viewport_size().width - sidebar
    }

    /// The current thread's agents not yet settled, newest first: what
    /// [`PaneLayout::SpreadAgents`] puts beside it.
    pub(super) fn spread_agents(&self, cx: &App) -> Option<Vec<String>> {
        let current_pane = &self.column.as_ref()?.current_pane;
        let parent = current_pane.read(cx).session_id()?;
        let store = current_pane.read(cx).store().read(cx);
        let mut agents: Vec<_> = store
            .sidebar_sessions()
            .iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(parent)
                    && meta.archived_at.is_none()
                    && !meta.is_settled()
            })
            .collect();
        agents.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        Some(agents.into_iter().map(|meta| meta.id.clone()).collect())
    }

    /// `layout` as the panes there would fill it: its tree, and what fills
    /// each of the tree's panels. `None` where the column has no room for it,
    /// or it has nothing to show: a single pane alone, a thread without
    /// agents.
    pub(super) fn plan_layout(
        &self,
        layout: PaneLayout,
        window: &Window,
        cx: &App,
    ) -> Option<(gpui_base::dock::PaneTree, Vec<(PanelId, Slot)>)> {
        if self.compact(cx) {
            return None;
        }
        let column = self.column.as_ref()?;
        let current_pane = column.current_pane.clone();
        let panes = column.panes(cx);
        let first = std::iter::once(Slot::Pane(current_pane.clone()));
        let (fills, fewest): (Vec<_>, _) = match layout {
            PaneLayout::SpreadAgents => {
                let store = current_pane.read(cx).store().read(cx);
                let agents = self.spread_agents(cx)?.into_iter().map(|id| {
                    let thread = store.thread_key(&id);
                    self.pane_showing(&thread, cx)
                        .map_or(Slot::Thread(thread), Slot::Pane)
                });
                (
                    first.chain(agents).take(crate::panes::MAX_PANES).collect(),
                    2,
                )
            }
            PaneLayout::Single if panes.len() < 2 => return None,
            layout => {
                let count = layout.splits(0).len() + 1;
                let others = panes
                    .iter()
                    .filter(|pane| **pane != current_pane)
                    .cloned()
                    .map(Slot::Pane);
                let fills = first
                    .chain(others)
                    .chain(std::iter::repeat(Slot::Waiting))
                    .take(count)
                    .collect();
                (fills, count)
            }
        };
        let mut tree = column
            .thread_panes
            .read(cx)
            .layout(DockPlacement::Center)?
            .clone();
        let anchor = PanelId::from(current_pane.entity_id());
        for panel in tree.panels().collect::<Vec<_>>() {
            if panel != anchor {
                tree.remove_panel(panel);
            }
        }
        (fewest..=fills.len()).rev().find_map(|count| {
            let mut tree = tree.clone();
            let mut ids = vec![anchor];
            for (ix, (from, placement)) in layout.splits(count).into_iter().enumerate() {
                let node = tree.find_panel_node(ids[from])?;
                let id = PanelId::from_u64(u64::MAX - ix as u64);
                tree.split(node, id, placement, None);
                ids.push(id);
            }
            // Every split opens right of or below its pane, so the last
            // input leads the tree's order.
            let slots: Vec<_> = tree.panels().zip(fills.iter().cloned()).collect();
            let thread_of = |panel: PanelId| match slots.iter().find(|(id, _)| *id == panel)? {
                (_, Slot::Pane(pane)) => pane.read(cx).thread(cx),
                (_, Slot::Thread(thread)) => Some(thread.clone()),
                (_, Slot::Waiting) => None,
            };
            self.column_fits(&tree, &thread_of, window, cx)
                .then_some((tree, slots))
        })
    }

    /// Choose `layout` for the column. It goes in as a layout swap (see
    /// [`crate::panes::LayoutSwap`]): the column fades out, the layout is put
    /// in at the bottom of the fade and the column fades back in. A layout that
    /// would leave every pane where it is drawn, none zoomed, changes
    /// nothing. The current pane keeps the keyboard.
    pub(super) fn apply_layout(
        &mut self,
        layout: PaneLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(plan) = self.plan_layout(layout, window, cx) else {
            return;
        };
        if layout == PaneLayout::SpreadAgents {
            // The current thread takes one pane.
            let left_out = self.spread_agents(cx).map_or(0, |agents| {
                agents.len().saturating_sub(crate::panes::MAX_PANES - 1)
            });
            if left_out > 0 {
                window.push_notification(
                    Notification::info(if left_out == 1 {
                        crate::tr!("panes.spread_agents_left_out_one").into_owned()
                    } else {
                        crate::tr!("panes.spread_agents_left_out", count = left_out).into_owned()
                    }),
                    cx,
                );
            }
        }
        let unchanged = self.layout_unchanged(&plan, cx);
        let Some(column) = self.column.as_mut() else {
            return;
        };
        if !(unchanged && column.zoom.pane().is_none() && column.layout_swap.is_idle()) {
            column.layout_swap.choose(layout);
        }
        let current_pane = column.current_pane.clone();
        self.pane_entered(current_pane.clone(), cx);
        window.focus(&current_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// This frame of a layout swap: the layout goes in at the bottom of the
    /// fade. The compact layout draws no panes, so a swap stops there.
    pub(super) fn swap_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.compact(cx);
        let Some(column) = self.column.as_mut() else {
            return;
        };
        if compact {
            column.layout_swap.land();
            return;
        }
        if let Some(layout) = column.layout_swap.sample(window, cx) {
            self.install_layout(layout, window, cx);
        }
    }

    /// Whether `plan` would leave every pane where the dock lays it out:
    /// nothing gliding or closing, and the same panes in the same splits at
    /// the same shares.
    pub(super) fn layout_unchanged(
        &self,
        (tree, slots): &(gpui_base::dock::PaneTree, Vec<(PanelId, Slot)>),
        cx: &App,
    ) -> bool {
        let Some(column) = self.column.as_ref() else {
            return false;
        };
        let area = column.thread_panes.read(cx);
        let Some(now) = area.layout(DockPlacement::Center) else {
            return false;
        };
        if !column.glides.is_idle()
            || column
                .all_panes(cx)
                .iter()
                .any(|pane| pane.read(cx).is_closing())
        {
            return false;
        }
        let shown = |planned: PanelId| match slots.iter().find(|(panel, _)| *panel == planned) {
            Some((_, Slot::Pane(pane))) => Some(PanelId::from(pane.entity_id())),
            _ => None,
        };
        crate::panes::same_layout(
            now.root(),
            tree.root(),
            &|now, planned| shown(planned) == Some(now),
            area.bounds().size,
        )
    }

    /// Put `layout` in at once, as the panes are now: running glides land,
    /// the zoomed pane is back in its slot, and the panes the layout leaves
    /// out close with their threads; the current pane keeps the keyboard.
    pub(super) fn install_layout(
        &mut self,
        layout: PaneLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.land_glides(window, cx);
        let Some(plan) = self.plan_layout(layout, window, cx) else {
            return;
        };
        let Some(column) = self.column.as_mut() else {
            return;
        };
        column.zoom.land();
        if self.layout_unchanged(&plan, cx) {
            return;
        }
        let (tree, slots) = plan;
        let (Some(column), Some(link)) = (self.column.as_ref(), self.current_link(cx)) else {
            return;
        };
        let (store, preview) = (link.store.clone(), link.preview.clone());
        let area = column.thread_panes.clone();
        let current_pane = column.current_pane.clone();
        let before = column.panes(cx);
        let mut panes = HashMap::new();
        let mut threads = Vec::new();
        for (panel, slot) in slots {
            let pane = match slot {
                Slot::Pane(pane) => pane,
                Slot::Waiting | Slot::Thread(_) => {
                    let pane = self.new_pane(&store, &preview, window, cx);
                    if let Slot::Thread(thread) = slot {
                        threads.push((pane.clone(), thread));
                    }
                    pane
                }
            };
            panes.insert(panel, pane);
        }
        area.update(cx, |area, cx| {
            area.set_center(
                crate::panes::layout_of(tree.root(), &|panel| panes.get(&panel).cloned()),
                window,
                cx,
            );
        });
        let kept: Vec<_> = panes.values().cloned().collect();
        for pane in before {
            if !kept.contains(&pane)
                && let Some(thread) = pane.read(cx).thread(cx)
                && !drawn_beside(&kept, &thread, cx)
            {
                pane.read(cx)
                    .store()
                    .clone()
                    .update(cx, |store, cx| store.close_thread(&thread.session_id, cx));
            }
        }
        for (pane, thread) in threads {
            self.bind_pane(&pane, &thread, window, cx);
        }
        self.pane_entered(current_pane.clone(), cx);
        window.focus(&current_pane.focus_handle(cx), cx);
        cx.notify();
    }

    /// Tell each pane whether it shares the column and how a zoom leaves it,
    /// and its conversation which window edges its header runs along and
    /// whether its header holds the pane's close button. The layout's tree
    /// decides the first and the last, so they hold from the frame a split
    /// lands; a pane drawn over the column is laid out as the whole column.
    ///
    /// The close button's fade is sampled here, keyed by the pane, rather
    /// than in the header: a split re-parents the pane's group in its frame,
    /// which would restart any keyed state in the header (see
    /// `panes::ThreadPanes`), so the button would snap in.
    ///
    /// Each view is updated only when what it is handed differs from what it
    /// holds: in gpui-fast an update while the window draws counts as a change
    /// to every view that read the entity, so handing a view what it already
    /// holds would draw all of them again next frame.
    pub(super) fn sync_panes(&mut self, window: &mut Window, cx: &mut App) {
        let Some(column) = self.column.as_ref() else {
            return;
        };
        let compact = self.compact(cx);
        for pane in column.all_panes(cx) {
            let zoom = column.zoom.of(&pane);
            ThreadPanel::set_zoom(&pane, zoom, cx);
        }
        let tree = column.final_layout(cx);
        let edges: Vec<_> = column
            .panes(cx)
            .into_iter()
            .map(|pane| {
                let overlaid = column.zoom.pane() == Some(&pane);
                let edges = tree
                    .as_ref()
                    .filter(|_| !compact && !overlaid)
                    .and_then(|tree| {
                        crate::panes::edges_of(
                            tree.root(),
                            crate::chat::WindowEdges::BOTH,
                            PanelId::from(pane.entity_id()),
                        )
                    })
                    .unwrap_or(crate::chat::WindowEdges::BOTH);
                (pane, edges)
            })
            .collect();
        let among_others = edges.len() > 1;
        // With panes of several hosts, each header names its own: the host
        // a pane's commands go to is no longer the one beside it.
        let several = edges
            .iter()
            .any(|(pane, _)| pane.read(cx).store() != column.current_pane.read(cx).store());
        let mut handed = Vec::with_capacity(edges.len());
        for (pane, edges) in edges {
            let recedes = among_others && !compact && pane != column.current_pane;
            let close = crate::motion::shown(
                gpui::ElementId::NamedInteger("pane-close".into(), pane.entity_id().as_u64()),
                (among_others && !compact).then(|| pane.downgrade()),
                SHOW,
                window,
                cx,
            );
            ThreadPanel::set_among_others(&pane, among_others, cx);
            let controls = self.pane_controls(&pane, window, cx);
            let host = several.then(|| pane.read(cx).store().read(cx).host_title());
            let chat = pane.read(cx).chat();
            handed.push((
                chat,
                HandedToChat {
                    edges,
                    recedes,
                    close,
                    controls,
                    host,
                },
            ));
        }
        let Some(column) = self.column.as_mut() else {
            return;
        };
        column
            .handed
            .retain(|chat, _| handed.iter().any(|(handed, _)| handed.entity_id() == *chat));
        for (chat, now) in handed {
            let last = column.handed.get(&chat.entity_id());
            if last.is_none_or(|last| last.edges != now.edges) {
                ChatView::set_edges(&chat, now.edges, cx);
            }
            if last.is_none_or(|last| last.recedes != now.recedes) {
                ChatView::set_title_recedes(&chat, now.recedes, cx);
            }
            if last.is_none_or(|last| last.close != now.close || last.controls != now.controls) {
                ChatView::set_pane(&chat, now.close.clone(), now.controls, cx);
            }
            if last.is_none_or(|last| last.host != now.host) {
                ChatView::set_named_host(&chat, now.host.clone(), cx);
            }
            column.handed.insert(chat.entity_id(), now);
        }
        let Some(column) = self.column.as_ref() else {
            return;
        };
        let shown = self.shown_threads(tree.as_ref(), cx);
        let hovered = self.sidebar.read(cx).hovered_thread();
        for pane in column.panes(cx) {
            let hovered = shown.map.iter().any(|cell| {
                cell.pane == pane.entity_id() && hovered.is_some() && cell.row == hovered
            });
            ThreadPanel::set_row_hovered(&pane, hovered, cx);
        }
        SessionsSidebar::set_shown(&self.sidebar, shown, cx);
    }

    /// What the threads list marks: nothing on a phone, nor on a route
    /// without the chat column, and of a zoomed column only the zoomed pane,
    /// the others being covered.
    /// `tree` is the column's layout without the panes closing.
    pub(super) fn shown_threads(
        &self,
        tree: Option<&gpui_base::dock::PaneTree>,
        cx: &App,
    ) -> crate::sidebar::ShownThreads {
        let Some(column) = self.column.as_ref() else {
            return Default::default();
        };
        if self.compact(cx) || self.window_state.read(cx).route() != Route::Chat {
            return Default::default();
        }
        let row = |pane: &Entity<ThreadPanel>| {
            let pane = pane.read(cx);
            let store = pane.store().read(cx);
            Some(store.thread_key(&store.roster_row(pane.session_id()?)))
        };
        let panes = match column.zoom.pane().filter(|_| column.zoom.is_zoomed()) {
            Some(zoomed) => vec![zoomed.clone()],
            None => column.panes(cx),
        };
        let rows: Vec<_> = panes.iter().map(|pane| (pane, row(pane))).collect();
        let bounds = gpui::Bounds::new(gpui::Point::default(), {
            column.thread_panes.read(cx).bounds().size
        });
        let mut rects = Vec::new();
        if let Some(tree) = tree.filter(|_| panes.len() > 1) {
            crate::panes::pane_rects(tree.root(), bounds, &mut rects);
        }
        let share = |of: Pixels, whole: Pixels| of / whole.max(px(1.));
        let map = rects
            .into_iter()
            .filter_map(|(panel, rect)| {
                let (pane, row) = rows
                    .iter()
                    .find(|(pane, _)| PanelId::from(pane.entity_id()) == panel)?;
                Some(crate::sidebar::MapCell {
                    pane: pane.entity_id(),
                    row: row.clone(),
                    rect: gpui::bounds(
                        gpui::point(
                            share(rect.left(), bounds.size.width),
                            share(rect.top(), bounds.size.height),
                        ),
                        gpui::size(
                            share(rect.size.width, bounds.size.width),
                            share(rect.size.height, bounds.size.height),
                        ),
                    ),
                })
            })
            .collect();
        let current = match rows.iter().find(|(pane, _)| **pane == column.current_pane) {
            Some((_, row)) => row.clone(),
            None => row(&column.current_pane),
        };
        crate::sidebar::ShownThreads {
            current,
            shown: rows.into_iter().filter_map(|(_, row)| row).collect(),
            map,
        }
    }
}
