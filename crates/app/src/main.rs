// Windows: run as a GUI app so launching tcode does not open a console window.
// Debug builds keep the console so `RUST_LOG` output stays visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::{borrow::Cow, time::Duration};

use gpui::{
    App, AppContext as _, Entity, KeyBinding, ParentElement as _, Styled as _, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, point, px, size,
};
use tcode_protocol::{Command, CommandResponse};
use tcode_runtime::pipe::{HostServices, spawn_host};
use tcode_services::{shell_env, store::SessionStore};
use tcode_ui::{AppShell, Quit, TogglePalette, WindowState, store::WorkspaceStore};
use tcode_ui::{assets, settings};

use gpui_component::{
    ActiveTheme as _, Root, Theme, ThemeMode as ComponentThemeMode, ThemeRegistry, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
};

const TCODE_THEME: &str = include_str!("../../../themes/tcode.json");

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

/// With an opaque window the translucent canvas colors would composite against
/// black; flatten them to their solid RGB. Keep the literals in sync with
/// themes/tcode.json (checked by debug builds via debug_assert).
fn flatten_canvas_for_opaque_window(theme_json: &str) -> String {
    let flattened = theme_json
        .replace("#F2F4F7C7", "#F2F4F7")
        .replace("#15171CC7", "#15171C");
    debug_assert_ne!(flattened, theme_json, "canvas colors moved; update flatten");
    flattened
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

fn handle_quit(
    _: &Quit,
    workspace_store: &Entity<WorkspaceStore>,
    window_state: &Entity<WindowState>,
    cx: &mut App,
) {
    let count = workspace_store.read(cx).working_sessions_count();
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
                    .title(tcode_i18n::tr!("quit.title"))
                    .description(tcode_i18n::tr!("quit.description", count = count))
                    // The stock alert maps Enter to OK. A custom footer keeps
                    // both Enter and Escape safe while retaining the alert's
                    // normal visual style and explicit danger action.
                    .footer(
                        DialogFooter::new()
                            .child(
                                Button::new("quit-working-sessions")
                                    .label(tcode_i18n::tr!("quit.confirm"))
                                    .danger()
                                    .on_click(move |_, window, cx| {
                                        finish_quit_prompt(&quit_state, epoch, cx);
                                        window.close_dialog(cx);
                                        cx.quit();
                                    }),
                            )
                            .child(
                                Button::new("cancel-quit")
                                    .label(tcode_i18n::tr!("settings.cancel"))
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
        finish_quit_prompt(window_state, epoch, cx);
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

fn main() {
    env_logger::init();

    // A Finder/Dock launch inherits launchd's minimal PATH, under which none of
    // the provider CLIs resolve — import the login shell's environment first,
    // before anything (probes, sessions, the terminal) reads PATH. Must stay
    // ahead of any thread spawn: it writes the process environment.
    shell_env::import_login_shell_environment();

    settings::apply_locale(None);

    // Hidden debug/dev flag: open the most recently updated session on launch.
    let open_latest = std::env::args().any(|arg| arg == "--open-latest");
    let store = SessionStore::open_default().expect("failed to open tcode data directory");
    let mut host_services = HostServices::default();
    match mcp_host::Host::bind() {
        Ok(mut mcp_host) => {
            host_services.preview = Some(preview_mcp::start(&mut mcp_host));
            host_services.orchestrate = Some(orchestrate_mcp::start(&mut mcp_host));
            host_services.computer_use = Some(computer_use_mcp::start(&mut mcp_host));
            if let Err(error) = mcp_host.start() {
                log::warn!("MCP host failed to start: {error}");
                host_services = HostServices::default();
            }
        }
        Err(error) => log::warn!("MCP host failed to bind: {error}"),
    }
    let host = spawn_host(store, host_services).expect("failed to start tcode host thread");

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            tcode_ui::markdown::init(cx);

            // Global ⌘K / Ctrl-K opens/closes the command palette (handled by
            // AppShell). `secondary` is gpui's platform modifier: command on
            // macOS, control on Windows/Linux — where a literal `cmd-` binding
            // would mean the Super/Win key, which the OS intercepts.
            cx.bind_keys([KeyBinding::new("secondary-k", TogglePalette, None)]);
            #[cfg(target_os = "macos")]
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);

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
            cx.text_system()
                .add_fonts(application_fonts)
                .expect("failed to register bundled application fonts");
            // The theme's canvas color stays translucent over macOS vibrancy
            // and Windows Acrylic (docs/visual-redesign.md). Opaque windows flatten
            // it onto each mode's solid base first. (macOS fullscreen flattens
            // at paint time instead: material::opaque_canvas.)
            let theme_json: Cow<'_, str> = if translucent_canvas_enabled() {
                Cow::Borrowed(TCODE_THEME)
            } else {
                Cow::Owned(flatten_canvas_for_opaque_window(TCODE_THEME))
            };
            ThemeRegistry::global_mut(cx)
                .load_themes_from_str(&theme_json)
                .expect("embedded themes/tcode.json must be valid");
            let light = ThemeRegistry::global(cx).themes()["tcode Light"].clone();
            let dark = ThemeRegistry::global(cx).themes()["tcode Dark"].clone();
            Theme::global_mut(cx).apply_config(&light);
            Theme::global_mut(cx).apply_config(&dark);

            let workspace_store =
                cx.new(|cx| tcode_ui::store::WorkspaceStore::new(host.clone(), cx));
            let initial_settings = workspace_store.read(cx).settings();
            let sidebar_collapsed = initial_settings.sidebar_collapsed;
            let window_state = cx.new(|_| WindowState::new(sidebar_collapsed));
            cx.on_action::<Quit>({
                let workspace_store = workspace_store.clone();
                let window_state = window_state.clone();
                move |action, cx| handle_quit(action, &workspace_store, &window_state, cx)
            });
            // Restart continuity: if this launch follows a permission-grant
            // relaunch, reopen the recorded session and Settings page. Runs
            // synchronously before the window (and settings page) is built, so
            // the page mounts already on the recorded section. No-op otherwise.
            if let Ok(CommandResponse::PendingRelaunchSection(Some(section))) =
                host.command_blocking(Command::ApplyPendingRelaunch)
            {
                window_state.update(cx, |state, cx| {
                    state.pending_settings_section = Some(section);
                    state.open_settings(cx);
                });
            }
            settings::apply_locale(initial_settings.language.as_deref());
            #[cfg(target_os = "macos")]
            cx.set_menus([gpui::Menu::new("tcode").items([gpui::MenuItem::action(
                tcode_i18n::tr!("quit.menu_item"),
                Quit,
            )])]);
            match initial_settings.theme_mode {
                settings::ThemeMode::Light => Theme::change(ComponentThemeMode::Light, None, cx),
                settings::ThemeMode::Dark => Theme::change(ComponentThemeMode::Dark, None, cx),
                settings::ThemeMode::System => Theme::sync_system_appearance(None, cx),
            }
            log::info!(
                "applied embedded themes/tcode.json mode={} theme={}",
                cx.theme().mode.name(),
                cx.theme().theme_name()
            );
            let quit_subscription = cx.on_app_quit({
                let host = host.clone();
                move |_cx| {
                    let host = host.clone();
                    async move {
                        let _ = host.shutdown().await;
                    }
                }
            });
            quit_subscription.detach();

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(800.)), cx)),
                window_min_size: Some(size(px(900.), px(600.))),
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
                ..Default::default()
            };

            cx.spawn(async move |cx| {
                let window = cx
                    .open_window(window_options, {
                        let theme_store = workspace_store.clone();
                        let workspace_store = workspace_store.clone();
                        let window_state = window_state.clone();
                        move |window, cx| {
                            match theme_store.read(cx).settings().theme_mode {
                                settings::ThemeMode::Light => {
                                    Theme::change(ComponentThemeMode::Light, Some(window), cx)
                                }
                                settings::ThemeMode::Dark => {
                                    Theme::change(ComponentThemeMode::Dark, Some(window), cx)
                                }
                                settings::ThemeMode::System => {
                                    Theme::sync_system_appearance(Some(window), cx)
                                }
                            }
                            let shell = cx
                                .new(|cx| AppShell::new(workspace_store, window_state, window, cx));
                            cx.new(|cx| Root::new(shell, window, cx))
                        }
                    })
                    .expect("failed to open tcode window");

                let _ = window.update(cx, |_, window, _| {
                    window.set_window_title("tcode");
                    window.activate_window();
                });

                if open_latest {
                    let _ = host.dispatch(Command::OpenLatestSession);
                    for _ in 0..100 {
                        if cx.update(|cx| workspace_store.read(cx).active_session_id().is_some()) {
                            break;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(10))
                            .await;
                    }
                    workspace_store.update(cx, |store, _cx| {
                        store.sync_active_conversation_ui();
                    });
                }
            })
            .detach();
        });
}
