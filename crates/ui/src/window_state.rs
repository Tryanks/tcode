use gpui::{Context, Entity, EventEmitter};

use crate::store::WorkspaceStore;

/// One place the window can be. The window keeps a *history* of them, and every
/// Back — the toolbar control, the Android gesture, the desktop Back row — pops
/// that one history. There is no second navigation authority: the compact
/// navigation stack mirrors it, and the wide layout derives its [`Route`] from
/// whichever destination is on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// Which host this window talks to: the root, and a place that can also be
    /// *visited* from an attached workspace without detaching from it.
    Hosts,
    /// The pairing form, pushed from Hosts (or from a Nearby / repair row that
    /// prefilled it).
    Pair,
    Threads,
    Thread,
    /// The thread's terminal, diff/plan or preview, full width.
    Panel,
    /// The settings root: the section list in compact, the whole route in wide.
    Settings,
    /// One settings section's detail. Which section it is belongs to the page;
    /// *that a detail is open* is navigation and belongs here.
    SettingsSection,
}

impl Destination {
    /// Which wide surface shows this destination. The wide layout shows a whole
    /// hierarchy at once, so several destinations share one route.
    pub fn route(self) -> Route {
        match self {
            Self::Hosts | Self::Pair => Route::Hosts,
            Self::Settings | Self::SettingsSection => Route::Settings,
            Self::Threads | Self::Thread | Self::Panel => Route::Chat,
        }
    }
}

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    /// The layout the window's current width calls for. Derived from the
    /// viewport by [`crate::window_seam::compact_for`] and never persisted:
    /// widening a window is not a preference.
    pub compact: bool,
    /// Where the window has been, oldest first. Never empty: `history[0]` is
    /// [`Destination::Hosts`], the root the platform's Back gesture falls off.
    history: Vec<Destination>,
    pub palette_open: bool,
    pub sidebar_collapsed: bool,
    pub quit_prompt_epoch: u64,
    pub quit_prompt_open: bool,
    pub pending_settings_section: Option<String>,
}

impl WindowState {
    pub fn new(sidebar_collapsed: bool) -> Self {
        Self {
            compact: false,
            history: vec![Destination::Hosts],
            palette_open: false,
            sidebar_collapsed,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            pending_settings_section: None,
        }
    }

    pub fn with_compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    /// Set the layout for the width the window now has. Returns whether the
    /// rule flipped, so the shell only reconciles navigation when it did.
    pub fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) -> bool {
        if self.compact == compact {
            return false;
        }
        self.compact = compact;
        cx.notify();
        true
    }

    pub fn history(&self) -> &[Destination] {
        &self.history
    }

    pub fn destination(&self) -> Destination {
        *self.history.last().expect("the history is never empty")
    }

    /// The destination Back returns to, or `None` at the root.
    pub fn parent(&self) -> Option<Destination> {
        (self.history.len() > 1).then(|| self.history[self.history.len() - 2])
    }

    pub fn route(&self) -> Route {
        self.destination().route()
    }

    /// Push `destination`. Visiting somewhere the window already is does
    /// nothing, so a repeated tap cannot stack the same page twice.
    pub fn go(&mut self, destination: Destination, cx: &mut Context<Self>) {
        if self.destination() == destination {
            return;
        }
        self.history.push(destination);
        cx.notify();
    }

    /// Pop one step. `false` only where there is nothing left to pop — in
    /// compact that is the root, where the platform closes the app; in wide it
    /// is the workspace, which is not a page you can leave.
    pub fn back(&mut self, cx: &mut Context<Self>) -> bool {
        if self.history.len() < 2 {
            return false;
        }
        if self.compact {
            self.history.pop();
            cx.notify();
            return true;
        }
        // Wide shows the whole workspace hierarchy at once, so only a change of
        // route is a step the user can see.
        let route = self.route();
        if route == Route::Chat {
            return false;
        }
        while self.history.len() > 1 && self.route() == route {
            self.history.pop();
        }
        cx.notify();
        true
    }

    /// A fresh attachment: the previous host's pages are not this host's, so the
    /// history restarts at its thread list.
    pub fn enter_workspace(&mut self, cx: &mut Context<Self>) {
        self.history.truncate(1);
        self.history.push(Destination::Threads);
        cx.notify();
    }

    /// No attachment: only the root remains.
    pub fn leave_workspace(&mut self, cx: &mut Context<Self>) {
        self.history.truncate(1);
        cx.notify();
    }

    /// "Show me this thread." It is a navigation intent, not a layout decision:
    /// every width emits it and the shell decides how to present it — a wide
    /// window already shows the thread beside the list, a compact one pushes it.
    pub fn open_thread(&mut self, cx: &mut Context<Self>) {
        cx.emit(OpenThread);
    }

    pub fn toggle_sidebar_collapsed(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        store.update(cx, |store, _cx| {
            store.set_sidebar_collapsed(self.sidebar_collapsed)
        });
        cx.notify();
    }

    /// Switch to the settings route (closes the palette).
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.go(Destination::Settings, cx);
    }

    /// Leave settings, however deep in it the window is.
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        while self.history.len() > 1 && self.route() == Route::Settings {
            self.history.pop();
        }
        cx.notify();
    }

    pub fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = true;
        cx.notify();
    }

    pub fn close_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = false;
        cx.notify();
    }

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        cx.notify();
    }
}

/// The top-level window surface: the chat workspace, the hosts list or the
/// full-page settings. Derived from [`WindowState::destination`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Chat,
    Hosts,
    Settings,
}

#[derive(Clone, Copy)]
pub struct OpenThread;
impl EventEmitter<OpenThread> for WindowState {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    /// The history is the only navigation authority: a visit to Hosts from an
    /// open thread comes back to that thread, and settings unwinds one step at
    /// a time.
    #[gpui::test]
    fn back_pops_the_history_one_step_and_stops_at_the_root(cx: &mut TestAppContext) {
        let state = cx.new(|_| WindowState::new(false).with_compact(true));
        state.update(cx, |state, cx| {
            state.enter_workspace(cx);
            state.go(Destination::Thread, cx);
            // Visiting Hosts from a thread is a push, not a rewind.
            state.go(Destination::Hosts, cx);
            assert_eq!(state.parent(), Some(Destination::Thread));
            assert!(state.back(cx));
            assert_eq!(state.destination(), Destination::Thread);

            state.open_settings(cx);
            state.go(Destination::SettingsSection, cx);
            assert!(state.back(cx));
            assert_eq!(state.destination(), Destination::Settings);
            assert!(state.back(cx));
            assert_eq!(state.destination(), Destination::Thread);

            assert!(state.back(cx));
            assert!(state.back(cx));
            assert_eq!(state.destination(), Destination::Hosts);
            assert!(!state.back(cx), "the root belongs to the platform");
        });
    }

    /// Wide has no page stack: Back leaves the settings route in one step and
    /// reports "not consumed" once the workspace is showing.
    #[gpui::test]
    fn wide_back_leaves_a_route_rather_than_a_page(cx: &mut TestAppContext) {
        let state = cx.new(|_| WindowState::new(false));
        state.update(cx, |state, cx| {
            state.enter_workspace(cx);
            state.open_settings(cx);
            state.go(Destination::SettingsSection, cx);
            assert!(state.back(cx));
            assert_eq!(state.route(), Route::Chat);
            assert!(!state.back(cx));
        });
    }
}
