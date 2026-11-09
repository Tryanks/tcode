//! The nested compositor: one client, software composition, input injected by
//! the host instead of read from devices.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use calloop::channel::{self, Channel, Sender};
use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, KeyboardHandle, Keycode, XkbConfig};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, DisconnectReason, ObjectId,
};
use smithay::reexports::wayland_server::protocol::{
    wl_callback::WlCallback, wl_seat, wl_surface::WlSurface,
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

use crate::compose::{Canvas, Image};
use crate::{Event, Frame, PointerEvent, ScrollSource, ViewId};

/// Text clipboard types, most preferred first.
const TEXT_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// Largest clipboard text read from the client.
const MAX_CLIPBOARD_BYTES: u64 = 16 << 20;

/// How long the compositor waits after its event loop fails, so a failure
/// that repeats does not spin.
const ERROR_PAUSE: Duration = Duration::from_millis(50);

/// Largest distance from a surface's origin, in either direction, that the
/// nest takes from the client; sums of a few of these stay within `i32`.
const MAX_COORDINATE: i32 = 1 << 20;

/// A running nested compositor.
///
/// The compositor thread outlives this handle and serves its client for the
/// life of the process: GDK exits the process when its display connection is
/// lost, and the embedded client is GTK in this process.
pub struct Nest {
    socket: String,
    commands: Sender<Command>,
    events: async_channel::Receiver<Event>,
}

enum Command {
    Configure {
        view: ViewId,
        size: Size<i32, Logical>,
    },
    Pointer {
        view: ViewId,
        event: PointerEvent,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    Focus(Option<ViewId>),
    Presented(ViewId),
    Clipboard(String),
}

impl Nest {
    /// Binds the socket in `$XDG_RUNTIME_DIR` and starts the compositor. The
    /// socket accepts one client and is unlinked once that client connects.
    pub fn start() -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
        let socket = format!("tcode-nest-{}", std::process::id());
        let path = runtime.join(&socket);
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
        std::thread::Builder::new()
            .name("wayland-nest".into())
            .spawn(move || {
                let Err(error) = run(
                    listener,
                    path.clone(),
                    command_source,
                    event_sender,
                    &ready_sender,
                );
                let _ = std::fs::remove_file(&path);
                log::error!("wayland nest did not start: {error}");
                let _ = ready_sender.send(Err(error));
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

    /// Sizes the view's window geometry, in physical pixels.
    pub fn configure(&self, view: ViewId, width: u32, height: u32) {
        let size = Size::from((width.max(1) as i32, height.max(1) as i32));
        self.send(Command::Configure { view, size });
    }

    pub fn pointer(&self, view: ViewId, event: PointerEvent) {
        self.send(Command::Pointer { view, event });
    }

    /// Presses or releases an evdev key on the view with keyboard focus.
    pub fn key(&self, code: u32, pressed: bool) {
        self.send(Command::Key { code, pressed });
    }

    /// Gives the keyboard to `view`, or takes it away.
    pub fn focus(&self, view: Option<ViewId>) {
        self.send(Command::Focus(view));
    }

    /// Reports that the view's last [`Event::Frame`] was shown, so its client
    /// may draw the next one.
    pub fn presented(&self, view: ViewId) {
        self.send(Command::Presented(view));
    }

    /// Offers `text` as the clipboard to the embedded client.
    pub fn set_clipboard(&self, text: String) {
        self.send(Command::Clipboard(text));
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::debug!("wayland nest is not running");
        }
    }
}

/// Runs the compositor; returns only if it cannot start.
fn run(
    listener: UnixListener,
    path: PathBuf,
    commands: Channel<Command>,
    events: async_channel::Sender<Event>,
    ready: &std::sync::mpsc::Sender<io::Result<()>>,
) -> io::Result<Infallible> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(io::Error::other)?;
    let display: Display<State> = Display::new().map_err(io::Error::other)?;
    let mut state = State::new(display.handle(), events)?;
    let handle = event_loop.handle();
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is only dropped with this source.
                let display = unsafe { display.get_mut() };
                let mut dispatched = catch_panic(|| display.dispatch_clients(state));
                // What the client sent after the request that panicked is
                // already read from its socket, so readiness will not report
                // it; it is dispatched from the client's buffer instead.
                while dispatched.is_none()
                    && let Some(client) = state.client.clone()
                {
                    dispatched =
                        catch_panic(|| display.backend().dispatch_single_client(state, client));
                }
                if let Some(Err(error)) = dispatched
                    && error.kind() != io::ErrorKind::WouldBlock
                {
                    log::warn!("wayland nest: dispatching the client failed: {error}");
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| io::Error::other(error.error))?;
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        if error.kind() != io::ErrorKind::WouldBlock {
                            log::warn!("wayland nest: accepting the client failed: {error}");
                        }
                        return Ok(PostAction::Continue);
                    }
                };
                let inserted = stream.set_nonblocking(true).and_then(|()| {
                    state
                        .dh
                        .insert_client(stream, Arc::new(ClientState::default()))
                });
                match inserted {
                    Ok(client) => {
                        state.client = Some(client.id());
                        let _ = std::fs::remove_file(&path);
                        Ok(PostAction::Remove)
                    }
                    Err(error) => {
                        log::warn!("wayland nest: adding the client failed: {error}");
                        Ok(PostAction::Continue)
                    }
                }
            },
        )
        .map_err(|error| io::Error::other(error.error))?;
    handle
        .insert_source(commands, |event, _, state| {
            if let channel::Event::Msg(command) = event {
                state.handle(command);
            }
        })
        .map_err(|error| io::Error::other(error.error))?;

    let _ = ready.send(Ok(()));
    loop {
        let turn = catch_panic(|| {
            let dispatched = event_loop.dispatch(None, &mut state);
            state.flush_frames();
            if let Err(error) = state.dh.flush_clients() {
                log::debug!("wayland nest: flushing the client failed: {error}");
            }
            dispatched
        });
        if let Some(Err(error)) = turn {
            log::warn!("wayland nest: the event loop failed: {error}");
            std::thread::sleep(ERROR_PAUSE);
        }
    }
}

/// Runs `f`, logging a panic instead of letting it end the compositor thread,
/// which would drop the display (see [`Nest`]).
fn catch_panic<T>(f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            log::error!("wayland nest: a handler panicked: {message}");
            None
        }
    }
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

struct View {
    toplevel: ToplevelSurface,
    size: Option<Size<i32, Logical>>,
    /// Bottom to top.
    popups: Vec<Popup>,
    /// Frame callbacks for content not yet sent to the host.
    callbacks: Vec<WlCallback>,
    /// Frame callbacks of the frame the host has not yet presented.
    in_flight: Option<Vec<WlCallback>>,
    dirty: bool,
}

struct State {
    dh: DisplayHandle,
    /// The one client the socket accepted.
    client: Option<ClientId>,
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
    images: HashMap<ObjectId, (WlSurface, Image)>,
    keyboard_view: Option<ViewId>,
    /// The clipboard text last exchanged in either direction.
    clipboard: Option<Arc<str>>,
    events: async_channel::Sender<Event>,
    epoch: Instant,
}

impl State {
    fn new(dh: DisplayHandle, events: async_channel::Sender<Event>) -> io::Result<Self> {
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

        Ok(Self {
            dh,
            client: None,
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
            images: HashMap::new(),
            keyboard_view: None,
            clipboard: None,
            events,
            epoch: Instant::now(),
        })
    }

    fn time(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Configure { view, size } => match self.views.get_mut(&view) {
                Some(entry) => {
                    if entry.size != Some(size) {
                        entry.size = Some(size);
                        configure(&entry.toplevel, entry.size);
                    }
                }
                None => {
                    self.requested.insert(view, size);
                }
            },
            Command::Pointer { view, event } => self.pointer_event(view, event),
            Command::Key { code, pressed } => {
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
            Command::Focus(view) => {
                self.keyboard_view = view;
                self.refocus();
            }
            Command::Clipboard(text) => {
                if self.clipboard.as_deref() != Some(text.as_str()) {
                    let text: Arc<str> = text.into();
                    self.clipboard = Some(text.clone());
                    let mime_types = TEXT_MIME_TYPES.map(String::from).to_vec();
                    set_data_device_selection(&self.dh, &self.seat, mime_types, text);
                }
            }
            Command::Presented(view) => {
                let time = self.time();
                if let Some(callbacks) = self.views.get_mut(&view).and_then(|v| v.in_flight.take())
                {
                    for callback in callbacks {
                        callback.done(time);
                    }
                }
            }
        }
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
                match shm::with_buffer_contents(&buffer, Image::import) {
                    Ok(Some(image)) => {
                        self.images.insert(surface.id(), (surface.clone(), image));
                    }
                    Ok(None) => log::debug!("wayland nest: unsupported shm buffer"),
                    Err(error) => log::debug!("wayland nest: unreadable buffer: {error}"),
                }
                buffer.release();
            }
            Some(BufferAssignment::Removed) => {
                self.images.remove(&surface.id());
            }
            None => {}
        }
        self.send_initial_configure(surface);

        let mut root = surface.clone();
        while let Some(parent) = compositor::get_parent(&root) {
            root = parent;
        }
        match self.view_of(&root).and_then(|id| self.views.get_mut(&id)) {
            Some(view) => {
                view.callbacks.extend(callbacks);
                view.dirty = true;
            }
            None => {
                let time = self.time();
                for callback in callbacks {
                    callback.done(time);
                }
            }
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
            for popup in &view.popups {
                if popup.surface.wl_surface() == surface
                    && !popup.surface.is_initial_configure_sent()
                    && let Err(error) = popup.surface.send_configure()
                {
                    log::debug!("wayland nest: popup configure failed: {error:?}");
                }
            }
        }
    }

    /// The view whose toplevel or popup is `surface`.
    fn view_of(&self, surface: &WlSurface) -> Option<ViewId> {
        self.views.iter().find_map(|(id, view)| {
            (view.toplevel.wl_surface() == surface
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
                index += 1;
                continue;
            };
            let toplevel = self.unclaimed.swap_remove(index);
            let size = self.requested.remove(&id);
            configure(&toplevel, size);
            let view = View {
                toplevel,
                size,
                popups: Vec::new(),
                callbacks: Vec::new(),
                in_flight: None,
                dirty: true,
            };
            self.views.insert(id, view);
            if self.keyboard_view == Some(id) {
                self.refocus();
            }
        }
    }

    /// The size frames of `view` have: what it was configured to once its
    /// client has drawn, otherwise whatever it drew.
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
        match declared {
            Some(geometry) => Some(Rectangle::new(clamp_point(geometry.loc), geometry.size)),
            None => self.images.get(&surface.id()).map(|(_, image)| {
                Rectangle::new((0, 0).into(), (image.width, image.height).into())
            }),
        }
    }

    /// Where the window geometry of `surface` (the view's toplevel or one of
    /// its popups) starts, in the view's coordinates.
    fn window_origin(&self, view: &View, surface: &WlSurface) -> Point<i32, Logical> {
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
        let geometry = positioner.get_unconstrained_geometry(bounds);
        Rectangle::new(clamp_point(geometry.loc), geometry.size)
    }

    fn flush_frames(&mut self) {
        let ready: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(_, view)| view.dirty && view.in_flight.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            let Some(frame) = self.composite(id) else {
                continue;
            };
            let Some(view) = self.views.get_mut(&id) else {
                continue;
            };
            view.dirty = false;
            view.in_flight = Some(std::mem::take(&mut view.callbacks));
            // With no host to present it, the frame stays in flight and the
            // client is not asked to draw again.
            if self
                .events
                .try_send(Event::Frame { view: id, frame })
                .is_err()
            {
                log::debug!("wayland nest: no host to present frames");
            }
        }
        self.images.retain(|_, (surface, _)| surface.is_alive());
    }

    fn composite(&self, id: ViewId) -> Option<Frame> {
        let view = self.views.get(&id)?;
        let toplevel = view.toplevel.wl_surface();
        let geometry = self.window_geometry(toplevel)?;
        let mut canvas = Canvas::new(geometry.size.w, geometry.size.h);
        self.draw_tree(
            &mut canvas,
            toplevel,
            Point::from((-geometry.loc.x, -geometry.loc.y)),
        );
        for popup in &view.popups {
            let surface = popup.surface.wl_surface();
            let origin = self.window_origin(view, surface);
            let Some(own) = self.window_geometry(surface) else {
                continue;
            };
            self.draw_tree(&mut canvas, surface, origin - own.loc);
        }
        Some(Frame {
            width: canvas.width as u32,
            height: canvas.height as u32,
            pixels: canvas.pixels,
        })
    }

    fn draw_tree(&self, canvas: &mut Canvas, surface: &WlSurface, origin: Point<i32, Logical>) {
        compositor::with_surface_tree_upward(
            surface,
            origin,
            |_, states, parent| TraversalAction::DoChildren(*parent + subsurface_offset(states)),
            |surface, states, parent| {
                let location = *parent + subsurface_offset(states);
                if let Some((_, image)) = self.images.get(&surface.id()) {
                    canvas.draw(image, location.x, location.y);
                }
            },
            |_, _, _| true,
        );
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
                let own = self
                    .window_geometry(surface)
                    .map(|g| g.loc)
                    .unwrap_or_default();
                return Some((surface.clone(), (origin - own).to_f64()));
            }
        }
        let toplevel = view.toplevel.wl_surface();
        let own = self
            .window_geometry(toplevel)
            .map(|g| g.loc)
            .unwrap_or_default();
        Some((toplevel.clone(), Point::from((-own.x, -own.y)).to_f64()))
    }

    /// A press outside every popup of a grabbing menu dismisses the menu and
    /// is not delivered, as on a desktop compositor.
    fn dismiss_popups(&mut self, id: ViewId, location: Point<f64, Logical>) -> bool {
        let Some(view) = self.views.get(&id) else {
            return false;
        };
        if !view.popups.iter().any(|p| p.grab) {
            return false;
        }
        let inside = view.popups.iter().any(|popup| {
            let origin = self.window_origin(view, popup.surface.wl_surface());
            Rectangle::new(origin, popup.geometry.size)
                .to_f64()
                .contains(location)
        });
        if inside {
            return false;
        }
        for popup in view.popups.iter().rev() {
            popup.surface.send_popup_done();
        }
        true
    }

    fn pointer_event(&mut self, id: ViewId, event: PointerEvent) {
        let pointer = self.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        match event {
            PointerEvent::Motion { x, y } => {
                let location = Point::from((x, y));
                let focus = self.surface_under(id, location);
                pointer.motion(
                    self,
                    focus,
                    &MotionEvent {
                        location,
                        serial,
                        time,
                    },
                );
            }
            PointerEvent::Leave => {
                let location = pointer.current_location();
                pointer.motion(
                    self,
                    None,
                    &MotionEvent {
                        location,
                        serial,
                        time,
                    },
                );
            }
            PointerEvent::Button { button, pressed } => {
                if pressed && self.dismiss_popups(id, pointer.current_location()) {
                    return;
                }
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state,
                    },
                );
            }
            PointerEvent::Scroll {
                dx,
                dy,
                source,
                discrete,
                stop,
            } => {
                let mut frame = AxisFrame::new(time).source(match source {
                    ScrollSource::Wheel => AxisSource::Wheel,
                    ScrollSource::Finger => AxisSource::Finger,
                });
                if dx != 0.0 {
                    frame = frame.value(Axis::Horizontal, dx);
                }
                if dy != 0.0 {
                    frame = frame.value(Axis::Vertical, dy);
                }
                if let Some((horizontal, vertical)) = discrete {
                    if horizontal != 0 {
                        frame = frame.v120(Axis::Horizontal, horizontal);
                    }
                    if vertical != 0 {
                        frame = frame.v120(Axis::Vertical, vertical);
                    }
                }
                if stop {
                    frame = frame.stop(Axis::Horizontal).stop(Axis::Vertical);
                }
                pointer.axis(self, frame);
            }
        }
        pointer.frame(self);
    }

    /// Gives the keyboard to the focused view's topmost grabbing popup, or to
    /// its toplevel.
    fn refocus(&mut self) {
        let target = self.keyboard_view.and_then(|id| {
            let view = self.views.get(&id)?;
            Some(
                view.popups
                    .iter()
                    .rev()
                    .find(|p| p.grab)
                    .map(|p| p.surface.wl_surface().clone())
                    .unwrap_or_else(|| view.toplevel.wl_surface().clone()),
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
        clamp_point(
            states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location,
        )
    } else {
        Point::from((0, 0))
    }
}

/// Bounds a position the client chose to [`MAX_COORDINATE`].
fn clamp_point(point: Point<i32, Logical>) -> Point<i32, Logical> {
    Point::from((
        point.x.clamp(-MAX_COORDINATE, MAX_COORDINATE),
        point.y.clamp(-MAX_COORDINATE, MAX_COORDINATE),
    ))
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
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
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
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl SelectionHandler for State {
    type SelectionUserData = Arc<str>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let Some(source) = source else {
            return;
        };
        let offered = source.mime_types();
        let Some(mime_type) = TEXT_MIME_TYPES
            .iter()
            .find(|mime| offered.iter().any(|offer| offer == *mime))
        else {
            return;
        };
        let (read, write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
            Ok(pipe) => pipe,
            Err(error) => {
                log::debug!("wayland nest: clipboard pipe failed: {error}");
                return;
            }
        };
        if let Err(error) =
            request_data_device_client_selection(&seat, mime_type.to_string(), write)
        {
            log::debug!("wayland nest: clipboard request failed: {error:?}");
            return;
        }
        // The client writes from its own thread of control; reading here
        // would block the compositor it is waiting on.
        let events = self.events.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let read = std::fs::File::from(read)
                .take(MAX_CLIPBOARD_BYTES)
                .read_to_string(&mut text);
            if read.is_ok() && !text.is_empty() {
                let _ = events.try_send(Event::Clipboard(text));
            }
        });
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        text: &Arc<str>,
    ) {
        let text = text.clone();
        std::thread::spawn(move || {
            let _ = std::fs::File::from(fd).write_all(text.as_bytes());
        });
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

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
            log::debug!("wayland nest: popup configure failed: {error:?}");
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unclaimed.retain(|toplevel| *toplevel != surface);
        let closed: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(_, view)| view.toplevel == surface)
            .map(|(id, _)| *id)
            .collect();
        for id in closed {
            self.views.remove(&id);
            let _ = self.events.try_send(Event::Closed { view: id });
        }
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
