use super::*;

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

impl AppShell {
    pub(super) fn compact(&self, cx: &App) -> bool {
        self.window_state.read(cx).compact
    }

    pub(crate) fn wake_connection(&self, wake: tcode_client::recovery::Wake) {
        for link in self.links.iter().filter_map(Link::link) {
            link.wake(wake.clone());
        }
    }

    /// Recompute the layout from the window. Only a flip does anything, and a
    /// flip reconciles navigation to where the state already is — it is not a
    /// Back gesture and must not animate like one.
    pub(super) fn sync_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = crate::window_seam::window_is_compact(window, cx);
        // Read first: an update while the window draws counts as a change to
        // every view that read the state, so they would all be drawn again
        // next frame though nothing changed.
        let flipped = self.compact(cx) != compact
            && self
                .window_state
                .update(cx, |state, cx| state.set_compact(compact, cx));
        if flipped {
            self.layout_flipped = true;
            self.move_edge_back(TouchPhase::Cancelled, window, cx);
            self.page_back = None;
            // The wide layout has no pages: an agent stays in its pane alone.
            if !compact {
                self.drop_leads(0, cx);
                self.release_leaving(cx);
            }
            // The visit survives the resize; only a thread selected in the wide
            // split — which is not a page there — has to become one here.
            if compact
                && self.destination(cx) == Destination::Threads
                && self.page_thread(cx).is_some()
            {
                self.window_state
                    .update(cx, |state, cx| state.go(Destination::Thread, cx));
            }
            self.sync_nav(NavMotion::Immediate, cx);
            cx.notify();
        }
    }

    /// `element` as the part `key` of the window's layout, which glides from
    /// where the other layout drew it when the window turns compact or wide.
    pub(super) fn layout_part(&self, key: &'static str, element: impl IntoElement) -> AnyElement {
        match (&self.layout_reflow, &self.departing) {
            (Some(reflow), None) => reflow.part(key, element),
            _ => element.into_any_element(),
        }
    }

    pub(super) fn destination(&self, cx: &App) -> Destination {
        match &self.departing {
            Some(departing) => departing.destination,
            None => self.window_state.read(cx).destination(),
        }
    }

    /// Mirror the window's navigation history into the compact stack: pop back
    /// to what the two still share, then push the rest. One view per history
    /// entry, so the same destination visited twice is two independent pages.
    pub(super) fn sync_nav(&mut self, motion: NavMotion, cx: &mut Context<Self>) {
        let history = self.window_state.read(cx).history().to_vec();
        if self.mounted != history {
            self.page_back = None;
            self.pages_changing.set(motion == NavMotion::Animated);
        }
        if self.mounted != history
            && let Some(swipe) = &mut self.back_swipe
        {
            swipe.finished = true;
            if let Some(sheet) = &swipe.sheet {
                sheet.update(cx, |sheet, cx| {
                    sheet.dragging = false;
                    sheet.progress = 0.;
                    App::notify(cx, sheet.parent);
                });
            }
        }
        let common = self
            .mounted
            .iter()
            .zip(&history)
            .take_while(|(mounted, target)| mounted == target)
            .count();
        let shell = cx.entity().downgrade();
        let mut pushed = Vec::new();
        self.nav.update(cx, |nav, cx| {
            while nav.depth() > common {
                if nav.pop(motion, cx).is_none() {
                    break;
                }
            }
            for destination in &history[common.min(history.len())..] {
                let page = AnyView::from(cx.new(|_| DestinationView {
                    shell: shell.clone(),
                    destination: *destination,
                }));
                nav.push(page, motion, cx);
                pushed.push(*destination);
            }
        });
        // A thread page the reader navigates to is put up anew, its thread
        // a new scene; one the window builds as it turns compact is the
        // conversation it showed, drawn under a new parent.
        if motion == NavMotion::Animated
            && self.window_state.read(cx).compact
            && pushed.contains(&Destination::Thread)
            && let Some(column) = &self.column
        {
            column
                .chat(cx)
                .update(cx, |chat, cx| chat.restart_scene(cx));
        }
        self.mounted.truncate(common);
        self.mounted.extend(pushed);
        for link in &self.links {
            self.mark_on_screen(&link.store, cx);
        }
    }

    /// Tell `store` whether the window shows its conversations now: each
    /// host's threads in the panes are on screen with the column.
    pub(super) fn mark_on_screen(&self, store: &Entity<WorkspaceStore>, cx: &mut App) {
        let state = self.window_state.read(cx);
        let on_screen = if state.compact {
            state.destination() == Destination::Thread
        } else {
            state.route() == Route::Chat
        };
        store.update(cx, |store, cx| {
            store.set_conversation_on_screen(on_screen, cx);
        });
    }

    /// Navigating dismisses a software keyboard. Only a mobile build has one,
    /// and every compact window is a mobile build's.
    pub(super) fn blur_navigation(&self, window: &mut Window, cx: &mut Context<Self>) {
        if crate::window_seam::is_mobile(cx) {
            window.blur(cx);
        }
    }

    pub(super) fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.go(destination, cx));
    }

    /// Answer a platform Back gesture (or a Back control). `true` means it was
    /// consumed; `false` only at the root, where the platform closes the app.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // A composition or a software keyboard is on top of everything.
        if crate::window_seam::keyboard_covers_window(window) {
            window.blur(cx);
            return true;
        }
        // Dialogs, menus and popovers own Cancel.
        if crate::overlay::dialog_open(window, cx)
            || gpui_base::GlobalState::is_in_deferred_context(cx)
        {
            window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
            return true;
        }
        if self.window_state.read(cx).palette_open {
            self.window_state
                .update(cx, |state, cx| state.close_palette(cx));
            return true;
        }
        // An agent is reached from its lead, so Back returns there first.
        if self.window_state.read(cx).compact && self.destination(cx) == Destination::Thread {
            self.keep_lead_beneath(window, cx);
            let lead = self.column.as_ref().and_then(|column| column.leads.last());
            if let Some(lead) = lead {
                // A pane of the lead's own, opened beside the agent while the
                // window was wide, would show the lead a second time once the
                // agent's pane does.
                let lead_pane = self.pane_showing(&lead.thread, cx);
                self.pop_lead(cx);
                if let Some(pane) = lead_pane {
                    self.close_pane_into(&pane, |_, _| None, window, cx);
                }
                return true;
            }
        }
        // Everything below is one history: settings detail, settings root, a
        // Hosts visit, Pair, and the workspace pages all pop the same way.
        // Leaving a page never touches the links.
        let leaving_thread =
            self.window_state.read(cx).compact && self.destination(cx) == Destination::Thread;
        let consumed = self.window_state.update(cx, |state, cx| state.back(cx));
        if consumed
            && leaving_thread
            && self.destination(cx) == Destination::Threads
            && let Some(column) = &self.column
        {
            column
                .page()
                .update(cx, |pane, cx| pane.release_preview(cx));
        }
        consumed
    }
}
