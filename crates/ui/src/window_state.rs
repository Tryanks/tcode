use gpui::{Context, EventEmitter, SharedString};
use serde::{Deserialize, Serialize};

/// One place the window can be. The window keeps a *history* of them, and every
/// Back — the toolbar control, the Android gesture, the desktop Back row — pops
/// that one history. There is no second navigation authority: the compact
/// navigation stack mirrors it, and the wide layout derives its [`Route`] from
/// whichever destination is on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// The machines this device knows, and which of them the window's
    /// workspace holds: a page visited over the threads.
    Hosts,
    /// The pairing form, pushed from Hosts, from a refused host's Pair again
    /// or from a window with no host.
    Pair,
    /// The threads of every host the window shows: the root.
    Threads,
    Thread,
    /// The thread's terminal, diff/plan or preview, full width.
    Panel,
    /// One of the thread's pull requests, full width.
    PullRequest,
    /// The settings root: the section list in compact, the whole route in wide.
    Settings,
    /// One settings section's detail. Which section it is belongs to the page;
    /// *that a detail is open* is navigation and belongs here.
    SettingsSection,
    SettingsThreadRules,
    /// One Git platform, managed on a phone's page pushed over Git Platforms.
    SettingsSourceControlHost,
    /// One harness's settings, a page on a compact window.
    SettingsHarness,
    /// The phone's page for pinning, settling and reordering threads by handle.
    ArrangeThreads,
}

/// The shell's client-local checkpoint, independent of the host's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NavigationSnapshot {
    pub history: Vec<Destination>,
    pub host_id: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub panel: NavigationPanel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NavigationPanel {
    Terminal,
    #[default]
    Diff,
    Plan,
    Agents,
    PullRequests,
    Preview,
}

impl NavigationPanel {
    pub fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Diff => "diff",
            Self::Plan => "plan",
            Self::Agents => "agents",
            Self::PullRequests => "pr",
            Self::Preview => "preview",
        }
    }
}

impl NavigationSnapshot {
    pub fn from_preferences(value: serde_json::Value) -> Option<Self> {
        let mut snapshot: Self = serde_json::from_value(value).ok()?;
        // Ignore malformed/unbounded local state rather than building an
        // arbitrary number of mounted views during startup.
        if snapshot.history.len() > 128 {
            return None;
        }
        snapshot.history.retain(|destination| {
            !matches!(
                destination,
                Destination::Settings
                    | Destination::SettingsSection
                    | Destination::SettingsThreadRules
                    | Destination::SettingsSourceControlHost
                    | Destination::SettingsHarness
                    | Destination::ArrangeThreads
                    | Destination::Pair
                    // A cold start reads the pull request afresh from its thread.
                    | Destination::PullRequest
            )
        });
        // Machines was the root before the threads were.
        if snapshot.history.first() == Some(&Destination::Hosts) {
            snapshot.history.remove(0);
        }
        if snapshot.history.first() != Some(&Destination::Threads) {
            snapshot.history.insert(0, Destination::Threads);
        }
        snapshot.history.dedup();
        if snapshot.host_id.is_none() {
            snapshot.without_host();
        } else if snapshot.session_id.is_none() {
            snapshot.without_thread();
        }
        Some(snapshot)
    }

    pub fn without_host(&mut self) {
        self.host_id = None;
        self.session_id = None;
        self.history = vec![Destination::Threads];
    }

    fn without_thread(&mut self) {
        self.session_id = None;
        if let Some(index) = self.history.iter().position(|destination| {
            matches!(
                destination,
                Destination::Thread | Destination::Panel | Destination::PullRequest
            )
        }) {
            self.history.truncate(index);
        }
    }
}

impl Destination {
    /// Which wide surface shows this destination. The wide layout shows a whole
    /// hierarchy at once, so several destinations share one route.
    pub fn route(self) -> Route {
        match self {
            Self::Hosts | Self::Pair => Route::Hosts,
            Self::Settings
            | Self::SettingsSection
            | Self::SettingsThreadRules
            | Self::SettingsSourceControlHost
            | Self::SettingsHarness => Route::Settings,
            Self::Threads
            | Self::Thread
            | Self::Panel
            | Self::PullRequest
            | Self::ArrangeThreads => Route::Chat,
        }
    }

    /// The short, fixed label a Back control carries when this destination is
    /// the one it returns to. Deliberately not the page's own title: a Back
    /// control names a kind of place, so it stays the same width whatever the
    /// machine or thread underneath is called, and never truncates. Pair is
    /// reached from Machines and answers with its caller's label.
    pub fn back_label(self) -> SharedString {
        crate::tr!(match self {
            Self::Hosts | Self::Pair => "hosts.title",
            Self::Threads | Self::ArrangeThreads => "mobile.threads",
            Self::Thread => "mobile.thread",
            Self::Panel => "chat.panels",
            Self::PullRequest => "pull_requests.title",
            Self::Settings
            | Self::SettingsSection
            | Self::SettingsThreadRules
            | Self::SettingsSourceControlHost
            | Self::SettingsHarness => "settings.title",
        })
        .into_owned()
        .into()
    }
}

/// Window-global UI state owned by the GPUI layer.
pub struct WindowState {
    /// The layout the window calls for. Derived from the build and the
    /// viewport by [`crate::window_seam::window_is_compact`] and never
    /// persisted: widening a window is not a preference.
    pub compact: bool,
    /// Where the window has been, oldest first. Never empty: `history[0]` is
    /// [`Destination::Threads`], the root the platform's Back gesture falls off.
    history: Vec<Destination>,
    /// Whether the palette is open. A closed palette stays on screen while it
    /// leaves (see [`Self::palette_mounted`]).
    pub palette_open: bool,
    /// Closed, with the palette's presence not yet gone; the palette clears
    /// it ([`Self::palette_left`]).
    palette_leaving: bool,
    /// Whether the palette focuses its search input when it opens. A tap on
    /// the search pill wants the keyboard; the mobile "+" button lists
    /// projects to tap and must not raise it.
    pub palette_focuses_query: bool,
    pub quit_prompt_epoch: u64,
    pub quit_prompt_open: bool,
    pub pending_settings_section: Option<String>,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            compact: false,
            history: vec![Destination::Threads],
            palette_open: false,
            palette_leaving: false,
            palette_focuses_query: true,
            quit_prompt_epoch: 0,
            quit_prompt_open: false,
            pending_settings_section: None,
        }
    }
}

impl WindowState {
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

    pub(crate) fn restore(&mut self, snapshot: &NavigationSnapshot, cx: &mut Context<Self>) {
        self.history.clone_from(&snapshot.history);
        cx.notify();
    }

    /// The thread the window showed is not there any more: a restored one
    /// went while this client was away, or the column left its host.
    pub(crate) fn drop_thread_pages(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.history.iter().position(|destination| {
            matches!(
                destination,
                Destination::Thread | Destination::Panel | Destination::PullRequest
            )
        }) {
            self.history.truncate(index);
            cx.notify();
        }
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

    /// Stand a thread's page on the topmost thread page, in place of what
    /// stood on it: the thread beneath stays a page to come back to.
    /// [`Self::go`] does nothing on a thread page, where this is the point.
    pub(crate) fn stack_thread(&mut self, cx: &mut Context<Self>) {
        if let Some(at) = self.thread_page_at() {
            self.history.truncate(at + 1);
            self.history.push(Destination::Thread);
            cx.notify();
        }
    }

    /// Take out the `count` topmost thread pages; what stands on them stays.
    pub(crate) fn unstack_threads(&mut self, count: usize, cx: &mut Context<Self>) {
        for _ in 0..count {
            if let Some(at) = self.thread_page_at() {
                self.history.remove(at);
                cx.notify();
            }
        }
    }

    fn thread_page_at(&self) -> Option<usize> {
        self.history
            .iter()
            .rposition(|destination| *destination == Destination::Thread)
    }

    /// Leave the pairing page for the Machines page it was opened over. Wide,
    /// [`Self::back`] leaves the whole Hosts route, Pair included.
    pub fn close_pair(&mut self, cx: &mut Context<Self>) {
        if self.destination() != Destination::Pair {
            return;
        }
        self.history.pop();
        if self.destination() != Destination::Hosts {
            self.history.push(Destination::Hosts);
        }
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

    /// A fresh workspace: the previous host's pages are not this host's, so
    /// the history restarts at the threads.
    pub fn enter_workspace(&mut self, cx: &mut Context<Self>) {
        self.history = vec![Destination::Threads];
        cx.notify();
    }

    /// No host: what remains is the threads' empty root and a visit to
    /// Machines, where hosts are added.
    pub fn leave_workspace(&mut self, cx: &mut Context<Self>) {
        self.history.retain(|destination| {
            matches!(
                destination,
                Destination::Threads | Destination::Hosts | Destination::Pair
            )
        });
        self.history.dedup();
        cx.notify();
    }

    /// Leave a visit to Machines for the threads beneath it.
    pub fn leave_hosts(&mut self, cx: &mut Context<Self>) {
        if self.route() != Route::Hosts {
            return;
        }
        while self.history.len() > 1 && self.route() == Route::Hosts {
            self.history.pop();
        }
        cx.notify();
    }

    /// Sidebar-driven conversation navigation replaces the wide Hosts visit
    /// with Chat. Compact keeps the visit on its stack so Back still returns
    /// through every page exactly as it did before.
    pub fn leave_route_for_chat(&mut self, cx: &mut Context<Self>) {
        if self.compact || self.route() != Route::Hosts {
            return;
        }
        while self.history.len() > 1 && self.route() == Route::Hosts {
            self.history.pop();
        }
        if self.route() != Route::Chat {
            return;
        }
        if self.history.len() == 1 {
            self.history.push(Destination::Thread);
        } else {
            *self.history.last_mut().expect("the history is never empty") = Destination::Thread;
        }
        cx.notify();
    }

    /// Switch to the settings route (closes the palette).
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        // Settings replaces the window the palette would leave over.
        self.palette_open = false;
        self.palette_leaving = false;
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
        self.show_palette(true, cx);
    }

    /// Open the palette as a list to tap through, leaving the search input
    /// unfocused so no software keyboard comes up.
    pub fn open_palette_without_keyboard(&mut self, cx: &mut Context<Self>) {
        self.show_palette(false, cx);
    }

    fn show_palette(&mut self, focuses_query: bool, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_leaving = false;
        self.palette_focuses_query = focuses_query;
        cx.notify();
    }

    /// Whether the palette is on screen: open, or still leaving after a
    /// close. Its host keeps it mounted meanwhile. A wide window draws the
    /// palette only on the Chat route, so on any other a leaving palette is
    /// gone.
    pub fn palette_mounted(&self) -> bool {
        self.palette_open || (self.palette_leaving && (self.compact || self.route() == Route::Chat))
    }

    /// Whether a closed palette is still leaving.
    pub(crate) fn palette_leaving(&self) -> bool {
        self.palette_leaving
    }

    /// The palette leaves mounted and inert until its presence is gone
    /// ([`Self::palette_left`]), then unmounts.
    pub fn close_palette(&mut self, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        self.palette_leaving = true;
        cx.notify();
    }

    /// The closed palette's presence is gone.
    pub(crate) fn palette_left(&mut self, cx: &mut Context<Self>) {
        if self.palette_leaving {
            self.palette_leaving = false;
            cx.notify();
        }
    }

    pub fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette_open {
            self.close_palette(cx);
        } else {
            self.open_palette(cx);
        }
    }
}

/// The top-level window surface: the chat workspace, the hosts list or
/// settings. Derived from [`WindowState::destination`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Route {
    #[default]
    Chat,
    Hosts,
    Settings,
}

#[derive(Clone, Copy)]
pub(crate) struct OpenPanel(pub NavigationPanel);
impl EventEmitter<OpenPanel> for WindowState {}
