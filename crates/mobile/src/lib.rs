//! The phone supervisor. Platform I/O belongs exclusively to `MobileHost`.
pub mod host;
mod pairing;
mod screens;

use gpui::{prelude::*, *};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use host::{MobilePreferences, PairedHost, SharedHost};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};
use tcode_client::{ConnectionState, HostLink};
use tcode_core::project::Project;
use tcode_ui::{
    OpenThread, WindowState,
    chat::ChatView,
    icon::{Icon, IconName},
    overlay::OverlayHost,
    palette::CommandPalette,
    sidebar::SessionsSidebar,
    store::{StoreChange, TopicKind, WorkspaceStore},
    theme::{self, ActiveTheme as _},
    tr,
    widgets::input::{Input, InputEvent, InputState},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Hosts,
    Threads,
    Thread,
}
#[derive(Clone)]
enum Sheet {
    Pair,
    Remove(PairedHost),
    Projects,
    Settings,
}

#[derive(Default)]
struct CachedThread {
    status: Option<tcode_protocol::SessionStatus>,
    records: Vec<tcode_core::session::StoredEvent>,
}

type ThreadCache = Rc<RefCell<HashMap<String, CachedThread>>>;

struct Connection {
    link: HostLink,
    active_session: Rc<RefCell<Option<String>>>,
    writable: Rc<Cell<bool>>,
    cache: ThreadCache,
    outgoing: async_channel::Sender<String>,
    incoming: async_channel::Sender<String>,
    target: Rc<RefCell<async_channel::Sender<String>>>,
    wire: Vec<Task<()>>,
    _tasks: Vec<Task<()>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.outgoing.close();
        self.incoming.close();
        self.target.borrow().close();
    }
}

struct MobileRoot {
    host: SharedHost,
    hosts: Vec<PairedHost>,
    preferences: MobilePreferences,
    page: Page,
    sheet: Option<Sheet>,
    connected_host: Option<PairedHost>,
    connection: Option<Connection>,
    store: Option<Entity<WorkspaceStore>>,
    store_subscriptions: Vec<Subscription>,
    subscriptions: Vec<Subscription>,
    index_ready: bool,
    state: ConnectionState,
    disconnected_since: Option<Duration>,
    elapsed: Box<dyn Fn() -> Duration>,
    pair: pairing::PairForm,
    device_name: Entity<InputState>,
    window_state: Option<Entity<WindowState>>,
    sidebar: Option<Entity<SessionsSidebar>>,
    chat: Option<Entity<ChatView>>,
    palette: Option<Entity<CommandPalette>>,
    _tick: Task<()>,
}

impl MobileRoot {
    fn new(host: SharedHost, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let clock = cx.background_executor().clone();
        let epoch = clock.now();
        let preferences = host.preferences();
        tcode_ui::apply_locale(preferences.language.as_deref());
        let device_name =
            cx.new(|cx| InputState::new(window, cx).default_value(host.device_name()));
        let pair = pairing::PairForm::new(&host, window, cx);
        let subscriptions = vec![
            cx.subscribe(&device_name, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    this.preferences.device_name = Some(input.read(cx).value().to_string());
                    this.host.save_preferences(&this.preferences);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_appearance(window, cx)
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() && this.connection.is_some() && !this.online() {
                    this.rewire(cx);
                }
            }),
        ];
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let mut root = Self {
            hosts: host.load_hosts(),
            host,
            preferences,
            page: Page::Hosts,
            sheet: None,
            connected_host: None,
            connection: None,
            store: None,
            store_subscriptions: vec![],
            subscriptions,
            index_ready: false,
            state: ConnectionState::Reconnecting { attempt: 1 },
            disconnected_since: None,
            elapsed: Box::new(move || clock.now().duration_since(epoch)),
            pair,
            device_name,
            window_state: None,
            sidebar: None,
            chat: None,
            palette: None,
            _tick: tick,
        };
        root.apply_appearance(window, cx);
        if let Some(last) = root
            .host
            .last_host_id()
            .and_then(|id| root.hosts.iter().find(|h| h.host_id == id).cloned())
        {
            root.connect(last, cx);
        }
        root
    }

    fn apply_appearance(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.preferences.appearance.as_deref() {
            Some("light") => theme::change_mode(theme::ThemeMode::Light, Some(window), cx),
            Some("dark") => theme::change_mode(theme::ThemeMode::Dark, Some(window), cx),
            _ => theme::sync_system_appearance(Some(window), cx),
        }
    }
    fn online(&self) -> bool {
        self.index_ready && self.state == ConnectionState::Connected
    }
    fn offline(&self) -> bool {
        self.state == ConnectionState::Offline
            || self
                .disconnected_since
                .is_some_and(|t| (self.elapsed)().saturating_sub(t) >= Duration::from_secs(30))
    }
    fn connect(&mut self, host: PairedHost, cx: &mut Context<Self>) {
        self.disconnect(false, cx);
        self.host.set_last_host_id(Some(&host.host_id));
        self.connected_host = Some(host.clone());
        self.page = Page::Threads;
        self.index_ready = false;
        self.disconnected_since = Some((self.elapsed)());
        self.state = ConnectionState::Reconnecting { attempt: 1 };
        let (outgoing, lines) = async_channel::unbounded::<String>();
        let (incoming, received) = async_channel::unbounded();
        let link = HostLink::new(outgoing.clone(), received);
        link.set_connection_state(self.state.clone());
        let initial = self.host.connect(&host);
        let target = Rc::new(RefCell::new(initial.to_host.clone()));
        let active_session = Rc::new(RefCell::new(None));
        let writable = Rc::new(Cell::new(false));
        let cache = ThreadCache::default();
        let cached = cache.clone();
        let can_write = writable.clone();
        let rejected = incoming.clone();
        let to = target.clone();
        let forward = cx.spawn(async move |_, _| {
            while let Ok(line) = lines.recv().await {
                if !can_write.get()
                    && let Ok(request) =
                        serde_json::from_str::<tcode_protocol::ClientMessage>(&line)
                    && matches!(request.payload, tcode_protocol::ClientPayload::Command(_))
                {
                    if let tcode_protocol::ClientPayload::Command(
                        tcode_protocol::Command::SelectSession { session_id },
                    ) = &request.payload
                    {
                        let events = cached.borrow().get(session_id).and_then(|thread| {
                            thread.status.clone().map(|status| {
                                vec![
                                    tcode_protocol::EventEnvelope {
                                        topic: tcode_protocol::Topic::ActiveSession,
                                        event: tcode_protocol::ServerEvent::ActiveSessionReplaced(
                                            Some(status),
                                        ),
                                    },
                                    tcode_protocol::EventEnvelope {
                                        topic: tcode_protocol::Topic::SessionEvents {
                                            session_id: session_id.clone(),
                                        },
                                        event: tcode_protocol::ServerEvent::SessionSnapshot(
                                            thread.records.clone(),
                                        ),
                                    },
                                ]
                            })
                        });
                        if let Some(events) = events {
                            for event in events {
                                if let Ok(line) = tcode_protocol::encode_line(
                                    &tcode_protocol::HostMessage::Event(event),
                                ) {
                                    let _ = rejected.send(line).await;
                                }
                            }
                        }
                    }
                    let ack = tcode_protocol::HostMessage::Ack {
                        id: request.id,
                        result: Err(tcode_protocol::ProtocolError {
                            code: "offline".into(),
                            message: "Host is disconnected".into(),
                        }),
                    };
                    if let Ok(line) = tcode_protocol::encode_line(&ack) {
                        let _ = rejected.send(line).await;
                    }
                    continue;
                }

                let sender = to.borrow().clone();
                if sender.send(line).await.is_err() {
                    break;
                }
            }
        });
        let pump = link.clone();
        let pump = cx.spawn(async move |_, _| pump.pump().await);
        let store = cx.new(|cx| {
            let mut store = WorkspaceStore::new(link.clone(), cx);
            store.attach_remote(host.name.clone(), cx);
            store
        });
        self.store_subscriptions = vec![
            cx.subscribe(&store, |this, _, change: &StoreChange, cx| {
                if change.topic == TopicKind::Index {
                    this.index_ready = true;
                    if let Some(sidebar) = &this.sidebar {
                        sidebar.update(cx, |sidebar, cx| sidebar.set_loading(false, cx));
                    }
                }
                cx.notify();
            }),
            cx.observe(&store, |this, store, cx| {
                if let Some(connection) = &this.connection {
                    let active = store.read(cx).active_session_id();
                    if *connection.active_session.borrow() != active {
                        *connection.active_session.borrow_mut() = active.clone();
                        if let Some(session_id) = active {
                            let _ = connection.link.subscribe(tcode_protocol::Subscription {
                                topic: tcode_protocol::Topic::SessionEvents { session_id },
                            });
                        }
                    }
                    let subscribed = connection.link.subscribed_topics();
                    for session in store.read(cx).flat_sessions() {
                        let topic = tcode_protocol::Topic::SessionStatus {
                            session_id: session.id,
                        };
                        if !subscribed.contains(&topic) {
                            let _ = connection
                                .link
                                .subscribe(tcode_protocol::Subscription { topic });
                        }
                    }
                }
                cx.notify();
            }),
        ];
        self.store = Some(store);
        self.connection = Some(Connection {
            link,
            active_session,
            writable,
            cache,
            outgoing,
            incoming,
            target,
            wire: vec![],
            _tasks: vec![forward, pump],
        });
        self.install_wire(initial, cx);
        cx.notify();
    }
    fn install_wire(&mut self, transport: host::Transport, cx: &mut Context<Self>) {
        let connection = self.connection.as_mut().expect("connection exists");
        *connection.target.borrow_mut() = transport.to_host;
        let incoming = connection.incoming.clone();
        let active_session = connection.active_session.clone();
        let cache = connection.cache.clone();
        let receiver = transport.from_host;
        let state = transport.state;
        connection.wire = vec![
            cx.spawn(async move |_, _| {
                while let Ok(line) = receiver.recv().await {
                    if let Ok(tcode_protocol::HostMessage::Event(event)) =
                        serde_json::from_str(&line)
                    {
                        let mut cache = cache.borrow_mut();
                        match &event.event {
                            tcode_protocol::ServerEvent::SessionStatusReplaced(status)
                            | tcode_protocol::ServerEvent::ActiveSessionReplaced(Some(status)) => {
                                cache.entry(status.session_id.clone()).or_default().status =
                                    Some(status.clone());
                            }
                            tcode_protocol::ServerEvent::SessionSnapshot(records) => {
                                if let tcode_protocol::Topic::SessionEvents { session_id } =
                                    &event.topic
                                {
                                    cache.entry(session_id.clone()).or_default().records =
                                        records.clone();
                                }
                            }
                            tcode_protocol::ServerEvent::SessionEvent(record) => {
                                if let tcode_protocol::Topic::SessionEvents { session_id } =
                                    &event.topic
                                {
                                    cache
                                        .entry(session_id.clone())
                                        .or_default()
                                        .records
                                        .push(record.clone());
                                }
                            }
                            _ => {}
                        }
                        if let tcode_protocol::Topic::SessionEvents { session_id } = &event.topic
                            && active_session.borrow().as_ref() != Some(session_id)
                        {
                            continue;
                        }
                    }
                    if incoming.send(line).await.is_err() {
                        break;
                    }
                }
            }),
            cx.spawn(async move |this, cx| {
                while let Ok(state) = state.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            if state == ConnectionState::Connected {
                                this.disconnected_since = None;
                                if let Some(host) = this.connected_host.as_mut() {
                                    host.last_connected_unix = Some(now_secs());
                                    if let Some(saved) =
                                        this.hosts.iter_mut().find(|h| h.host_id == host.host_id)
                                    {
                                        *saved = host.clone();
                                    }
                                    this.host.save_hosts(&this.hosts);
                                }
                            } else if this.disconnected_since.is_none() {
                                this.disconnected_since = Some((this.elapsed)());
                            }
                            if let Some(connection) = &this.connection {
                                connection.writable.set(state == ConnectionState::Connected);
                                if state == ConnectionState::Connected
                                    && let Some(session_id) =
                                        connection.active_session.borrow().clone()
                                    && let Some(store) = &this.store
                                {
                                    store.update(cx, |store, _| store.select_session(session_id));
                                }

                                connection.link.set_connection_state(state.clone());
                            }
                            this.state = state;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }),
        ];
    }
    fn rewire(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.connected_host.clone() else {
            return;
        };
        if let Some(connection) = &mut self.connection {
            connection.target.borrow().close();
            connection.wire.clear();
        }
        self.install_wire(self.host.connect(&host), cx);
        if let Some(connection) = &self.connection {
            for topic in connection.link.subscribed_topics() {
                let _ = connection
                    .link
                    .subscribe(tcode_protocol::Subscription { topic });
            }
        }
    }
    fn disconnect(&mut self, clear_last: bool, cx: &mut Context<Self>) {
        self.connection = None;
        self.store = None;
        self.store_subscriptions.clear();
        self.connected_host = None;
        self.sheet = None;
        self.page = Page::Hosts;
        self.window_state = None;
        self.sidebar = None;
        self.chat = None;
        self.palette = None;
        if clear_last {
            self.host.set_last_host_id(None);
        }
        cx.notify();
    }
    fn back(&mut self, cx: &mut Context<Self>) -> bool {
        if self.sheet.take().is_some() {
            cx.notify();
            return true;
        }
        match self.page {
            Page::Thread => {
                self.page = Page::Threads;
                cx.notify();
                true
            }
            Page::Threads => {
                self.disconnect(false, cx);
                true
            }
            Page::Hosts => false,
        }
    }
    fn start_draft(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        if !self.online() {
            return;
        }
        if let Some(store) = &self.store {
            store.update(cx, |s, _| {
                s.start_draft(project.id.clone(), project.root.clone())
            });
        }
        self.page = Page::Thread;
        self.sheet = None;
        if let Some(chat) = &self.chat {
            chat.update(cx, |chat, cx| chat.focus_composer(window, cx));
        }
        cx.notify();
    }

    fn ensure_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.is_some() {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let state = cx.new(|_| WindowState::new(false).with_compact(true));
        let sidebar = cx.new(|cx| {
            let mut sidebar = SessionsSidebar::new(store.clone(), state.clone(), cx);
            sidebar.set_loading(!self.index_ready, cx);
            sidebar
        });
        let chat = cx.new(|cx| ChatView::new(store.clone(), state.clone(), window, cx));
        let palette = cx.new(|cx| CommandPalette::new(store, state.clone(), window, cx));
        self.store_subscriptions.push(cx.subscribe_in(
            &state,
            window,
            |this, _, _: &OpenThread, window, cx| {
                this.page = Page::Thread;
                if let Some(chat) = &this.chat {
                    chat.update(cx, |chat, cx| chat.focus_composer(window, cx));
                }
                cx.notify();
            },
        ));
        self.store_subscriptions
            .push(cx.observe_in(&state, window, |this, state, window, cx| {
                if state.read(cx).palette_open
                    && let Some(palette) = &this.palette
                {
                    palette.update(cx, |palette, cx| palette.focus(window, cx));
                }
                cx.notify();
            }));
        self.window_state = Some(state);
        self.sidebar = Some(sidebar);
        self.chat = Some(chat);
        self.palette = Some(palette);
    }
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}
fn label(key: &str) -> String {
    tr!(format!("mobile.{key}")).into_owned()
}
fn text(value: impl Into<SharedString>, size: f32) -> Div {
    div()
        .text_size(px(size))
        .line_height(px(if size >= 16. { 22. } else { 20. }))
        .child(value.into())
}
fn button(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let title = title.into();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(title.clone())
        .min_w(px(44.))
        .min_h(px(44.))
        .px(px(10.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(12.))
        .text_size(px(16.))
        .line_height(px(22.))
        .cursor_pointer()
        .text_color(if primary {
            theme.primary_foreground
        } else {
            theme.primary
        })
        .when(primary, |d| d.bg(theme.primary))
        .when(!enabled, |d| {
            d.text_color(theme.muted_foreground)
                .when(primary, |d| d.bg(theme.secondary))
        })
        .active(|s| s.bg(theme.foreground.opacity(0.08)))
        .child(div().min_w_0().truncate().child(title))
}
fn icon_button(
    id: impl Into<ElementId>,
    aria_label: impl Into<SharedString>,
    icon: IconName,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(aria_label.into())
        .size(px(44.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_color(if enabled {
            theme.primary
        } else {
            theme.muted_foreground
        })
        .active(|s| s.bg(theme.foreground.opacity(0.08)))
        .child(Icon::new(icon).size(px(18.)))
}
fn scroll(id: &'static str, content: Div) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(content.flex_none())
}

// Keep connection feedback independent of a separate animation asset.
fn spinner(diameter: f32, color: Hsla) -> impl IntoElement {
    div().size(px(diameter)).flex_none().with_animation(
        "phone-spinner",
        Animation::new(Duration::from_millis(800)).repeat(),
        move |element, progress| {
            element.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let center = bounds.center();
                        for spoke in 0..12 {
                            let angle = (spoke as f32 / 12. + progress) * std::f32::consts::TAU;
                            let mut path = PathBuilder::stroke(px(1.4));
                            path.move_to(
                                center
                                    + point(
                                        px(angle.cos() * diameter * 0.24),
                                        px(angle.sin() * diameter * 0.24),
                                    ),
                            );
                            path.line_to(
                                center
                                    + point(
                                        px(angle.cos() * diameter * 0.43),
                                        px(angle.sin() * diameter * 0.43),
                                    ),
                            );
                            if let Ok(path) = path.build() {
                                window.paint_path(path, color.opacity(0.25 + spoke as f32 / 16.));
                            }
                        }
                    },
                )
                .size_full(),
            )
        },
    )
}

impl Render for MobileRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_workspace(window, cx);
        let insets = self.host.insets();
        let safe = insets.safe_area;
        let bottom = safe.bottom.max(insets.ime.bottom);
        let page = match self.page {
            Page::Hosts => self.render_hosts(cx),
            Page::Threads => self.render_threads(cx),
            Page::Thread => self.render_thread(cx),
        };
        let content = v_flex()
            .size_full()
            .pt(safe.top)
            .pb(bottom)
            .pl(safe.left)
            .pr(safe.right)
            .child(page);
        v_flex()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family(cx.theme().font_family.clone())
            .text_size(px(16.))
            .line_height(px(22.))
            .child(content)
            .when(
                self.window_state
                    .as_ref()
                    .is_some_and(|state| state.read(cx).palette_open),
                |el| el.children(self.palette.clone()),
            )
            .when_some(self.sheet.clone(), |d, s| {
                d.child(self.render_sheet(s, window, cx))
            })
    }
}

/// Consume a sheet/back-stack step. The platform exits only at the root.
struct MobileRootHandle(WeakEntity<MobileRoot>);
impl Global for MobileRootHandle {}

pub fn handle_back(cx: &mut App) -> bool {
    let Some(root) = cx
        .try_global::<MobileRootHandle>()
        .map(|handle| handle.0.clone())
    else {
        return false;
    };
    if gpui_base::GlobalState::is_in_deferred_context(cx) {
        if let Some(window) = cx.windows().into_iter().next() {
            let _ = window.update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx)
            });
        }
        return true;
    }
    root.update(cx, |root, cx| {
        if let Some(state) = &root.window_state
            && state.read(cx).palette_open
        {
            state.update(cx, |state, cx| state.close_palette(cx));
            return true;
        }
        root.back(cx)
    })
    .unwrap_or(false)
}

#[cfg(feature = "native")]
pub fn run(cx: &mut App) {
    run_with_host(cx, Rc::new(host::NativeHost::from_env()));
}

/// Open the same phone UI on native platforms and browser hosts.
pub fn run_with_host(cx: &mut App, host: SharedHost) {
    run_with_size(cx, host, size(px(393.), px(852.)));
}
/// Desktop preview geometry; the screen implementation is identical.
pub fn run_with_size(cx: &mut App, host: SharedHost, dimensions: Size<Pixels>) {
    #[cfg(target_os = "android")]
    let fonts = vec![
        std::borrow::Cow::Borrowed(tcode_ui::assets::DM_SANS),
        std::borrow::Cow::Borrowed(tcode_ui::assets::LILEX_REGULAR),
        std::borrow::Cow::Borrowed(tcode_ui::assets::LILEX_BOLD),
        std::borrow::Cow::Borrowed(tcode_ui::assets::LILEX_ITALIC),
        std::borrow::Cow::Borrowed(tcode_ui::assets::LILEX_BOLD_ITALIC),
    ];
    #[cfg(not(target_os = "android"))]
    let fonts = vec![std::borrow::Cow::Borrowed(tcode_ui::assets::DM_SANS)];
    cx.text_system().add_fonts(fonts).expect("mobile fonts");
    let palette: serde_json::Value =
        serde_json::from_str(include_str!("../../../themes/tcode.json")).expect("theme");
    let palette = palette
        .to_string()
        .replace("#F2F4F7C7", "#F2F4F7")
        .replace("#15171CC7", "#15171C");
    #[cfg(target_os = "android")]
    let palette = palette.replace("SF Mono", "Lilex");
    theme::init_with_json(&palette, cx);
    tcode_ui::markdown::init(cx);
    cx.activate(true);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.), px(0.)),
            dimensions,
        ))),
        titlebar: None,
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        window.set_window_title("tcode phone");
        let root = cx.new(|cx| MobileRoot::new(host, window, cx));
        cx.set_global(MobileRootHandle(root.downgrade()));
        cx.new(|cx| OverlayHost::new(root, window, cx))
    })
    .expect("open phone window");
}
