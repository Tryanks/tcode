//! The nested compositor: one client, software composition onto the host's
//! surfaces, the session's pointer, and keys and text the host sends.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{self, Read as _, Write as _};
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use calloop::channel::{self, Channel, Sender};
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use smithay::backend::input::{ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, KeyboardHandle, Keycode, XkbConfig};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, DisconnectReason, ObjectId,
};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_seat, wl_surface::WlSurface,
};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{self, ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
};

use crate::compose::Canvas;
use crate::host::{Host, HostPtr};
use crate::{Event, Placement, ViewId};

mod text_input;

/// Text clipboard types, most preferred first.
const TEXT_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// The socket's name, before the process id.
const SOCKET_PREFIX: &str = "tcode-nest-";

/// A running nested compositor.
///
/// Once its client has connected, the compositor thread outlives this handle
/// and serves the client for the life of the process: GDK exits the process
/// when its display connection is lost, and the embedded client is GTK in
/// this process.
pub struct Nest {
    socket: String,
    commands: Sender<Command>,
    events: async_channel::Receiver<Event>,
}

enum Command {
    Attach {
        view: ViewId,
        surface: HostPtr,
    },
    Place {
        view: ViewId,
        placement: Option<Placement>,
    },
    Detach {
        view: ViewId,
        done: std::sync::mpsc::Sender<()>,
    },
    LeaveHost {
        done: std::sync::mpsc::Sender<()>,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    Type(char),
    Focus(Option<ViewId>),
    CommitText(String),
    ComposeText {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// The client's copy was read: the nest's own selection, which answers
    /// from the host's clipboard, takes its place.
    OfferClipboard,
}

impl Nest {
    /// Binds the socket in `$XDG_RUNTIME_DIR` and starts the compositor,
    /// which shows its views through `display`, the host's `wl_display`. The
    /// socket accepts one client and is unlinked once that client connects,
    /// or when this handle is dropped before one has.
    ///
    /// # Safety
    ///
    /// `display` is a live `wl_display` until [`Nest::leave_host`] returns.
    pub unsafe fn start(display: NonNull<c_void>) -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
        let socket = format!("{SOCKET_PREFIX}{}", std::process::id());
        let path = runtime.join(&socket);
        // A run with this process id that ended before its client connected
        // left its socket behind.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;

        let (commands, command_source) = channel::channel();
        let (event_sender, events) = async_channel::unbounded();
        let (ready_sender, ready) = std::sync::mpsc::channel();
        let display = HostPtr(display);
        let sender = commands.clone();
        std::thread::Builder::new()
            .name("wayland-nest".into())
            .spawn(move || {
                let result = run(
                    listener,
                    path.clone(),
                    display,
                    sender,
                    command_source,
                    event_sender,
                    &ready_sender,
                );
                let _ = std::fs::remove_file(&path);
                if let Err(error) = result {
                    log::error!("wayland nest stopped: {error}");
                    let _ = ready_sender.send(Err(error));
                }
            })?;
        ready
            .recv()
            .map_err(|_| io::Error::other("wayland nest thread exited during startup"))??;
        Ok(Self {
            socket,
            commands,
            events,
        })
    }

    /// The `WAYLAND_DISPLAY`-style name the embedded client connects to.
    pub fn socket_name(&self) -> &str {
        &self.socket
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    /// Shows `view` on `surface`, a `wl_surface` of the host's display, once
    /// it is placed.
    ///
    /// # Safety
    ///
    /// `surface` is a live `wl_surface` of the host's display until
    /// [`Nest::detach`] returns for `view`.
    pub unsafe fn attach(&self, view: ViewId, surface: NonNull<c_void>) {
        self.send(Command::Attach {
            view,
            surface: HostPtr(surface),
        });
    }

    /// Sizes `view` and shows `placement.crop` of it on its surface; `None`
    /// stops drawing it, for a surface the host has hidden. A hidden view's
    /// client is not asked for frames.
    pub fn place(&self, view: ViewId, placement: Option<Placement>) {
        self.send(Command::Place { view, placement });
    }

    /// Stops using the surface of `view`; returns once nothing of the nest
    /// refers to it.
    pub fn detach(&self, view: ViewId) {
        let (done, wait) = std::sync::mpsc::channel();
        self.send(Command::Detach { view, done });
        let _ = wait.recv();
    }

    /// Stops using the host's display; returns once nothing of the nest
    /// refers to it. Views are no longer drawn.
    pub fn leave_host(&self) {
        let (done, wait) = std::sync::mpsc::channel();
        self.send(Command::LeaveHost { done });
        let _ = wait.recv();
    }

    /// Presses or releases an evdev key on the view with keyboard focus.
    pub fn key(&self, code: u32, pressed: bool) {
        self.send(Command::Key { code, pressed });
    }

    /// Types `c` on the view with keyboard focus as a key press, for a
    /// character the nest's US layout ([`evdev_key`](crate::evdev_key))
    /// cannot type. Unlike [`Nest::commit_text`], it reaches the client in
    /// order with the keys around it.
    pub fn type_char(&self, c: char) {
        self.send(Command::Type(c));
    }

    /// Gives the keyboard to `view`, or takes it away.
    pub fn focus(&self, view: Option<ViewId>) {
        self.send(Command::Focus(view));
    }

    /// Inserts `text` into the focused view's text field, as an input method
    /// commits it, ending any composition.
    pub fn commit_text(&self, text: String) {
        self.send(Command::CommitText(text));
    }

    /// Shows `text` as the focused text field's composition, with its cursor
    /// at the byte range `cursor` of `text` (`None` hides the cursor). Empty
    /// `text` ends the composition without inserting anything.
    pub fn compose_text(&self, text: String, cursor: Option<Range<usize>>) {
        let cursor = cursor.map(|cursor| (cursor.start, cursor.end));
        self.send(Command::ComposeText { text, cursor });
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::error!("wayland nest is not running");
        }
    }
}

/// The embedded client asking for the clipboard's text.
pub struct Paste(OwnedFd);

impl Paste {
    /// Gives the client `text`; `None` when the clipboard holds no text.
    pub fn answer(self, text: Option<String>) {
        let Some(text) = text else {
            return;
        };
        // The client reads from its own thread of control, in its own time.
        std::thread::spawn(move || {
            let _ = std::fs::File::from(self.0).write_all(text.as_bytes());
        });
    }
}

/// The evdev code of the key [`State::type_char`] rebinds: F24, which the
/// host never sends since no key of a US layout types it.
const SPARE_KEY: u32 = 194;

/// The nest's keymap: US on an evdev PC keyboard, as the host's keys are
/// translated to ([`crate::evdev_key`]), with `<FK24>` ([`SPARE_KEY`]) typing
/// `spare`. Spelled out rather than taken from XKB_DEFAULT_* in the
/// environment, which applies to the user's own keyboard, not to the codes
/// the host sends.
fn keymap(spare: Option<char>) -> String {
    let spare = spare
        .map(|c| format!("key <FK24> {{ [ U{:04X} ] }};", u32::from(c)))
        .unwrap_or_default();
    format!(
        "xkb_keymap {{
            xkb_keycodes {{ include \"evdev+aliases(qwerty)\" }};
            xkb_types {{ include \"complete\" }};
            xkb_compat {{ include \"complete\" }};
            xkb_symbols {{ include \"pc+us+inet(evdev)\" {spare} }};
        }};"
    )
}

/// Runs the compositor until the handle is dropped with no client connected,
/// or the event loop fails.
fn run(
    listener: UnixListener,
    path: PathBuf,
    display: HostPtr,
    sender: Sender<Command>,
    commands: Channel<Command>,
    events: async_channel::Sender<Event>,
    ready: &std::sync::mpsc::Sender<io::Result<()>>,
) -> io::Result<()> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(io::Error::other)?;
    let handle = event_loop.handle();
    let server: Display<State> = Display::new().map_err(io::Error::other)?;
    // SAFETY: the caller of `Nest::start` keeps the display alive until
    // `Nest::leave_host` returns, which is when the host is dropped.
    let host = unsafe { Host::join(display, &handle) }?;
    let mut state = State::new(server.handle(), handle.clone(), host, sender, events)?;
    let keyboard = state.keyboard.clone();
    keyboard
        .set_keymap_from_string(&mut state, keymap(None))
        .map_err(|error| io::Error::other(format!("the nest's keymap: {error:?}")))?;
    handle
        .insert_source(
            Generic::new(server, Interest::READ, Mode::Level),
            |_, server, state| {
                // SAFETY: the display is only dropped with this source.
                unsafe { server.get_mut() }.dispatch_clients(state)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| io::Error::other(error.error))?;
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                let (stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(error) => return Err(error),
                };
                stream.set_nonblocking(true)?;
                let client = state
                    .dh
                    .insert_client(stream, Arc::new(ClientState::default()))?;
                state.client = Some(client.id());
                let _ = std::fs::remove_file(&path);
                Ok(PostAction::Remove)
            },
        )
        .map_err(|error| io::Error::other(error.error))?;
    handle
        .insert_source(commands, |event, _, state| match event {
            channel::Event::Msg(command) => state.handle(command),
            channel::Event::Closed => state.abandoned = true,
        })
        .map_err(|error| io::Error::other(error.error))?;

    let _ = ready.send(Ok(()));
    // With a client connected the thread must go on: see [`Nest`].
    while !(state.abandoned && state.client.is_none()) {
        event_loop
            .dispatch(None, &mut state)
            .map_err(io::Error::other)?;
        state.read_selection();
        state.draw_views();
        state.dh.flush_clients()?;
    }
    Ok(())
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        log::warn!("wayland nest: the embedded client disconnected ({reason:?})");
    }
}

struct Popup {
    surface: PopupSurface,
    /// Position and size relative to the parent's window geometry.
    geometry: Rectangle<i32, Logical>,
    grab: bool,
}

struct Dialog {
    toplevel: ToplevelSurface,
    /// The size it was last asked to take to fit the view, if it had to be.
    fitted: Option<Size<i32, Logical>>,
}

struct View {
    toplevel: ToplevelSurface,
    size: Option<Size<i32, Logical>>,
    /// The client's windows that belong to this one (a file chooser, a
    /// colour chooser), bottom to top. They are drawn in the middle of the
    /// view, over its toplevel.
    dialogs: Vec<Dialog>,
    /// Bottom to top, over the dialogs.
    popups: Vec<Popup>,
    /// Frame callbacks for content not yet drawn on the host.
    callbacks: Vec<WlCallback>,
    /// Frame callbacks of content drawn on the host, answered when the
    /// session asks for the next frame.
    in_flight: Vec<WlCallback>,
    dirty: bool,
}

/// A surface's committed buffer, held until the client replaces it and read
/// where it lies.
struct Contents {
    surface: WlSurface,
    buffer: WlBuffer,
    width: i32,
    height: i32,
}

pub(crate) struct State {
    dh: DisplayHandle,
    handle: LoopHandle<'static, State>,
    /// The session's side; `None` once the host has left.
    pub(crate) host: Option<Host>,
    /// The one client the socket accepted.
    client: Option<ClientId>,
    /// Whether the [`Nest`] handle is gone.
    abandoned: bool,
    compositor: CompositorState,
    xdg_shell: XdgShellState,
    shm: ShmState,
    seat_state: SeatState<State>,
    data_device: DataDeviceState,
    _outputs: OutputManagerState,
    _output: Output,
    seat: Seat<State>,
    pointer: PointerHandle<State>,
    keyboard: KeyboardHandle<State>,
    views: HashMap<ViewId, View>,
    /// Toplevels whose title has not named a view yet.
    unclaimed: Vec<ToplevelSurface>,
    /// Sizes asked for before the view's toplevel appeared.
    requested: HashMap<ViewId, Size<i32, Logical>>,
    contents: HashMap<ObjectId, Contents>,
    keyboard_view: Option<ViewId>,
    /// The character [`SPARE_KEY`] types, if it was rebound.
    spare: Option<char>,
    text_inputs: text_input::TextInputs,
    /// The seat and type of a selection the client set, to be read.
    copied: Option<(Seat<State>, &'static str)>,
    commands: Sender<Command>,
    events: async_channel::Sender<Event>,
    epoch: Instant,
}

impl State {
    fn new(
        dh: DisplayHandle,
        handle: LoopHandle<'static, State>,
        host: Host,
        commands: Sender<Command>,
        events: async_channel::Sender<Event>,
    ) -> io::Result<Self> {
        let compositor = CompositorState::new::<Self>(&dh);
        let xdg_shell = XdgShellState::new::<Self>(&dh);
        let shm = ShmState::new::<Self>(&dh, Vec::new());
        let data_device = DataDeviceState::new::<Self>(&dh);
        let outputs = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        // GTK fits windows to their monitor, so the output must be larger than
        // any view.
        let output = Output::new(
            "nest".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Tcode".into(),
                model: "Preview".into(),
            },
        );
        let mode = OutputMode {
            size: (7680, 4320).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);
        dh.create_global::<Self, ZwpTextInputManagerV3, _>(1, ());

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        let pointer = seat.add_pointer();
        // Key repeat is off: the host repeats keys itself and sends each repeat.
        let keyboard = seat
            .add_keyboard(
                XkbConfig {
                    layout: "us",
                    ..XkbConfig::default()
                },
                200,
                0,
            )
            .map_err(|error| io::Error::other(format!("{error:?}")))?;

        let state = Self {
            dh,
            handle,
            host: Some(host),
            client: None,
            abandoned: false,
            compositor,
            xdg_shell,
            shm,
            seat_state,
            data_device,
            _outputs: outputs,
            _output: output,
            seat,
            pointer,
            keyboard,
            views: HashMap::new(),
            unclaimed: Vec::new(),
            requested: HashMap::new(),
            contents: HashMap::new(),
            keyboard_view: None,
            spare: None,
            text_inputs: text_input::TextInputs::default(),
            copied: None,
            commands,
            events,
            epoch: Instant::now(),
        };
        state.offer_clipboard();
        Ok(state)
    }

    pub(crate) fn time(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Attach { view, surface } => {
                let Some(host) = self.host.as_mut() else {
                    return;
                };
                // SAFETY: guaranteed by the caller of `Nest::attach`.
                if let Err(error) = unsafe { host.attach(view, surface) } {
                    log::error!("wayland nest: cannot draw on the host's surface: {error}");
                }
            }
            Command::Place { view, placement } => {
                if let Some(placement) = placement {
                    self.resize(
                        view,
                        Size::from((
                            placement.width.max(1) as i32,
                            placement.height.max(1) as i32,
                        )),
                    );
                }
                if let Some(target) = self
                    .host
                    .as_mut()
                    .and_then(|host| host.targets.get_mut(&view))
                {
                    target.place(placement);
                }
            }
            Command::Detach { view, done } => {
                if let Some(host) = self.host.as_mut() {
                    host.targets.remove(&view);
                    host.flush();
                }
                let _ = done.send(());
            }
            Command::LeaveHost { done } => {
                if let Some(host) = self.host.take() {
                    host.leave(&self.handle);
                }
                let _ = done.send(());
            }
            Command::Key { code, pressed } => self.key(code, pressed),
            Command::Type(c) => self.type_char(c),
            Command::Focus(view) => {
                self.keyboard_view = view;
                self.refocus();
            }
            Command::CommitText(text) => self.commit_text(text),
            Command::ComposeText { text, cursor } => self.compose_text(text, cursor),
            Command::OfferClipboard => self.offer_clipboard(),
        }
    }

    fn resize(&mut self, view: ViewId, size: Size<i32, Logical>) {
        match self.views.get_mut(&view) {
            Some(entry) => {
                if entry.size != Some(size) {
                    entry.size = Some(size);
                    configure(&entry.toplevel, entry.size);
                }
            }
            None => {
                self.requested.insert(view, size);
            }
        }
    }

    /// Makes the nest's own selection the clipboard: one that asks the host
    /// for its clipboard's text each time the client pastes. The nest cannot
    /// watch the session's clipboard, and this way it never holds a copy
    /// that could be out of date.
    fn offer_clipboard(&self) {
        let mime_types = TEXT_MIME_TYPES.map(String::from).to_vec();
        set_data_device_selection(&self.dh, &self.seat, mime_types, ());
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let keyboard = self.keyboard.clone();
        keyboard.input::<(), _>(
            self,
            Keycode::new(code + 8),
            state,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    /// Binds [`SPARE_KEY`] to `c` and presses it.
    ///
    /// GTK 3 applies text-input-v3 text while it reads the Wayland
    /// connection, ahead of the key events GDK has queued from the same
    /// read, so a character committed that way could land before keys typed
    /// just before it. Key events stay in order, and GDK reads a keymap
    /// change in the same stream as the keys.
    fn type_char(&mut self, c: char) {
        if self.spare != Some(c) {
            let keyboard = self.keyboard.clone();
            if let Err(error) = keyboard.set_keymap_from_string(self, keymap(Some(c))) {
                log::error!("wayland nest: no keymap types {c:?}: {error:?}");
                return;
            }
            self.spare = Some(c);
        }
        self.key(SPARE_KEY, true);
        self.key(SPARE_KEY, false);
    }

    fn on_commit(&mut self, surface: &WlSurface) {
        let (buffer, callbacks) = compositor::with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            (
                current.buffer.take(),
                std::mem::take(&mut current.frame_callbacks),
            )
        });
        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                match shm::with_buffer_contents(&buffer, |_, _, data| (data.width, data.height)) {
                    Ok((width, height)) => {
                        let contents = Contents {
                            surface: surface.clone(),
                            buffer,
                            width,
                            height,
                        };
                        if let Some(old) = self.contents.insert(surface.id(), contents)
                            && self.contents[&surface.id()].buffer != old.buffer
                        {
                            old.buffer.release();
                        }
                    }
                    Err(error) => {
                        log::error!("wayland nest: a buffer that is not wl_shm: {error}");
                        buffer.release();
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                if let Some(old) = self.contents.remove(&surface.id()) {
                    old.buffer.release();
                }
            }
            None => {}
        }
        self.send_initial_configure(surface);
        self.fit_dialog(surface);

        let mut root = surface.clone();
        while let Some(parent) = compositor::get_parent(&root) {
            root = parent;
        }
        match self.view_of(&root).and_then(|id| self.views.get_mut(&id)) {
            Some(view) => {
                view.callbacks.extend(callbacks);
                view.dirty = true;
            }
            // A surface of no view is drawn nowhere, so nothing paces it.
            None => {
                let time = self.time();
                for callback in callbacks {
                    callback.done(time);
                }
            }
        }
    }

    /// Asks a dialog larger than its view to be the view's size, so that all
    /// of it can be reached: the view is all of the dialog that is shown.
    fn fit_dialog(&mut self, surface: &WlSurface) {
        let Some(id) = self.view_of(surface) else {
            return;
        };
        let Some(size) = self.window_geometry(surface).map(|geometry| geometry.size) else {
            return;
        };
        let bounds = self.view_size(&self.views[&id]);
        let Some(dialog) = self.views.get_mut(&id).and_then(|view| {
            view.dialogs
                .iter_mut()
                .find(|d| d.toplevel.wl_surface() == surface)
        }) else {
            return;
        };
        let fitted = Size::from((size.w.min(bounds.w), size.h.min(bounds.h)));
        if fitted != size && dialog.fitted != Some(fitted) {
            dialog.fitted = Some(fitted);
            dialog
                .toplevel
                .with_pending_state(|state| state.size = Some(fitted));
            dialog.toplevel.send_pending_configure();
        }
    }

    fn send_initial_configure(&self, surface: &WlSurface) {
        if let Some(toplevel) = self.unclaimed.iter().find(|t| t.wl_surface() == surface) {
            if !toplevel.is_initial_configure_sent() {
                configure(toplevel, None);
                toplevel.send_configure();
            }
            return;
        }
        for view in self.views.values() {
            if view.toplevel.wl_surface() == surface {
                if !view.toplevel.is_initial_configure_sent() {
                    configure(&view.toplevel, view.size);
                    view.toplevel.send_configure();
                }
                return;
            }
            for dialog in &view.dialogs {
                if dialog.toplevel.wl_surface() == surface {
                    if !dialog.toplevel.is_initial_configure_sent() {
                        dialog.toplevel.send_configure();
                    }
                    return;
                }
            }
            for popup in &view.popups {
                if popup.surface.wl_surface() == surface
                    && !popup.surface.is_initial_configure_sent()
                    && let Err(error) = popup.surface.send_configure()
                {
                    log::error!("wayland nest: popup configure failed: {error:?}");
                }
            }
        }
    }

    /// The view whose toplevel, dialog or popup is `surface`.
    fn view_of(&self, surface: &WlSurface) -> Option<ViewId> {
        self.views.iter().find_map(|(id, view)| {
            (view.toplevel.wl_surface() == surface
                || view
                    .dialogs
                    .iter()
                    .any(|d| d.toplevel.wl_surface() == surface)
                || view
                    .popups
                    .iter()
                    .any(|p| p.surface.wl_surface() == surface))
            .then_some(*id)
        })
    }

    /// Binds toplevels whose title names a view.
    fn claim(&mut self) {
        let mut index = 0;
        while index < self.unclaimed.len() {
            let title = compositor::with_states(self.unclaimed[index].wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .and_then(|data| data.lock().ok()?.title.clone())
            });
            let Some(id) = title.as_deref().and_then(ViewId::from_title) else {
                // A window the client opened for one of a view's windows is
                // that view's dialog.
                let owner = self.unclaimed[index]
                    .parent()
                    .and_then(|parent| self.view_of(&parent));
                match owner.and_then(|id| self.views.get_mut(&id)) {
                    Some(view) => {
                        let toplevel = self.unclaimed.swap_remove(index);
                        toplevel.with_pending_state(|state| {
                            state.states.set(xdg_toplevel::State::Activated);
                        });
                        if toplevel.is_initial_configure_sent() {
                            toplevel.send_pending_configure();
                        }
                        view.dialogs.push(Dialog {
                            toplevel,
                            fitted: None,
                        });
                        view.dirty = true;
                        self.refocus();
                    }
                    None => index += 1,
                }
                continue;
            };
            let toplevel = self.unclaimed.swap_remove(index);
            let size = self.requested.remove(&id);
            configure(&toplevel, size);
            let view = View {
                toplevel,
                size,
                dialogs: Vec::new(),
                popups: Vec::new(),
                callbacks: Vec::new(),
                in_flight: Vec::new(),
                dirty: true,
            };
            self.views.insert(id, view);
            if self.keyboard_view == Some(id) {
                self.refocus();
            }
        }
    }

    /// The size of `view`: what it was configured to once its client has
    /// drawn, otherwise whatever it drew.
    fn view_size(&self, view: &View) -> Size<i32, Logical> {
        self.window_geometry(view.toplevel.wl_surface())
            .map(|geometry| geometry.size)
            .or(view.size)
            .unwrap_or_else(|| Size::from((1, 1)))
    }

    /// The xdg window geometry of `surface` in its own coordinates.
    fn window_geometry(&self, surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
        let declared = compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
        });
        declared.or_else(|| {
            self.contents.get(&surface.id()).map(|contents| {
                Rectangle::new((0, 0).into(), (contents.width, contents.height).into())
            })
        })
    }

    /// Where the window geometry of `surface` (the view's toplevel, or one of
    /// its dialogs or popups) starts, in the view's coordinates.
    fn window_origin(&self, view: &View, surface: &WlSurface) -> Point<i32, Logical> {
        if view
            .dialogs
            .iter()
            .any(|d| d.toplevel.wl_surface() == surface)
        {
            // In the middle of the view; one larger than the view keeps its
            // top left corner, where its title bar is, in sight.
            let size = self
                .window_geometry(surface)
                .map(|geometry| geometry.size)
                .unwrap_or_default();
            let view = self.view_size(view);
            return Point::from((
                ((view.w - size.w) / 2).max(0),
                ((view.h - size.h) / 2).max(0),
            ));
        }
        let Some(popup) = view
            .popups
            .iter()
            .find(|p| p.surface.wl_surface() == surface)
        else {
            return Point::from((0, 0));
        };
        let parent = popup
            .surface
            .get_parent_surface()
            .map(|parent| self.window_origin(view, &parent))
            .unwrap_or_default();
        parent + popup.geometry.loc
    }

    /// Where `surface` (the view's toplevel, or one of its dialogs or popups)
    /// has its origin, in the view's coordinates.
    fn surface_origin(&self, view: &View, surface: &WlSurface) -> Point<i32, Logical> {
        let own = self
            .window_geometry(surface)
            .map(|geometry| geometry.loc)
            .unwrap_or_default();
        self.window_origin(view, surface) - own
    }

    /// Places a popup inside the view, letting the positioner's constraint
    /// adjustments flip, slide or shrink it to fit (GTK scrolls a shrunken
    /// menu). A view is drawn on one host surface, cropped to where the view
    /// is shown, so a popup reaching past it would be cut off; showing it
    /// beyond the view would take a host surface of its own.
    fn place_popup(
        &self,
        view: &View,
        surface: &PopupSurface,
        positioner: PositionerState,
    ) -> Rectangle<i32, Logical> {
        let parent = surface
            .get_parent_surface()
            .map(|parent| self.window_origin(view, &parent))
            .unwrap_or_default();
        let bounds = Rectangle::new(Point::from((-parent.x, -parent.y)), self.view_size(view));
        positioner.get_unconstrained_geometry(bounds)
    }

    /// Reads the text the client last copied and hands it to the host.
    fn read_selection(&mut self) {
        let Some((seat, mime_type)) = self.copied.take() else {
            return;
        };
        let (read, write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
            Ok(pipe) => pipe,
            Err(error) => {
                log::error!("wayland nest: clipboard pipe failed: {error}");
                return;
            }
        };
        if let Err(error) =
            request_data_device_client_selection(&seat, mime_type.to_string(), write)
        {
            log::error!("wayland nest: clipboard request failed: {error:?}");
            return;
        }
        // The client writes from its own thread of control; reading here
        // would block the compositor it is waiting on.
        let events = self.events.clone();
        let commands = self.commands.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            match std::fs::File::from(read).read_to_string(&mut text) {
                Ok(_) => {
                    let _ = events.try_send(Event::Copied(text));
                }
                Err(error) => log::error!("wayland nest: reading the client's copy: {error}"),
            }
            // Only now: a client whose selection is replaced drops what it
            // copied, read or not.
            let _ = commands.send(Command::OfferClipboard);
        });
    }

    /// Draws every view that has something new to show and whose host
    /// surface is due a frame.
    fn draw_views(&mut self) {
        let ids: Vec<ViewId> = self.views.keys().copied().collect();
        for id in ids {
            let Some(mut target) = self.host.as_mut().and_then(|host| host.targets.remove(&id))
            else {
                continue;
            };
            self.draw_view(id, &mut target);
            if let Some(host) = self.host.as_mut() {
                host.targets.insert(id, target);
            }
        }
        if let Some(host) = &self.host {
            host.flush();
        }
        self.contents.retain(|_, contents| {
            let alive = contents.surface.is_alive();
            if !alive {
                contents.buffer.release();
            }
            alive
        });
    }

    fn draw_view(&mut self, id: ViewId, target: &mut crate::host::Target) {
        let (Some(host), Some(view), Some(placement)) =
            (&self.host, self.views.get(&id), target.placement)
        else {
            return;
        };
        // New content waits for the session to ask for a frame; a view shown
        // somewhere else is drawn there at once.
        let due = target.stale || (view.dirty && !target.awaits_session());
        if !due || placement.crop.width <= 0 || placement.crop.height <= 0 {
            return;
        }
        let toplevel = view.toplevel.wl_surface();
        let Some(geometry) = self.window_geometry(toplevel) else {
            return;
        };
        let drawn = target.present(host, id, placement, |canvas| {
            self.draw_tree(
                canvas,
                toplevel,
                Point::from((-geometry.loc.x, -geometry.loc.y)),
            );
            let dialogs = view.dialogs.iter().map(|d| d.toplevel.wl_surface());
            let popups = view.popups.iter().map(|p| p.surface.wl_surface());
            for surface in dialogs.chain(popups) {
                let origin = self.window_origin(view, surface);
                if let Some(own) = self.window_geometry(surface) {
                    self.draw_tree(canvas, surface, origin - own.loc);
                }
            }
        });
        if let Err(error) = drawn {
            log::error!("wayland nest: cannot draw a view on the host: {error}");
            return;
        }
        if let Some(view) = self.views.get_mut(&id) {
            view.dirty = false;
            view.in_flight.append(&mut view.callbacks);
        }
    }

    fn draw_tree(&self, canvas: &mut Canvas<'_>, surface: &WlSurface, origin: Point<i32, Logical>) {
        compositor::with_surface_tree_upward(
            surface,
            origin,
            |_, states, parent| TraversalAction::DoChildren(*parent + subsurface_offset(states)),
            |surface, states, parent| {
                let location = *parent + subsurface_offset(states);
                let Some(contents) = self.contents.get(&surface.id()) else {
                    return;
                };
                let drawn = shm::with_buffer_contents(&contents.buffer, |ptr, len, data| {
                    // SAFETY: smithay keeps the pool mapped for this call.
                    unsafe { canvas.draw(ptr, len, data, location.x, location.y) }
                });
                if let Err(error) = drawn {
                    log::error!("wayland nest: a surface's buffer cannot be read: {error}");
                }
            },
            |_, _, _| true,
        );
    }

    /// The session asked for the next frame of `id`: its client may draw
    /// again.
    pub(crate) fn session_wants_frame(&mut self, id: ViewId) {
        let time = self.time();
        if let Some(view) = self.views.get_mut(&id) {
            for callback in view.in_flight.drain(..) {
                callback.done(time);
            }
        }
    }

    /// The topmost surface of `id` at `location`, with that surface's origin.
    fn surface_under(
        &self,
        id: ViewId,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let view = self.views.get(&id)?;
        for popup in view.popups.iter().rev() {
            let surface = popup.surface.wl_surface();
            let origin = self.window_origin(view, surface);
            if Rectangle::new(origin, popup.geometry.size)
                .to_f64()
                .contains(location)
            {
                return Some((surface.clone(), self.surface_origin(view, surface).to_f64()));
            }
        }
        for dialog in view.dialogs.iter().rev() {
            let surface = dialog.toplevel.wl_surface();
            let Some(geometry) = self.window_geometry(surface) else {
                continue;
            };
            if Rectangle::new(self.window_origin(view, surface), geometry.size)
                .to_f64()
                .contains(location)
            {
                return Some((surface.clone(), self.surface_origin(view, surface).to_f64()));
            }
        }
        let toplevel = view.toplevel.wl_surface();
        Some((
            toplevel.clone(),
            self.surface_origin(view, toplevel).to_f64(),
        ))
    }

    /// The session's pointer is at `(x, y)` of `id`.
    pub(crate) fn pointer_motion(&mut self, id: ViewId, x: f64, y: f64) {
        let location = Point::from((x, y));
        let focus = self.surface_under(id, location);
        let event = MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
        };
        self.pointer.clone().motion(self, focus, &event);
    }

    /// The session's pointer left the view it was over.
    pub(crate) fn pointer_leave(&mut self) {
        let pointer = self.pointer.clone();
        let event = MotionEvent {
            location: pointer.current_location(),
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
        };
        pointer.motion(self, None, &event);
        pointer.frame(self);
    }

    /// A press outside a grabbing popup still goes to the client, as an
    /// xdg_popup grab delivers presses on the client's own surfaces, and the
    /// client closes the popup itself. After popup_done instead, a page whose
    /// `<select>` popup it closed takes no more clicks or keys (WebKitGTK
    /// 2.52, GTK 3.24.41).
    pub(crate) fn pointer_button(&mut self, button: u32, pressed: bool) {
        let event = ButtonEvent {
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
            button,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        };
        self.pointer.clone().button(self, &event);
    }

    /// Ends a group of the session's pointer events, with its scrolling.
    pub(crate) fn pointer_frame(&mut self, scroll: Option<AxisFrame>) {
        let pointer = self.pointer.clone();
        if let Some(scroll) = scroll {
            pointer.axis(self, scroll);
        }
        pointer.frame(self);
    }

    /// Gives the keyboard to the focused view's topmost grabbing popup, else
    /// to its topmost dialog, else to its toplevel.
    fn refocus(&mut self) {
        let target = self.keyboard_view.and_then(|id| {
            let view = self.views.get(&id)?;
            Some(
                view.popups
                    .iter()
                    .rev()
                    .find(|p| p.grab)
                    .map(|p| p.surface.wl_surface())
                    .or_else(|| view.dialogs.last().map(|d| d.toplevel.wl_surface()))
                    .unwrap_or_else(|| view.toplevel.wl_surface())
                    .clone(),
            )
        });
        let keyboard = self.keyboard.clone();
        if keyboard.current_focus() != target {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }
}

fn subsurface_offset(states: &SurfaceData) -> Point<i32, Logical> {
    if states.role == Some("subsurface") {
        states
            .cached_state
            .get::<SubsurfaceCachedState>()
            .current()
            .location
    } else {
        Point::from((0, 0))
    }
}

/// Sets the pending configure of a view's toplevel. The tiled states keep GTK
/// from drawing shadows or resize borders around it.
fn configure(toplevel: &ToplevelSurface, size: Option<Size<i32, Logical>>) {
    toplevel.with_pending_state(|state| {
        state.size = size;
        for flag in [
            xdg_toplevel::State::Activated,
            xdg_toplevel::State::TiledLeft,
            xdg_toplevel::State::TiledRight,
            xdg_toplevel::State::TiledTop,
            xdg_toplevel::State::TiledBottom,
        ] {
            state.states.set(flag);
        }
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every nest client is inserted with ClientState")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.on_commit(surface);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        self.contents
            .retain(|_, contents| contents.buffer != *buffer);
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.dh.get_client(surface.id()).ok());
        set_data_device_focus(&self.dh, seat, client);
        self.focus_text_input(focused);
    }

    /// GTK 3 sets its cursors as surfaces it draws, which name no shape; a
    /// host running the client in its own process reads the cursor from the
    /// toolkit instead.
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let text = source.and_then(|source| {
            let offered = source.mime_types();
            TEXT_MIME_TYPES
                .iter()
                .find(|mime| offered.iter().any(|offer| offer == *mime))
        });
        // smithay stores the new selection only after this returns, so it is
        // read once the request is dispatched (see
        // [`State::read_selection`]). A copy with no text in it has nothing
        // for the host's clipboard, and the nest's selection takes its place
        // at once.
        match text {
            Some(mime_type) => self.copied = Some((seat, *mime_type)),
            None => {
                let _ = self.commands.send(Command::OfferClipboard);
            }
        }
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        (): &(),
    ) {
        let _ = self.events.try_send(Event::Paste(Paste(fd)));
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

/// A drag the client starts is dropped on its own surfaces, which is all a
/// page's drag and drop needs. Nothing is dragged between the view and the
/// host's session: the nest has no part in the session's data device.
impl ClientDndGrabHandler for State {}

impl ServerDndGrabHandler for State {}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.unclaimed.push(surface);
        self.claim();
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.claim();
    }

    fn parent_changed(&mut self, _surface: ToplevelSurface) {
        self.claim();
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let Some(parent) = surface.get_parent_surface() else {
            return;
        };
        let Some((id, view)) = self
            .view_of(&parent)
            .and_then(|id| Some((id, self.views.get(&id)?)))
        else {
            surface.with_pending_state(|state| state.geometry = positioner.get_geometry());
            return;
        };
        let geometry = self.place_popup(view, &surface, positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        let Some(view) = self.views.get_mut(&id) else {
            return;
        };
        view.popups.push(Popup {
            surface,
            geometry,
            grab: false,
        });
        view.dirty = true;
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        for view in self.views.values_mut() {
            if let Some(popup) = view.popups.iter_mut().find(|p| p.surface == surface) {
                popup.grab = true;
            }
        }
        self.refocus();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let Some((id, view)) = self
            .view_of(surface.wl_surface())
            .and_then(|id| Some((id, self.views.get(&id)?)))
        else {
            return;
        };
        let geometry = self.place_popup(view, &surface, positioner);
        surface.with_pending_state(|state| {
            state.geometry = geometry;
            state.positioner = positioner;
        });
        let Some(view) = self.views.get_mut(&id) else {
            return;
        };
        if let Some(popup) = view.popups.iter_mut().find(|p| p.surface == surface) {
            popup.geometry = geometry;
        }
        view.dirty = true;
        surface.send_repositioned(token);
        if let Err(error) = surface.send_configure() {
            log::error!("wayland nest: popup configure failed: {error:?}");
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unclaimed.retain(|toplevel| *toplevel != surface);
        self.views.retain(|_, view| view.toplevel != surface);
        for view in self.views.values_mut() {
            let before = view.dialogs.len();
            view.dialogs.retain(|dialog| dialog.toplevel != surface);
            if view.dialogs.len() != before {
                view.dirty = true;
            }
        }
        self.refocus();
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        for view in self.views.values_mut() {
            let before = view.popups.len();
            view.popups.retain(|popup| popup.surface != surface);
            if view.popups.len() != before {
                view.dirty = true;
            }
        }
        self.refocus();
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
