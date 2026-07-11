mod app;
mod assets;
mod session;
mod settings;
mod smoke;
mod store;
mod ui;

use std::borrow::Cow;

use gpui::{AppContext as _, KeyBinding, WindowBounds, WindowOptions, px, size};
use gpui_component::{
    ActiveTheme as _, Root, Theme, ThemeMode as ComponentThemeMode, ThemeRegistry, TitleBar,
};

const TCODE_THEME: &str = include_str!("../themes/tcode.json");

fn main() {
    env_logger::init();

    let smoke_spec = smoke::parse_args();
    // Hidden debug/dev flag: open the most recently updated session on launch.
    let open_latest = std::env::args().any(|arg| arg == "--open-latest");
    // Hidden debug/dev flag: also open the diff panel on launch (pairs with
    // --open-latest; lets the diff panel be screenshotted headlessly).
    let open_diff = std::env::args().any(|arg| arg == "--open-diff");
    // Hidden debug/dev flags: open the settings page / command palette on
    // launch so those surfaces can be screenshotted headlessly.
    let open_settings = std::env::args().any(|arg| arg == "--open-settings");
    let open_palette = std::env::args().any(|arg| arg == "--open-palette");
    let store = store::SessionStore::open_default().expect("failed to open tcode data directory");

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);

            // Global ⌘K opens/closes the command palette (handled by AppShell).
            cx.bind_keys([KeyBinding::new("cmd-k", ui::TogglePalette, None)]);

            cx.text_system()
                .add_fonts(vec![Cow::Borrowed(assets::DM_SANS)])
                .expect("failed to register bundled DM Sans font");
            ThemeRegistry::global_mut(cx)
                .load_themes_from_str(TCODE_THEME)
                .expect("embedded themes/tcode.json must be valid");
            let light = ThemeRegistry::global(cx).themes()["tcode Light"].clone();
            let dark = ThemeRegistry::global(cx).themes()["tcode Dark"].clone();
            Theme::global_mut(cx).apply_config(&light);
            Theme::global_mut(cx).apply_config(&dark);

            let app_state = cx.new(|_| app::AppState::new(store));
            match app_state.read(cx).settings.theme_mode {
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
                let app_state = app_state.clone();
                move |cx| {
                    let _ = app_state.update(cx, |state, _| state.shutdown_active());
                    async {}
                }
            });
            quit_subscription.detach();

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(800.)), cx)),
                window_min_size: Some(size(px(900.), px(600.))),
                titlebar: Some(TitleBar::title_bar_options()),
                ..Default::default()
            };

            cx.spawn(async move |cx| {
                let window = cx
                    .open_window(window_options, {
                        let app_state = app_state.clone();
                        move |window, cx| {
                            match app_state.read(cx).settings.theme_mode {
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
                            let shell = cx.new(|cx| ui::AppShell::new(app_state, window, cx));
                            cx.new(|cx| Root::new(shell, window, cx))
                        }
                    })
                    .expect("failed to open tcode window");

                let _ = window.update(cx, |_, window, _| {
                    window.set_window_title("tcode");
                    window.activate_window();
                });

                if let Some(spec) = smoke_spec {
                    let _ = cx.update(|cx| smoke::drive(spec, app_state, cx));
                } else if open_latest || open_settings || open_palette {
                    let _ = app_state.update(cx, |state, cx| {
                        if open_latest {
                            state.open_latest_session(cx);
                        }
                        if open_diff {
                            state.open_diff_panel(cx);
                        }
                        if open_settings {
                            state.open_settings(cx);
                        }
                        if open_palette {
                            state.open_palette(cx);
                        }
                    });
                }
            })
            .detach();
        });
}
