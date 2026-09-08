// Windows: run as a GUI app so launching tcode does not open a console window.
// Debug builds keep the console so `RUST_LOG` output stays visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{borrow::Cow, rc::Rc, time::Duration};

use gpui::{
    App, BorrowAppContext as _, Entity, ParentElement as _, Styled as _, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, point, px, size,
};
use tcode_client::{HostLink, host::ClientHost as _, host::Transport};
use tcode_protocol::{Command, CommandResponse};
use tcode_remote::{HostMux, NativeClientHost};
use tcode_runtime::pipe::{HostServices, SpawnedHost, spawn_host};
use tcode_services::{shell_env, store::SessionStore};
use tcode_ui::remote::{AttachmentTarget, RemoteController, machine_name};
use tcode_ui::{
    AppShell, Quit, ShellOptions, ShellSetup, WindowSeam, WindowState, theme::ActiveTheme as _,
};
use tcode_ui::{assets, settings};

use tcode_ui::overlay::{DialogActions, OverlayExt as _};
use tcode_ui::widgets::button::{Button, ButtonVariants as _};

#[cfg(not(target_os = "linux"))]
mod preview_smoke;

/// macOS vibrancy can be disabled with `TCODE_NO_VIBRANCY=1` as a diagnostic
/// escape hatch (opaque window + flattened palette).
fn vibrancy_enabled() -> bool {
    cfg!(target_os = "macos") && !std::env::var("TCODE_NO_VIBRANCY").is_ok_and(|v| v == "1")
}

fn translucent_canvas_enabled() -> bool {
    cfg!(target_os = "windows") || vibrancy_enabled()
}

fn main_window_background() -> WindowBackgroundAppearance {
    if cfg!(target_os = "windows") || vibrancy_enabled() {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Opaque
    }
}

const QUIT_PROMPT_TIMEOUT: Duration = Duration::from_secs(15);

fn finish_quit_prompt(window_state: &Entity<WindowState>, epoch: u64, cx: &mut App) -> bool {
    window_state.update(cx, |state, _| {
        if !state.quit_prompt_open || state.quit_prompt_epoch != epoch {
            return false;
        }
        state.quit_prompt_epoch = state.quit_prompt_epoch.wrapping_add(1);
        state.quit_prompt_open = false;
        true
    })
}

fn handle_quit(_: &Quit, shell: &Entity<AppShell>, cx: &mut App) {
    let count = shell
        .read(cx)
        .store()
        .map(|store| store.read(cx).working_sessions_count())
        .unwrap_or(0);
    if count == 0 {
        cx.quit();
        return;
    }

    let Some(window_handle) = cx
        .active_window()
        .or_else(|| cx.windows().into_iter().next())
    else {
        cx.quit();
        return;
    };

    let window_state = shell.read(cx).window_state();
    let epoch = window_state.update(cx, |state, _| {
        if state.quit_prompt_open {
            return None;
        }
        state.quit_prompt_epoch = state.quit_prompt_epoch.wrapping_add(1);
        state.quit_prompt_open = true;
        Some(state.quit_prompt_epoch)
    });
    let Some(epoch) = epoch else {
        return;
    };

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
                    .description(tcode_ui::tr!("quit.description", count = count))
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
                                        finish_quit_prompt(&quit_state, epoch, cx);
                                        window.close_dialog(cx);
                                        cx.quit();
                                    }),
                            )
                            .child(
                                Button::new("cancel-quit")
                                    .label(tcode_ui::tr!("settings.cancel"))
                                    .primary()
                                    .on_click(move |_, window, cx| {
                                        finish_quit_prompt(&cancel_state, epoch, cx);
                                        window.close_dialog(cx);
                                    }),
                            ),
                    )
                    .on_ok(move |_, _, cx| {
                        finish_quit_prompt(&enter_state, epoch, cx);
                        true
                    })
                    .on_cancel(move |_, _, cx| {
                        finish_quit_prompt(&escape_state, epoch, cx);
                        true
                    })
            });
        })
        .is_err()
    {
        finish_quit_prompt(&window_state, epoch, cx);
        cx.quit();
        return;
    }

    let timeout_state = window_state.clone();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(QUIT_PROMPT_TIMEOUT).await;
        cx.update(|cx| {
            if finish_quit_prompt(&timeout_state, epoch, cx) {
                let _ = window_handle.update(cx, |_, window, cx| window.close_dialog(cx));
            }
        });
    })
    .detach();
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

/// Hidden `tcode --pair <addr> <port> <code>`: pair with a host over HTTP,
/// record it in `hosts.json`, print its id and exit. The desktop pairing UI
/// does the same thing; this is the headless path used by the remote e2e run.
fn pair_command(args: &[String], client_host: &NativeClientHost) -> Result<String, String> {
    let [addr, port, code] = args else {
        return Err("usage: tcode --pair <addr> <port> <code>".into());
    };
    let port: u16 = port
        .parse()
        .map_err(|error| format!("invalid port: {error}"))?;
    let host = smol::block_on(client_host.pair(tcode_client::host::PairRequest {
        origin: tcode_client::pairing::lan_origin(addr, port),
        code: code.clone(),
    }))?;
    let mut hosts = client_host.load_hosts();
    hosts.retain(|existing| existing.host_id != host.host_id);
    let host_id = host.host_id.clone();
    hosts.push(host);
    client_host.save_hosts(&hosts);
    Ok(host_id)
}

/// Start the in-process host and put the mux in front of it. This window is
/// then one ordinary client among every link attached to that mux.
fn start_local(store: SessionStore) -> (SpawnedHost, HostMux) {
    let mut host_services = HostServices {
        background_startup_probes: true,
        ai_title_generation: true,
        ..HostServices::default()
    };
    match mcp_host::Host::bind() {
        Ok(mut mcp_host) => {
            host_services.preview = Some(preview_mcp::start(&mut mcp_host));
            host_services.orchestrate = Some(orchestrate_mcp::start(&mut mcp_host));
            host_services.computer_use = Some(computer_use_mcp::start(&mut mcp_host));
            if let Err(error) = mcp_host.start() {
                log::warn!("MCP host failed to start: {error}");
                host_services.preview = None;
                host_services.orchestrate = None;
                host_services.computer_use = None;
            }
        }
        Err(error) => log::warn!("MCP host failed to bind: {error}"),
    }
    let host = spawn_host(store, host_services).expect("failed to start tcode host thread");
    let mux = HostMux::new(host.to_host.clone(), host.from_host.clone());
    (host, mux)
}

struct LocalKernel {
    host: SpawnedHost,
    mux: HostMux,
    control_link: HostLink,
    _control_pump: smol::Task<()>,
}

impl LocalKernel {
    fn start(store: SessionStore) -> Self {
        let (host, mux) = start_local(store);
        let connection = mux.attach();
        let control_link = HostLink::new(connection.to_host, connection.from_host);
        let pump_link = control_link.clone();
        let control_pump = smol::spawn(async move { pump_link.pump().await });
        Self {
            host,
            mux,
            control_link,
            _control_pump: control_pump,
        }
    }

    /// A window's link to the local kernel. The mux keeps the kernel alive
    /// independently, so this is an ordinary client connection like any other.
    fn transport(&self) -> Transport {
        let connection = self.mux.attach();
        // Nothing reports connection state for an in-process host; the closed
        // receiver simply ends the forwarder on its first poll.
        let (_, state) = async_channel::unbounded();
        Transport {
            to_host: connection.to_host.into(),
            from_host: connection.from_host,
            state,
        }
    }

    fn settings(&self) -> settings::Settings {
        let topic = tcode_protocol::Topic::Settings;
        if let Err(error) = self.control_link.subscribe(tcode_protocol::Subscription {
            topic: topic.clone(),
            after: None,
        }) {
            log::error!("could not request local settings: {}", error.message);
            return settings::Settings::default();
        }
        let events = self.control_link.events();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            match events.try_recv() {
                Ok(tcode_protocol::EventEnvelope {
                    event:
                        tcode_protocol::ServerEvent::SettingsSnapshot(settings)
                        | tcode_protocol::ServerEvent::SettingsReplaced(settings),
                    ..
                }) => break settings,
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break settings::Settings::default(),
            }
        };
        let _ = self
            .control_link
            .unsubscribe(tcode_protocol::Subscription { topic, after: None });
        result
    }
}

fn main() {
    env_logger::init();

    if std::env::args().any(|arg| arg == "--cu-smoke") {
        use std::io::Write as _;
        print!("{}", computer_use_mcp::smoke());
        let _ = std::io::stdout().flush();
        return;
    }

    let preview_smoke = std::env::args().any(|arg| arg == "--preview-smoke");
    #[cfg(target_os = "linux")]
    if preview_smoke {
        use std::io::Write as _;
        eprintln!("preview-smoke: SKIP (no native webview)");
        let _ = std::io::stderr().flush();
        return;
    }
    #[cfg(not(target_os = "linux"))]
    let preview_smoke_watchdog = preview_smoke.then(preview_smoke::Watchdog::start);

    // A Finder/Dock launch inherits launchd's minimal PATH, under which none of
    // the provider CLIs resolve — import the login shell's environment first,
    // before anything (probes, sessions, the terminal) reads PATH. Must stay
    // ahead of any thread spawn: it writes the process environment.
    shell_env::import_login_shell_environment();

    settings::apply_locale(None);

    // Hidden debug/dev flag: open the most recently updated session on launch.
    let open_latest = std::env::args().any(|arg| arg == "--open-latest");
    let args: Vec<String> = std::env::args().collect();
    // The local kernel is process composition, not a property of the window's
    // current attachment. Open its store unconditionally and keep it alive even
    // when the window starts on, or later switches to, a remote host.
    let store = SessionStore::open_default().expect("failed to open tcode data directory");
    let data_dir = store.root().clone();
    // The UI never resolves a data directory of its own: whatever client-owned
    // files it needs (the WebView2 profile) live under this one.
    tcode_ui::set_client_data_dir(data_dir.clone());
    // Launching an editor is process work this crate already owns the helpers
    // for, so it is injected rather than linked into the UI.
    let native_client = Rc::new(
        NativeClientHost::new(data_dir.clone(), machine_name()).with_editor_opener(|path| {
            tcode_services::desktop::open_in_zed(path).map_err(|error| error.to_string())
        }),
    );

    if let Some(index) = args.iter().position(|arg| arg == "--pair") {
        match pair_command(&args[index + 1..], &native_client) {
            Ok(host_id) => println!("{host_id}"),
            Err(error) => {
                eprintln!("tcode: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    let initial_target = match arg_value(&args, "--connect") {
        Some(host_id) => {
            let Some(host) = native_client
                .load_hosts()
                .into_iter()
                .find(|host| host.host_id == host_id)
            else {
                eprintln!(
                    "tcode: no added machine with id {host_id:?} in {}/hosts.json; add it first (the sidebar's Machines row, or tcode --pair <addr> <port> <code>)",
                    data_dir.display()
                );
                std::process::exit(1);
            };
            AttachmentTarget::Remote(host)
        }
        None => AttachmentTarget::Local,
    };
    // Kernel ownership is process composition, not a property of whichever
    // host the window currently views.
    let kernel = Rc::new(LocalKernel::start(store));
    let local_settings = kernel.settings();

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
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

            // Hosting belongs to the process-owned local kernel; it carries no
            // current-attachment mode.
            cx.set_global(RemoteController::new(
                kernel.mux.clone(),
                data_dir.clone(),
                kernel.control_link.clone(),
                local_settings.clone(),
            ));
            if local_settings.remote_hosting_enabled {
                let port = local_settings
                    .remote_port
                    .unwrap_or(tcode_ui::remote::DEFAULT_REMOTE_PORT);
                let name = local_settings
                    .remote_host_name
                    .clone()
                    .unwrap_or_else(machine_name);
                cx.update_global::<RemoteController, _>(|controller, _| {
                    if let Err(error) = controller.start_hosting(port, name) {
                        log::error!("remote hosting could not start: {error}");
                    }
                });
            }
            #[cfg(target_os = "macos")]
            cx.set_menus([gpui::Menu::new(tcode_ui::tr!("app.name")).items([
                gpui::MenuItem::action(tcode_ui::tr!("quit.menu_item"), Quit),
            ])]);
            // Process ownership, not the window's current attachment, grants
            // authority to stop the local kernel on application quit.
            let quit_subscription = cx.on_app_quit({
                let link = kernel.control_link.clone();
                let to_host = kernel.host.to_host.clone();
                let stopped = kernel.host.stopped.clone();
                move |_cx| {
                    let link = link.clone();
                    let to_host = to_host.clone();
                    let stopped = stopped.clone();
                    async move {
                        let _ = link.shutdown().await;
                        // `shutdown` only closes this client's mux connection;
                        // the host loop ends when its own inbox closes.
                        to_host.close();
                        let _ = stopped.recv().await;
                    }
                }
            });
            quit_subscription.detach();

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(800.)), cx)),
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
                window_decorations: cfg!(target_os = "windows")
                    .then_some(WindowDecorations::Client),
                // Persistent windows use the platform's system material:
                // macOS blur, Windows Acrylic, or an opaque fallback.
                window_background: main_window_background(),
                // Throttle background redraws (spinners, streaming output) to
                // ~2 FPS while the window is inactive; gpui lifts the cap the
                // moment the window is active or receiving high-rate input.
                // This setting is captured at window creation and requires a restart.
                inactive_frame_interval: (!local_settings.inactive_frame_throttle_disabled)
                    .then(|| Duration::from_millis(500)),
                ..Default::default()
            };

            let local_kernel = kernel.clone();
            let (window, shell) = tcode_ui::run_shell(
                cx,
                native_client.clone(),
                // A desktop window has no system occlusion of its own.
                WindowSeam::flush(),
                ShellOptions {
                    window: window_options,
                    title: tcode_ui::tr!("app.name").into(),
                    fonts: application_fonts,
                    theme_json,
                    activate: true,
                    system_locale: None,
                    setup: ShellSetup {
                        client_host: Some(native_client.clone()),
                        local: Some(Rc::new(move || local_kernel.transport())),
                        initial: Some(initial_target.clone()),
                        initial_pairing_error: None,
                        // Only here: bootstrap applies locale and theme from the
                        // host's own settings before the first frame.
                        seed_blocking: true,
                        restore_navigation: false,
                    },
                },
            );

            cx.on_action::<Quit>({
                let shell = shell.clone();
                move |action, cx| handle_quit(action, &shell, cx)
            });
            // Restart continuity: if this launch follows a permission-grant
            // relaunch, reopen the recorded session and Settings page. Only
            // meaningful for a host in this process: the marker lives in this
            // machine's data dir, and a remote host's marker is its own.
            if shell
                .read(cx)
                .store()
                .is_some_and(|store| !store.read(cx).is_remote())
                && let Ok(CommandResponse::PendingRelaunchSection {
                    section: Some(section),
                    session_id,
                }) = kernel
                    .control_link
                    .command_blocking(Command::ApplyPendingRelaunch)
            {
                if let Some(id) = session_id
                    && let Some(store) = shell.read(cx).store()
                {
                    store.update(cx, |store, _| store.select_session(id));
                }
                let window_state = shell.read(cx).window_state();
                window_state.update(cx, |state, cx| {
                    state.pending_settings_section = Some(section);
                    state.open_settings(cx);
                });
            }

            cx.spawn(async move |cx| {
                #[cfg(not(target_os = "linux"))]
                if let Some(watchdog) = preview_smoke_watchdog {
                    preview_smoke::run(watchdog, shell, window, cx).await;
                    return;
                }
                let _ = window;

                if open_latest {
                    let Some(link) = cx.update(|cx| shell.read(cx).link()) else {
                        return;
                    };
                    if let Ok(CommandResponse::SessionId(Some(id))) =
                        link.command(Command::OpenLatestSession).await
                        && let Some(store) = cx.update(|cx| shell.read(cx).store())
                    {
                        store.update(cx, |store, _| store.select_session(id));
                    }
                    for _ in 0..100 {
                        if cx.update(|cx| {
                            shell
                                .read(cx)
                                .store()
                                .is_some_and(|store| store.read(cx).active_session_id().is_some())
                        }) {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(10))
                            .await;
                    }
                    if let Some(store) = cx.update(|cx| shell.read(cx).store()) {
                        store.update(cx, |store, _cx| {
                            store.sync_active_conversation_ui();
                        });
                    }
                }
            })
            .detach();
        });
}
