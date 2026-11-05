//! The agent cursor on Windows: a small click-through, topmost layered window
//! that never activates, drawn per frame with `UpdateLayeredWindow` on its own
//! UI thread. It shares the macOS cursor's lifetime and motion and draws the
//! same disc, glow, ripple and press, from the same numbers.
//!
//! What it does not match: a drag ripples once at its press and once at its
//! release instead of trailing ripples that stay where they were emitted,
//! because the window is the size of one ripple rather than the display; the
//! accent is the system highlight colour; the size follows the system DPI,
//! not the monitor's.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    COLOR_HIGHLIGHT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDC, GetDeviceCaps, GetSysColor, HBITMAP, HDC, HGDIOBJ, LOGPIXELSX, ReleaseDC, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GA_ROOT, GetAncestor,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassW, SPI_GETCLIENTAREAANIMATION, SW_HIDE, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ShowWindow, SystemParametersInfoW, TranslateMessage,
    ULW_ALPHA, UpdateLayeredWindow, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WindowFromPoint,
};
use windows::core::w;

use crate::backend::cursor::motion::{Effect, Fade, Motion, Point};
pub(crate) use crate::backend::cursor::{CursorArrival, OverlayActionKind, begin_action};
use crate::backend::cursor::{Publication, QueuedCommand, UiCommand};

pub(crate) fn set_enabled(on: bool) {
    crate::backend::cursor::set_enabled(on, enqueue);
}

pub(crate) fn clear(owner: Option<u64>, action: Option<u64>) {
    crate::backend::cursor::clear(owner, action, enqueue);
}

pub(crate) fn show_action(
    publication: Publication,
    window_id: u32,
    kind: OverlayActionKind,
    point: Option<Point>,
    paced: bool,
) -> CursorArrival {
    publish(crate::backend::cursor::action(
        publication,
        window_id,
        kind,
        point,
        paced,
    ))
}

pub(crate) fn show_drag(
    publication: Publication,
    window_id: u32,
    from: Point,
    to: Point,
    paced: bool,
) -> CursorArrival {
    publish(crate::backend::cursor::drag(
        publication,
        window_id,
        from,
        to,
        paced,
    ))
}

fn publish(queued: Option<(QueuedCommand, CursorArrival)>) -> CursorArrival {
    let Some((command, arrival)) = queued else {
        return CursorArrival::none();
    };
    enqueue(command);
    arrival
}

static UI_THREAD: Mutex<Option<Sender<QueuedCommand>>> = Mutex::new(None);

/// A command that cannot reach the UI thread is dropped, which releases any
/// action waiting for its cursor.
fn enqueue(command: QueuedCommand) {
    let mut sender = UI_THREAD.lock().unwrap();
    if sender.is_none() {
        let (commands, received) = channel();
        if thread::Builder::new()
            .name("tcode-cu-cursor".into())
            .spawn(move || run(received))
            .is_err()
        {
            log::debug!("could not start the agent cursor thread");
            return;
        }
        *sender = Some(commands);
    }
    if sender
        .as_ref()
        .is_some_and(|commands| commands.send(command).is_err())
    {
        *sender = None;
    }
}

const FRAME: Duration = Duration::from_millis(16);
const TARGET_POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_FRAME_STEP: Duration = Duration::from_millis(50);

fn run(commands: Receiver<QueuedCommand>) {
    let mut overlay = Overlay::new();
    loop {
        let received = match overlay.next_wake() {
            Some(wait) => commands.recv_timeout(wait),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(command) => overlay.receive(command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(command) = commands.try_recv() {
            overlay.receive(command);
        }
        pump_messages();
        overlay.tick(Instant::now());
    }
}

struct Target {
    window_id: u32,
    point: Option<Point>,
    publication: Publication,
}

#[derive(Clone, Copy)]
enum Ripple {
    Click,
    Trail,
}

impl Ripple {
    /// (from scale, to scale, from opacity, duration, line width). Under
    /// Reduce Motion a ripple keeps its full size and only fades.
    fn animation(self, reduced: bool) -> (f64, f64, f64, Duration, f64) {
        let (from, to, opacity, seconds, line_width) = match self {
            Self::Click => (0.45, 1.7, 0.9, 0.5, 2.0),
            Self::Trail => (0.35, 0.95, 0.55, 0.38, 1.25),
        };
        let from = if reduced { to } else { from };
        (
            from,
            to,
            opacity,
            Duration::from_secs_f64(seconds),
            line_width,
        )
    }
}

struct Overlay {
    window: Option<CursorWindow>,
    motion: Motion,
    fade: Fade,
    target: Option<Target>,
    shown: bool,
    reduced: bool,
    click: Option<Instant>,
    ripple: Option<(Ripple, Instant)>,
    last_frame: Instant,
    last_poll: Instant,
}

impl Overlay {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            window: None,
            motion: Motion::new(seed()),
            fade: Fade::hidden(),
            target: None,
            shown: false,
            reduced: false,
            click: None,
            ripple: None,
            last_frame: now,
            last_poll: now,
        }
    }

    fn receive(&mut self, command: QueuedCommand) {
        let now = Instant::now();
        crate::backend::cursor::run_accepted(command, now, |command, publication| match command {
            UiCommand::Show {
                window_id,
                point,
                waypoints,
            } => self.show(window_id, point, publication, waypoints, now),
            UiCommand::Clear => self.clear(now),
        });
    }

    fn show(
        &mut self,
        window_id: u32,
        point: Option<Point>,
        publication: Publication,
        mut waypoints: Vec<crate::backend::cursor::motion::Waypoint>,
        now: Instant,
    ) {
        let point = point.or_else(|| {
            self.target
                .as_ref()
                .filter(|current| current.window_id == window_id)
                .and_then(|current| current.point)
        });
        let overlay = self.window.as_ref().map(|window| window.hwnd);
        let Some(shows_point) = window_shows_point(window_id, point, overlay) else {
            self.clear(now);
            return;
        };
        self.target = Some(Target {
            window_id,
            point,
            publication,
        });
        if !shows_point {
            // Nothing is drawn there, so the action need not wait for it.
            waypoints
                .iter_mut()
                .for_each(|waypoint| waypoint.reached = None);
        }
        if self.window.is_none() && !waypoints.is_empty() {
            self.window = CursorWindow::new();
        }
        if self.window.is_none() {
            return;
        }
        self.reduced = reduce_motion();
        self.motion.set_reduced(self.reduced);
        for effect in self.motion.push(now, waypoints) {
            self.play(effect, now);
        }
        self.shown = shows_point;
    }

    /// The last position is kept, so the next action glides from it.
    fn clear(&mut self, now: Instant) {
        self.motion.halt(now);
        self.shown = false;
        self.target = None;
    }

    fn play(&mut self, effect: Effect, now: Instant) {
        match effect {
            Effect::Click => {
                self.click = Some(now);
                self.ripple = Some((Ripple::Click, now));
            }
            Effect::Press | Effect::Release => self.ripple = Some((Ripple::Trail, now)),
        }
    }

    fn rippling(&self, now: Instant) -> bool {
        self.ripple
            .is_some_and(|(kind, start)| now < start + kind.animation(self.reduced).3)
            || self
                .click
                .is_some_and(|start| now < start + Duration::from_millis(450))
    }

    fn next_wake(&self) -> Option<Duration> {
        let now = Instant::now();
        let animating = self.motion.is_moving()
            || self.motion.is_pressed()
            || self.rippling(now)
            || self.fade.opacity() != if self.shown { 1.0 } else { 0.0 };
        if animating {
            Some(FRAME)
        } else if self.target.is_some() {
            Some(TARGET_POLL_INTERVAL)
        } else {
            None
        }
    }

    fn tick(&mut self, now: Instant) {
        if now >= self.last_poll + TARGET_POLL_INTERVAL {
            self.last_poll = now;
            self.refresh_visibility(now);
        }
        for effect in self.motion.advance(now) {
            self.play(effect, now);
        }
        let step = now
            .saturating_duration_since(self.last_frame)
            .min(MAX_FRAME_STEP);
        self.last_frame = now;
        let position = self.motion.position(now);
        self.fade.step(step, self.shown && position.is_some());
        // Every pixel, ripple and glow included, is drawn at the cursor's
        // opacity, so nothing stays on screen once the cursor has faded.
        let opacity = self.fade.opacity();
        let drawn = position
            .filter(|_| opacity > 0.0)
            .map(|point| (point, self.frame(now, opacity)));
        let Some(window) = self.window.as_mut() else {
            return;
        };
        match drawn {
            Some((point, frame)) => window.draw(point, &frame),
            None => window.hide(),
        }
    }

    fn refresh_visibility(&mut self, now: Instant) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let overlay = self.window.as_ref().map(|window| window.hwnd);
        let shows_point = (crate::backend::cursor::is_enabled()
            && target.publication.is_current(now))
        .then(|| window_shows_point(target.window_id, target.point, overlay))
        .flatten();
        match shows_point {
            Some(shown) => self.shown = shown,
            None => self.clear(now),
        }
    }

    fn frame(&self, now: Instant, opacity: f64) -> CursorFrame {
        let pressed = if self.motion.is_pressed() {
            PRESSED_SCALE
        } else {
            1.0
        };
        let since_click = self.click.map(|start| now.saturating_duration_since(start));
        // A click dips the pointer to its pressed size and back.
        let dip = since_click
            .filter(|_| !self.reduced)
            .map(|elapsed| elapsed.as_secs_f64() / 0.09)
            .filter(|t| *t < 2.0)
            .map_or(1.0, |t| {
                let t = if t < 1.0 { t } else { 2.0 - t };
                1.0 - (1.0 - PRESSED_SCALE) * ease_out(t)
            });
        let glow = since_click
            .map(|elapsed| elapsed.as_secs_f64() / 0.45)
            .filter(|t| *t < 1.0)
            .map_or(0.0, |t| 1.0 - ease_out(t));
        let ripple = self.ripple.and_then(|(kind, start)| {
            let (from, to, alpha, duration, line_width) = kind.animation(self.reduced);
            let t = now.saturating_duration_since(start).as_secs_f64() / duration.as_secs_f64();
            (t < 1.0).then(|| {
                let eased = ease_out(t);
                (
                    RIPPLE_RADIUS * (from + (to - from) * eased),
                    alpha * (1.0 - eased),
                    line_width,
                )
            })
        });
        CursorFrame {
            opacity,
            pointer_scale: pressed.min(dip),
            glow,
            ripple,
        }
    }
}

fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

// The pointer is a translucent disc whose centre is the action point.
const POINTER_RADIUS: f64 = 10.0;
const RIPPLE_RADIUS: f64 = 20.0;
const PRESSED_SCALE: f64 = 0.72;
const GLOW_REACH: f64 = 12.0;
// Half the window's side, in points: the largest ripple and its line.
const EXTENT: f64 = RIPPLE_RADIUS * 1.7 + 4.0;

struct CursorFrame {
    opacity: f64,
    pointer_scale: f64,
    glow: f64,
    /// (radius, opacity, line width) of the current ripple.
    ripple: Option<(f64, f64, f64)>,
}

/// Premultiplied RGBA in 0..=1.
#[derive(Clone, Copy, Default)]
struct Pixel([f64; 4]);

impl Pixel {
    fn over(self, (r, g, b): (f64, f64, f64), alpha: f64) -> Self {
        let alpha = alpha.clamp(0.0, 1.0);
        let [dr, dg, db, da] = self.0;
        let keep = 1.0 - alpha;
        Self([
            r * alpha + dr * keep,
            g * alpha + dg * keep,
            b * alpha + db * keep,
            alpha + da * keep,
        ])
    }

    fn bgra(self, opacity: f64) -> u32 {
        let channel = |value: f64| (value * opacity * 255.0).round().clamp(0.0, 255.0) as u32;
        let [r, g, b, a] = self.0;
        (channel(a) << 24) | (channel(r) << 16) | (channel(g) << 8) | channel(b)
    }
}

/// Coverage of a ring of `width` centred on `radius` at distance `distance`.
fn ring(distance: f64, radius: f64, width: f64) -> f64 {
    (width / 2.0 + 0.5 - (distance - radius).abs()).clamp(0.0, 1.0)
}

fn disc(distance: f64, radius: f64) -> f64 {
    (radius - distance + 0.5).clamp(0.0, 1.0)
}

fn shade(distance: f64, frame: &CursorFrame, accent: (f64, f64, f64)) -> Pixel {
    let radius = POINTER_RADIUS * frame.pointer_scale;
    let mut pixel = Pixel::default();
    if let Some((ripple_radius, alpha, width)) = frame.ripple {
        pixel = pixel
            .over(accent, 0.12 * alpha * disc(distance, ripple_radius))
            .over(accent, 0.9 * alpha * ring(distance, ripple_radius, width));
    }
    if frame.glow > 0.0 {
        let reach = ((distance - radius) / GLOW_REACH).clamp(0.0, 1.0);
        pixel = pixel.over(accent, 0.85 * frame.glow * (1.0 - reach).powi(2));
    }
    pixel
        .over(
            (0.0, 0.0, 0.0),
            0.22 * ring(distance, radius + 1.0, 2.0) * (1.0 - disc(distance, radius)),
        )
        .over(accent, 0.42 * disc(distance, radius))
        .over((1.0, 1.0, 1.0), 0.75 * ring(distance, radius - 0.5, 1.0))
}

struct CursorWindow {
    hwnd: HWND,
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u32,
    side: i32,
    scale: f64,
    accent: (f64, f64, f64),
    visible: bool,
}

impl CursorWindow {
    fn new() -> Option<Self> {
        // SAFETY: every handle created here is checked and owned by the
        // returned value, which releases each exactly once in Drop; the class
        // is registered once for this thread's lifetime.
        unsafe {
            let instance = GetModuleHandleW(None).ok()?;
            let class_name = w!("TcodeAgentCursor");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: class_name,
                ..WNDCLASSW::default()
            };
            let _ = RegisterClassW(&class);
            let screen = GetDC(None);
            if screen.is_invalid() {
                return None;
            }
            let scale = (f64::from(GetDeviceCaps(Some(screen), LOGPIXELSX)) / 96.0).max(1.0);
            let side = (EXTENT * 2.0 * scale).ceil() as i32;
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class_name,
                w!(""),
                WS_POPUP,
                0,
                0,
                side,
                side,
                None,
                None,
                Some(instance.into()),
                None,
            ) else {
                let _ = ReleaseDC(None, screen);
                return None;
            };
            let memory = CreateCompatibleDC(Some(screen));
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: side,
                    biHeight: -side,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..BITMAPINFOHEADER::default()
                },
                ..BITMAPINFO::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = if memory.is_invalid() {
                None
            } else {
                CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()
            };
            let Some(bitmap) = bitmap.filter(|_| !bits.is_null()) else {
                if let Some(bitmap) = bitmap {
                    let _ = DeleteObject(bitmap.into());
                }
                if !memory.is_invalid() {
                    let _ = DeleteDC(memory);
                }
                let _ = ReleaseDC(None, screen);
                let _ = DestroyWindow(hwnd);
                return None;
            };
            let previous = SelectObject(memory, bitmap.into());
            let color = GetSysColor(COLOR_HIGHLIGHT);
            let channel = |shift: u32| f64::from((color >> shift) & 0xFF) / 255.0;
            Some(Self {
                hwnd,
                screen,
                memory,
                bitmap,
                previous,
                bits: bits.cast(),
                side,
                scale,
                accent: (channel(0), channel(8), channel(16)),
                visible: false,
            })
        }
    }

    fn draw(&mut self, (x, y): Point, frame: &CursorFrame) {
        let side = self.side as usize;
        let center = f64::from(self.side) / 2.0;
        // SAFETY: the DIB section holds side × side 32-bit pixels and lives as
        // long as self.
        let pixels = unsafe { std::slice::from_raw_parts_mut(self.bits, side * side) };
        for (index, pixel) in pixels.iter_mut().enumerate() {
            let dx = (index % side) as f64 + 0.5 - center;
            let dy = (index / side) as f64 + 0.5 - center;
            let distance = dx.hypot(dy) / self.scale;
            *pixel = shade(distance, frame, self.accent).bgra(frame.opacity);
        }
        let origin = POINT {
            x: (x - center).round() as i32,
            y: (y - center).round() as i32,
        };
        let size = SIZE {
            cx: self.side,
            cy: self.side,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: u8::MAX,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: all handles are owned by self and the structs outlive the call.
        unsafe {
            if let Err(error) = UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen),
                Some(&origin),
                Some(&size),
                Some(self.memory),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                log::debug!("could not draw the agent cursor: {error}");
                return;
            }
            if !self.visible {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.visible = true;
            }
        }
    }

    fn hide(&mut self) {
        if self.visible {
            // SAFETY: hwnd is owned by self.
            let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            self.visible = false;
        }
    }
}

impl Drop for CursorWindow {
    fn drop(&mut self) {
        // SAFETY: each handle was created in new and is released exactly once.
        unsafe {
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory);
            let _ = ReleaseDC(None, self.screen);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: forwards this window's messages to the default procedure.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: `message` is valid writable memory; only this thread's windows
    // are pumped.
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// `None` once the window is gone, hidden or minimized. Otherwise whether the
/// window under `point` belongs to the window's process, so its own menus and
/// popups count as showing the point.
fn window_shows_point(window_id: u32, point: Option<Point>, overlay: Option<HWND>) -> Option<bool> {
    let root = HWND(window_id as usize as *mut _);
    // SAFETY: these calls only inspect window handles and write process ids
    // into local integers.
    unsafe {
        if root.is_invalid()
            || !IsWindow(Some(root)).as_bool()
            || !IsWindowVisible(root).as_bool()
            || IsIconic(root).as_bool()
        {
            return None;
        }
        let Some((x, y)) = point else {
            return Some(true);
        };
        if !x.is_finite() || !y.is_finite() {
            return Some(false);
        }
        let hit = WindowFromPoint(POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        });
        if hit.is_invalid() {
            return Some(false);
        }
        if Some(hit) == overlay {
            return Some(true);
        }
        let mut root_process = 0;
        let mut hit_process = 0;
        GetWindowThreadProcessId(root, Some(&mut root_process));
        GetWindowThreadProcessId(GetAncestor(hit, GA_ROOT), Some(&mut hit_process));
        Some(root_process != 0 && hit_process == root_process)
    }
}

/// Windows' "Animation effects" setting off is its Reduce Motion.
fn reduce_motion() -> bool {
    let mut animations = windows::core::BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the given pointer.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut animations).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_ok() && !animations.as_bool()
}

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}
