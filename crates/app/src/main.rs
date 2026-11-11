// Windows: run as a GUI app so launching tcode does not open a console window.
// Debug builds keep the console so `RUST_LOG` output stays visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{borrow::Cow, cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    AnyWindowHandle, App, BorrowAppContext as _, Entity, ParentElement as _, Styled as _,
    TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions,
    point, px, size,
};
use tcode_client::{HostLink, host::ClientHost as _, host::Transport};
use tcode_protocol::{Command, CommandResponse};
use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_services::{shell_env, store::SessionStore};
use tcode_traverse::{HostMux, NativeClientHost};
use tcode_ui::remote::{AttachmentTarget, RemoteController, machine_name};
use tcode_ui::{
    AppShell, Quit, ShellOptions, ShellPlace, ShellSetup, WindowState, theme::ActiveTheme as _,
};
use tcode_ui::{assets, settings};

use tcode_ui::overlay::{DialogActions, OverlayExt as _};
use tcode_ui::widgets::button::{Button, ButtonVariants as _};

mod editor;
mod migration;

/// macOS vibrancy can be disabled with `TCODE_NO_VIBRANCY=1` as a diagnostic
/// escape hatch (opaque window + flattened palette).
fn vibrancy_enabled() -> bool {
    cfg!(target_os = "macos") && !std::env::var("TCODE_NO_VIBRANCY").is_ok_and(|v| v == "1")
}

fn translucent_canvas_enabled() -> bool {
    cfg!(target_os = "windows") || vibrancy_enabled()
}

fn main_window_background() -> WindowBackgroundAppearance {
    if cfg!(target_os = "windows") {
        WindowBackgroundAppearance::Blurred
    } else if vibrancy_enabled() {
        // `run_shell` slides Liquid Glass under a transparent window; GPUI's
        // `Blurred` is a colorless blur with no glass option.
        WindowBackgroundAppearance::Transparent
    } else {
        WindowBackgroundAppearance::Opaque
    }
}

fn finish_quit_prompt(window_state: &Entity<WindowState>, cx: &mut App) {
    window_state.update(cx, |state, _| state.quit_prompt_open = false);
}

fn handle_quit(_: &Quit, shell: &Entity<AppShell>, cx: &mut App) {
    // Quitting stops only this machine's host; other machines' work goes on.
    let (working, background) = shell
        .read(cx)
        .local_store()
        .map(|store| {
            let store = store.read(cx);
            (
                store.working_sessions_count(),
                store.background_sessions_count(),
            )
        })
        .unwrap_or((0, 0));
    if working == 0 && background == 0 {
        cx.quit();
        return;
    }
    let description = if working > 0 {
        tcode_ui::tr!("quit.description", count = working)
    } else {
        tcode_ui::tr!("quit.description_background", count = background)
    };

    let Some(window_handle) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        cx.quit();
        return;
    };

    let window_state = shell.read(cx).window_state();
    let opened = window_state.update(cx, |state, _| {
        !std::mem::replace(&mut state.quit_prompt_open, true)
    });
    if !opened {
        return;
    }

    let prompt_state = window_state.clone();
    if window_handle
        .update(cx, move |_, window, cx| {
            let quit_state = prompt_state.clone();
            let cancel_state = prompt_state.clone();
            let enter_state = prompt_state.clone();
            let escape_state = prompt_state.clone();
            window.open_alert_dialog(cx, move |alert, _, cx| {
                let alert = alert.bg(cx.theme().popover);
                let quit_state = quit_state.clone();
                let cancel_state = cancel_state.clone();
                let enter_state = enter_state.clone();
                let escape_state = escape_state.clone();
                alert
                    .title(tcode_ui::tr!("quit.title"))
                    .description(description.clone())
                    // The stock alert maps Enter to OK. A custom footer keeps
                    // both Enter and Escape safe while retaining the alert's
                    // normal visual style and explicit danger action.
                    .footer(
                        DialogActions::new()
                            .child(
                                Button::new("quit-working-sessions")
                                    .label(tcode_ui::tr!("quit.confirm"))
                                    .danger()
                                    .on_click(move |_, window, cx| {
                                        finish_quit_prompt(&quit_state, cx);
                                        window.close_dialog(cx);
                                        cx.quit();
                                    }),
                            )
                            .child(
                                Button::new("cancel-quit")
                                    .label(tcode_ui::tr!("settings.cancel"))
                                    .primary()
                                    .on_click(move |_, window, cx| {
                                        finish_quit_prompt(&cancel_state, cx);
                                        window.close_dialog(cx);
                                    }),
                            ),
                    )
                    .on_ok(move |_, _, cx| {
                        finish_quit_prompt(&enter_state, cx);
                        true
                    })
                    .on_cancel(move |_, _, cx| {
                        finish_quit_prompt(&escape_state, cx);
                        true
                    })
            });
        })
        .is_err()
    {
        finish_quit_prompt(&window_state, cx);
        cx.quit();
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

/// Hidden `tcode --pair <tcode://pair?…>`: pair with the machine an invite
/// names, record it in `hosts.json`, print its id and exit. The desktop
/// pairing UI does the same thing; this is the headless path used by the
/// remote e2e run.
fn pair_command(args: &[String], client_host: &NativeClientHost) -> Result<String, String> {
    let [invite] = args else {
        return Err("usage: tcode --pair <tcode://pair?...>".into());
    };
    let invite = tcode_client::pairing::parse_pair_url(invite).ok_or("invalid invite link")?;
    let host = smol::block_on(client_host.pair(invite, None)).map_err(|error| error.to_string())?;
    let host_id = host.host_id.clone();
    client_host
        .remember_host(host)
        .map_err(|error| format!("paired, but the machine could not be saved: {error}"))?;
    Ok(host_id)
}

/// Start the in-process host and put the mux in front of it. This window is
/// then one ordinary client among every link attached to that mux.
fn start_local(
    store: SessionStore,
    login_shell_unread: Option<String>,
) -> std::io::Result<(SpawnedHost, HostMux)> {
    let mut host_services = HostServices {
        background_startup_probes: true,
        ai_title_generation: true,
        login_shell_unread,
        ..HostServices::default()
    };
    host_services.start_mcp_servers(true);
    let host = spawn_host(store, host_services)?;
    let mux = HostMux::new(host.attachments.clone(), host.from_host.clone())
        .with_original_backend(std::sync::Arc::new(host.clone()));
    Ok((host, mux))
}

/// No data directory can even be named, so there is nowhere for this client's
/// own files and no window to compose. Say why on stderr and in a dialog, then
/// exit non-zero.
fn exit_with_startup_failure(error: std::io::Error) -> ! {
    struct StartupFailure;
    impl gpui::Render for StartupFailure {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    eprintln!("tcode: {error}");
    let detail = error.to_string();
    gpui_platform::application().run(move |cx| {
        // gpui ends a quit with status 0; this observer is the last code that
        // runs before it.
        cx.on_app_quit(|_| async { std::process::exit(1) }).detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(420.), px(160.)), cx)),
            ..Default::default()
        };
        let opened = cx.open_window(options, |_, cx| {
            gpui::AppContext::new(cx, |_| StartupFailure)
        });
        let Ok(window) = opened else {
            cx.quit();
            return;
        };
        cx.activate(true);
        let _ = window.update(cx, |_, window, cx| {
            let quit = tcode_ui::tr!("quit.confirm");
            let answer = window.prompt(
                gpui::PromptLevel::Critical,
                &tcode_ui::tr!("quit.startup_failed"),
                Some(&detail),
                &[quit.as_ref()],
                cx,
            );
            cx.spawn(async move |_, cx| {
                let _ = answer.await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    });
    std::process::exit(1)
}

/// Every desktop window: the main one and the startup migration's.
fn window_options(cx: &App, throttle_inactive: bool) -> WindowOptions {
    let mut options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(px(tcode_ui::DEFAULT_WINDOW_WIDTH), px(800.)),
            cx,
        )),
        // Low enough that the window can actually be dragged into the
        // compact layout; the launch geometry above is unchanged.
        window_min_size: Some(size(px(360.), px(480.))),
        // macOS: seamless titlebar — transparent, with the traffic lights
        // nudged down to sit vertically centered in the 52px top strip.
        //
        // Windows: also client-decorated — a transparent titlebar hides
        // the system one — and `tcode_ui`'s window caption cluster draws
        // the minimize/maximize/close controls into whichever top strip
        // is rightmost (`crates/ui/src/window_caption.rs`).
        //
        // Linux: we draw no controls of our own there, so a transparent
        // titlebar would leave the window with no way to be closed from
        // the chrome. Keep the native system titlebar; our top strip
        // simply sits below it.
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: cfg!(any(target_os = "macos", target_os = "windows")),
            traffic_light_position: Some(point(px(12.), px(19.))),
        }),
        // macOS: the app owns titlebar dragging. AppKit's native
        // titlebar-region drag sits on top of whatever gpui draws in the
        // top strip — dragging the Preview URL bar moved the window
        // instead of selecting text. Every draggable strip already goes
        // through `window_drag_area` (`start_window_move`), so hand the
        // whole content view to the app and let controls own their input.
        app_owns_titlebar_drag: true,
        // Spell out that Windows is client-decorated. (This field is
        // advisory off Wayland; leaving it `None` elsewhere keeps Linux
        // on whatever its compositor/backend already chose.)
        window_decorations: cfg!(target_os = "windows").then_some(WindowDecorations::Client),
        // Persistent windows use the platform's system material:
        // macOS sidebar vibrancy, Windows Acrylic, or an opaque fallback.
        window_background: main_window_background(),
        ..Default::default()
    };
    // gpui reads the inactive-window frame cap only as it creates the window,
    // so turning it off in Settings takes a restart.
    if !throttle_inactive {
        options.inactive_frame_interval = None;
    }
    options
}

/// The local host, once this process owns the data dir.
enum LocalStart {
    Started(LocalKernel),
    /// An older build's data must move into the data dir first.
    Migrate(SessionStore),
}

/// Take ownership of the data dir and start the local host on it: at launch,
/// and again on Retry. The error is what the window shows instead of that
/// host.
fn start_local_host(login_shell_unread: Option<String>) -> Result<LocalStart, String> {
    let store = SessionStore::open_host(None).map_err(|error| {
        tcode_ui::tr!("hosts.local.default_settings", reason = error.to_string()).into_owned()
    })?;
    if store.needs_migration().map_err(|error| error.to_string())? {
        return Ok(LocalStart::Migrate(store));
    }
    LocalKernel::start(store, login_shell_unread)
        .map(LocalStart::Started)
        .map_err(|error| error.to_string())
}

struct LocalKernel {
    host: SpawnedHost,
    mux: HostMux,
    control_link: HostLink,
    _control_pump: smol::Task<()>,
}

impl LocalKernel {
    fn start(store: SessionStore, login_shell_unread: Option<String>) -> std::io::Result<Self> {
        let (host, mux) = start_local(store, login_shell_unread)?;
        let connection = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        );
        let control_link = HostLink::new(connection.to_host, connection.from_host);
        let pump_link = control_link.clone();
        let control_pump = smol::spawn(async move { pump_link.pump().await });
        Ok(Self {
            host,
            mux,
            control_link,
            _control_pump: control_pump,
        })
    }

    /// Make this kernel the process's host: it hosts remote devices when its
    /// settings say so, and stops when the application quits. Returns its
    /// settings, or why they could not be read, which the window shows
    /// instead of this host.
    fn own(&self, data_dir: &std::path::Path, cx: &mut App) -> Result<settings::Settings, String> {
        let local_settings = self.settings()?;
        // Hosting belongs to the process-owned local kernel; it carries no
        // current-attachment mode.
        cx.set_global(RemoteController::new(
            self.mux.clone(),
            data_dir.to_path_buf(),
            self.control_link.clone(),
            local_settings.clone(),
        ));
        if local_settings.remote_hosting_enabled {
            cx.update_global::<RemoteController, _>(|controller, _| {
                let name = controller.machine_name();
                if let Err(error) = controller.start_hosting(&local_settings.traverse, name) {
                    log::error!("remote hosting could not start: {error}");
                }
            });
        }
        // Process ownership, not the window's current attachment, grants
        // authority to stop the local kernel on application quit.
        let link = self.control_link.clone();
        let host = self.host.clone();
        cx.on_app_quit(move |_cx| {
            let link = link.clone();
            let stop = host.stop();
            async move {
                // `shutdown` only closes this client's mux connection;
                // the host loop ends when its own inbox closes.
                let _ = link.shutdown().await;
                stop.await;
            }
        })
        .detach();
        Ok(local_settings)
    }

    /// A window's link to the local kernel. The mux keeps the kernel alive
    /// independently, so this is an ordinary client connection like any other.
    fn transport(&self) -> Transport {
        let connection = self.mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::LOCAL,
        );
        // Nothing reports connection state for an in-process host; the closed
        // receiver simply ends the forwarder on its first poll.
        let (_, state) = async_channel::unbounded();
        Transport {
            originals: Some(std::sync::Arc::new(
                tcode_client::original::BackendOriginal {
                    backend: std::sync::Arc::new(self.host.clone()),
                    principal: tcode_protocol::Principal::Full,
                },
            )),
            to_host: connection.to_host.into(),
            from_host: connection.from_host,
            state,
            current_host: None,
        }
    }

    /// The host's settings, read once before the window opens. Nothing on
    /// screen can stop this wait yet, so it ends after five seconds.
    fn settings(&self) -> Result<settings::Settings, String> {
        let topic = tcode_protocol::Topic::Settings;
        self.control_link
            .subscribe(tcode_protocol::Subscription {
                known_index: None,
                topic: topic.clone(),
                after: None,
            })
            .map_err(|error| error.message)?;
        let events = self.control_link.events();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            match events.try_recv() {
                Ok(tcode_protocol::EventEnvelope {
                    event:
                        tcode_protocol::ServerEvent::SettingsSnapshot(settings)
                        | tcode_protocol::ServerEvent::SettingsReplaced(settings),
                    ..
                }) => break Ok(settings),
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(async_channel::TryRecvError::Empty) => {
                    break Err(tcode_ui::tr!("remote.failure.timeout").into_owned());
                }
                Err(async_channel::TryRecvError::Closed) => {
                    break Err(tcode_ui::tr!("remote.failure.host_closed").into_owned());
                }
            }
        };
        let _ = self.control_link.unsubscribe(tcode_protocol::Subscription {
            known_index: None,
            topic,
            after: None,
        });
        result
    }
}

gpui::actions!(tcode, [ShowMainWindow]);

/// The desktop's one window. Closing it ends the process except on macOS,
/// where the process and the local kernel it owns keep running and the window
/// opens again, where it was, when the app is reopened.
#[derive(Clone)]
struct MainWindow(Rc<MainWindowState>);

struct MainWindowState {
    native_client: Rc<NativeClientHost>,
    local: tcode_ui::attachment::LocalTransport,
    throttle_inactive: bool,
    shown: RefCell<Shown>,
    /// The frame the window had as the user closed it.
    closed_bounds: std::cell::Cell<Option<WindowBounds>>,
}

enum Shown {
    Open {
        window: AnyWindowHandle,
        shell: Entity<AppShell>,
    },
    /// Where it was when it closed; nothing before it first opens.
    Closed(Option<ShellPlace>),
}

impl gpui::Global for MainWindow {}

impl MainWindow {
    /// Become the process's main window, before it first opens.
    fn install(
        native_client: Rc<NativeClientHost>,
        local: tcode_ui::attachment::LocalTransport,
        throttle_inactive: bool,
        cx: &mut App,
    ) -> Self {
        let this = Self(Rc::new(MainWindowState {
            native_client,
            local,
            throttle_inactive,
            shown: RefCell::new(Shown::Closed(None)),
            closed_bounds: Default::default(),
        }));
        cx.on_window_closed({
            let this = this.clone();
            move |cx, closed| {
                if this
                    .window()
                    .is_some_and(|window| window.window_id() == closed)
                {
                    this.release(cx);
                }
            }
        })
        .detach();
        cx.set_global(this.clone());
        this
    }

    fn open(
        &self,
        setup: ShellSetup,
        bounds: Option<WindowBounds>,
        cx: &mut App,
    ) -> Entity<AppShell> {
        let state = &self.0;
        let mut options = window_options(cx, state.throttle_inactive);
        if bounds.is_some() {
            options.window_bounds = bounds;
        }
        let (window, shell) = tcode_ui::run_shell(
            cx,
            state.native_client.clone(),
            ShellOptions {
                title: tcode_ui::tr!("app.name").into(),
                activate: true,
                // A desktop window is never suspended by the OS.
                lifecycle: None,
                window: options,
                setup: ShellSetup {
                    local: Some(state.local.clone()),
                    ..setup
                },
                ..ShellOptions::default()
            },
        );
        let this = self.clone();
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |window, cx| {
                if cfg!(target_os = "macos") {
                    this.0.closed_bounds.set(Some(window.window_bounds()));
                    true
                } else {
                    // Closing the window quits here, so it asks what Quit
                    // asks before stopping working threads.
                    window.dispatch_action(Box::new(Quit), cx);
                    false
                }
            });
        });
        *state.shown.borrow_mut() = Shown::Open {
            window,
            shell: shell.clone(),
        };
        shell
    }

    fn window(&self) -> Option<AnyWindowHandle> {
        match &*self.0.shown.borrow() {
            Shown::Open { window, .. } => Some(*window),
            Shown::Closed(_) => None,
        }
    }

    /// The open window's shell.
    fn shell(cx: &App) -> Option<Entity<AppShell>> {
        match &*cx.try_global::<Self>()?.0.shown.borrow() {
            Shown::Open { shell, .. } => Some(shell.clone()),
            Shown::Closed(_) => None,
        }
    }

    /// The window has gone: keep where it was and close its links to the
    /// hosts it showed. They, the local kernel included, keep running.
    fn release(&self, cx: &mut App) {
        let Shown::Open { shell, .. } = self.0.shown.replace(Shown::Closed(None)) else {
            return;
        };
        let place = shell.read(cx).current_place(cx);
        shell.update(cx, |shell, cx| shell.close_links(cx));
        *self.0.shown.borrow_mut() = Shown::Closed(Some(place));
    }

    /// Bring the main window to the front, opening it where it was if it
    /// has closed. Nothing opens before launch has installed it.
    fn show(cx: &mut App) {
        let Some(this) = cx.try_global::<Self>().cloned() else {
            return;
        };
        if let Some(window) = this.window() {
            if window
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                cx.activate(true);
                return;
            }
            this.release(cx);
        }
        let Shown::Closed(place) = this.0.shown.replace(Shown::Closed(None)) else {
            return;
        };
        this.open(
            ShellSetup {
                reopen: place,
                ..ShellSetup::default()
            },
            this.0.closed_bounds.take(),
            cx,
        );
    }
}

fn main() {
    // ssh's ProxyCommand: a short-lived pipe that must not start anything
    // else of the app, nor write to stderr beyond its one refusal line.
    if std::env::args().nth(1).as_deref() == Some("ssh-proxy") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(editor::proxy::run(&args));
    }
    env_logger::init();
    tcode_services::process::raise_open_file_limit();

    if std::env::args().any(|arg| arg == "--cu-smoke") {
        use std::io::Write as _;
        print!("{}", computer_use_mcp::smoke());
        let _ = std::io::stdout().flush();
        return;
    }

    // A Finder/Dock launch inherits launchd's minimal PATH, under which none of
    // the provider CLIs resolve — import the login shell's environment first,
    // before anything (probes, sessions, the terminal) reads PATH. Must stay
    // ahead of any thread spawn: it writes the process environment. Why it
    // could not be read goes to the local host, which says so.
    let login_shell_unread = shell_env::import_login_shell_environment().err();

    settings::apply_locale(None);

    // Hidden debug/dev flag: open the most recently updated session on launch.
    let open_latest = std::env::args().any(|arg| arg == "--open-latest");
    let args: Vec<String> = std::env::args().collect();
    let data_dir =
        tcode_services::store::data_dir().unwrap_or_else(|error| exit_with_startup_failure(error));
    // The UI never resolves a data directory of its own: whatever client-owned
    // files it needs (the WebView2 profile) live under this one.
    tcode_ui::set_client_data_dir(data_dir.clone());
    // Launching an editor is process work this crate already owns the helpers
    // for, so it is injected rather than linked into the UI.
    let native_client = Rc::new(
        NativeClientHost::new(data_dir.clone(), machine_name())
            .with_editor_opener(|path| {
                tcode_services::desktop::open_in_zed(path).map_err(|error| error.to_string())
            })
            .with_machine_editor(editor::ZedRemote::new(data_dir.clone())),
    );

    if let Some(index) = args.iter().position(|arg| arg == "--pair") {
        // Pairing writes hosts.json and device.json, at which a later move of
        // an older data dir would stop.
        match SessionStore::open_host(None).and_then(|store| {
            store
                .pending_relocation()
                .map(|previous| previous.map(std::path::Path::to_path_buf))
        }) {
            Ok(None) => {}
            Ok(Some(previous)) => {
                eprintln!(
                    "tcode: {} has not been moved into {} yet; start Tcode once first, then pair",
                    previous.display(),
                    data_dir.display()
                );
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("tcode: {error}");
                std::process::exit(1);
            }
        }
        match pair_command(&args[index + 1..], &native_client) {
            Ok(host_id) => println!("{host_id}"),
            Err(error) => {
                eprintln!("tcode: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Read once the data dir is in place, which may be after the migration.
    let connect = arg_value(&args, "--connect");
    let initial_target = {
        let native_client = native_client.clone();
        let data_dir = data_dir.clone();
        move || match connect {
            Some(host_id) => {
                let saved = native_client.load_hosts().unwrap_or_else(|error| {
                    eprintln!("tcode: could not read {}/{error}", data_dir.display());
                    std::process::exit(1);
                });
                let Some(host) = saved.into_iter().find(|host| host.host_id == host_id) else {
                    eprintln!(
                        "tcode: no added machine with id {host_id:?} in {}/hosts.json; add it first (the sidebar's Machines row, or tcode --pair <tcode://pair?...>)",
                        data_dir.display()
                    );
                    std::process::exit(1);
                };
                AttachmentTarget::Remote(Box::new(host))
            }
            None => AttachmentTarget::Local,
        }
    };
    // The local kernel is process composition, not a property of the window's
    // current attachment: it stays alive when the window starts on, or later
    // switches to, a remote host. An older build's threads move into tcode.db
    // before it starts, behind a window of their own; with nothing to
    // migrate, it starts before the event loop. A kernel that does not start
    // leaves the window without a local host, and says why there.
    let started = start_local_host(login_shell_unread.clone());

    let application = gpui_platform::application();
    // AppKit asks only when the app has no window on screen: a Dock click,
    // or a launch of the running app from Finder.
    application.on_reopen(MainWindow::show);
    application.with_assets(assets::Assets).run(move |cx| {
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let application_fonts: Vec<Cow<'static, [u8]>> = vec![
            Cow::Borrowed(assets::DM_SANS),
            Cow::Borrowed(assets::LILEX_REGULAR),
            Cow::Borrowed(assets::LILEX_BOLD),
            Cow::Borrowed(assets::LILEX_ITALIC),
            Cow::Borrowed(assets::LILEX_BOLD_ITALIC),
        ];
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let application_fonts: Vec<Cow<'static, [u8]>> = vec![Cow::Borrowed(assets::DM_SANS)];
        // Translucent canvas colors composite over macOS vibrancy and Windows
        // Acrylic. Opaque windows flatten them to the solid base; macOS
        // fullscreen applies the same fallback in material::opaque_canvas.
        let theme_json: Cow<'static, str> = if translucent_canvas_enabled() {
            Cow::Borrowed(tcode_ui::THEME_JSON)
        } else {
            Cow::Owned(tcode_ui::flattened_theme_json())
        };

        let client_options = ShellOptions {
            fonts: application_fonts,
            theme_json,
            system_locale: None,
            ..ShellOptions::default()
        };
        tcode_ui::init_client(cx, native_client.as_ref(), &client_options);
        #[cfg(target_os = "macos")]
        {
            cx.set_menus([
                gpui::Menu::new(tcode_ui::tr!("app.name")).items([gpui::MenuItem::action(
                    tcode_ui::tr!("quit.menu_item"),
                    Quit,
                )]),
                gpui::Menu::new(tcode_ui::tr!("app.window_menu")).items([gpui::MenuItem::action(
                    tcode_ui::tr!("app.name"),
                    ShowMainWindow,
                )]),
            ]);
            cx.bind_keys([gpui::KeyBinding::new("cmd-0", ShowMainWindow, None)]);
        }
        cx.on_action(|_: &ShowMainWindow, cx| MainWindow::show(cx));
        // Preferences that cannot be read leave the migration dialog in the
        // system appearance; the window after it says why.
        let appearance = native_client
            .load_preferences()
            .ok()
            .and_then(|preferences| preferences.appearance);
        cx.on_action::<Quit>(move |action, cx| {
            if migration::quit(cx) {
                return;
            }
            match MainWindow::shell(cx) {
                Some(shell) => handle_quit(action, &shell, cx),
                None => cx.quit(),
            }
        });

        let migrated_login_shell = login_shell_unread.clone();
        let launch = move |cx: &mut App, started: Result<LocalKernel, String>| {
            let initial_target = initial_target();
            let owned = started.and_then(|kernel| {
                let settings = kernel.own(&data_dir, cx)?;
                Ok((Rc::new(kernel), settings))
            });
            let (kernel, failure, local_settings) = match owned {
                Ok((kernel, settings)) => (Some(kernel), None, settings),
                // A window without a local host has read none of its settings.
                Err(reason) => (None, Some(reason), settings::Settings::default()),
            };
            let owned = Rc::new(RefCell::new(kernel.clone()));
            let local: tcode_ui::attachment::LocalTransport = Rc::new(move |cx| {
                if let Some(kernel) = owned.borrow().as_ref() {
                    return Ok(kernel.transport());
                }
                match start_local_host(login_shell_unread.clone())? {
                    LocalStart::Started(kernel) => {
                        kernel.own(&data_dir, cx)?;
                        let transport = kernel.transport();
                        *owned.borrow_mut() = Some(Rc::new(kernel));
                        Ok(transport)
                    }
                    LocalStart::Migrate(_) => {
                        Err(tcode_ui::tr!("hosts.local.needs_migration").into_owned())
                    }
                }
            });

            let main_window = MainWindow::install(
                native_client.clone(),
                local,
                !local_settings.inactive_frame_throttle_disabled,
                cx,
            );
            let shell = main_window.open(
                ShellSetup {
                    local_failure: failure,
                    initial: Some(initial_target),
                    local_settings: kernel.is_some().then_some(local_settings),
                    ..ShellSetup::default()
                },
                None,
                cx,
            );
            // Restart continuity: if this launch follows a permission-grant
            // relaunch, reopen the recorded session and Settings page. Only
            // meaningful for a host in this process: the marker lives in this
            // machine's data dir, and a remote host's marker is its own.
            if let Some(kernel) = kernel
                && let Some(store) = shell.read(cx).local_store()
                && let Ok(CommandResponse::PendingRelaunchSection {
                    section: Some(section),
                    session_id,
                }) = kernel
                    .control_link
                    .command_blocking(Command::ApplyPendingRelaunch)
            {
                if let Some(id) = session_id {
                    store.update(cx, |store, cx| store.show_thread(id, cx));
                }
                let window_state = shell.read(cx).window_state();
                window_state.update(cx, |state, cx| {
                    state.pending_settings_section = Some(section);
                    state.open_settings_on(tcode_ui::store::HostKey::Local, cx);
                });
            }

            cx.spawn(async move |cx| {
                if open_latest {
                    let Some(link) = cx.update(|cx| shell.read(cx).link(cx)) else {
                        return;
                    };
                    if let Ok(CommandResponse::SessionId(Some(id))) =
                        link.command(Command::OpenLatestSession).await
                        && let Some(store) = cx.update(|cx| shell.read(cx).store(cx))
                    {
                        store.update(cx, |store, cx| store.show_thread(id, cx));
                    }
                }
            })
            .detach();
        };

        match started {
            Ok(LocalStart::Started(kernel)) => launch(cx, Ok(kernel)),
            Ok(LocalStart::Migrate(store)) => {
                let options = window_options(cx, true);
                let kernel_store = store.clone();
                migration::run(cx, store, options, appearance, move |cx| {
                    launch(
                        cx,
                        Ok(LocalKernel::start(kernel_store, migrated_login_shell)?),
                    );
                    Ok(())
                });
            }
            Err(reason) => {
                log::error!("the local host did not start: {reason}");
                launch(cx, Err(reason))
            }
        }
    });
}
