mod app;
mod assets;
mod checkpoints;
mod git;
mod session;
mod settings;
mod smoke;
mod store;
mod ui;

rust_i18n::i18n!("locales", fallback = "en");

use std::borrow::Cow;

use gpui::{
    AppContext as _, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::{
    ActiveTheme as _, Root, Theme, ThemeMode as ComponentThemeMode, ThemeRegistry,
};

const TCODE_THEME: &str = include_str!("../themes/tcode.json");

fn main() {
    env_logger::init();

    settings::apply_locale(None);

    let smoke_spec = smoke::parse_args();
    // Hidden debug/dev flag: open the most recently updated session on launch.
    let open_latest = std::env::args().any(|arg| arg == "--open-latest");
    // Hidden debug/dev flag: also open the diff panel on launch (pairs with
    // --open-latest; lets the diff panel be screenshotted headlessly).
    let open_diff = std::env::args().any(|arg| arg == "--open-diff");
    // Hidden debug/dev flag: open the right panel on the Plan/Tasks tab (pairs
    // with --open-latest; lets the plan panel be screenshotted headlessly).
    let open_plan = std::env::args().any(|arg| arg == "--open-plan");
    // Hidden dev flag: open the Preview tab and navigate to the given URL (pairs
    // with --open-latest) so the preview browser can be screenshotted headlessly.
    let open_preview = std::env::args()
        .skip_while(|arg| arg != "--open-preview")
        .nth(1);
    // Hidden debug/dev flag: open the active session's terminal drawer. This
    // implies --open-latest so it is useful by itself for screenshot checks.
    let open_terminal = std::env::args().any(|arg| arg == "--open-terminal");
    let terminal_demo = std::env::args().any(|arg| arg == "--terminal-demo");
    // Hidden debug/dev flags: open the settings page / command palette on
    // launch so those surfaces can be screenshotted headlessly.
    let open_settings = std::env::args().any(|arg| arg == "--open-settings");
    let open_palette = std::env::args().any(|arg| arg == "--open-palette");
    // Hidden dev flag: open a draft thread for a project (by id or name) so the
    // draft state can be screenshotted headlessly.
    let open_draft = std::env::args()
        .skip_while(|arg| arg != "--open-draft")
        .nth(1);
    // Screenshot-only: seed the composer text (drives the @/$// trigger menus)
    // or inject a pending image (paste/drag-drop cannot be driven headlessly).
    let debug_compose = std::env::args()
        .skip_while(|arg| arg != "--debug-compose")
        .nth(1);
    let debug_image = std::env::args()
        .skip_while(|arg| arg != "--debug-image")
        .nth(1)
        .map(std::path::PathBuf::from);
    // Screenshot-only: open a deterministic draft rooted at this cwd (so the
    // `@`-mention walk lists a known repo, independent of the newest session).
    let debug_cwd = std::env::args()
        .skip_while(|arg| arg != "--debug-cwd")
        .nth(1)
        .map(std::path::PathBuf::from);
    // Hidden E2E flags: run a real commit (`--debug-git-commit "msg"`) or
    // generate a commit message (`--debug-git-genmsg`) on the opened session's
    // repo, so the git flows can be exercised headlessly. Both imply --open-latest.
    let debug_git_commit = std::env::args()
        .skip_while(|arg| arg != "--debug-git-commit")
        .nth(1);
    let debug_git_genmsg = std::env::args().any(|arg| arg == "--debug-git-genmsg");
    // Screenshot-only: open the commit dialog on launch.
    let debug_git_dialog = std::env::args().any(|arg| arg == "--debug-git-dialog");
    // Hidden E2E flag: run a non-commit quick-action (push|pull|publish|init).
    let debug_git_action = std::env::args()
        .skip_while(|arg| arg != "--debug-git-action")
        .nth(1);
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
            // Bring up the in-process preview MCP server and register it with the
            // app so every spawned agent session can drive the embedded browser.
            match preview_mcp::start() {
                Ok(server) => {
                    log::info!("preview MCP server listening at {}", server.url);
                    app_state.update(cx, |state, _| state.attach_preview_mcp(server));
                }
                Err(err) => log::warn!("preview MCP server failed to start: {err}"),
            }
            // Refresh the model catalogs in the background so the picker shows
            // real, up-to-date models (the persisted cache serves until then).
            app_state.update(cx, |state, cx| state.refresh_model_catalogs(cx));
            {
                let (dc, di) = (debug_compose.clone(), debug_image.clone());
                app_state.update(cx, |state, _| {
                    state.debug_compose = dc;
                    state.debug_image = di;
                });
            }
            let debug_seed =
                debug_compose.is_some() || debug_image.is_some() || debug_cwd.is_some();
            settings::apply_locale(app_state.read(cx).settings.language.as_deref());
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
                // Seamless titlebar: transparent, with the traffic lights nudged
                // down to sit vertically centered in the 52px top strip.
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(18.))),
                }),
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
                } else if open_latest
                    || open_terminal
                    || terminal_demo
                    || open_settings
                    || open_palette
                    || open_plan
                    || open_preview.is_some()
                    || open_draft.is_some()
                    || debug_seed
                    || debug_git_commit.is_some()
                    || debug_git_genmsg
                    || debug_git_action.is_some()
                    || debug_git_dialog
                {
                    let _ = app_state.update(cx, |state, cx| {
                        if let Some(cwd) = debug_cwd.clone() {
                            // Deterministic draft rooted at `cwd` for screenshots.
                            if let Some(project_id) = state.create_project(cwd.clone(), cx) {
                                state.start_draft(project_id, cwd, cx);
                            }
                        } else if open_latest
                            || open_terminal
                            || open_plan
                            || debug_seed
                            || terminal_demo
                            || open_preview.is_some()
                            || debug_git_commit.is_some()
                            || debug_git_genmsg
                            || debug_git_action.is_some()
                            || debug_git_dialog
                        {
                            state.open_latest_session(cx);
                        }
                        if let Some(url) = &open_preview {
                            state.open_preview_with_url(url.clone(), cx);
                        }
                        if open_diff {
                            state.open_diff_panel(cx);
                        }
                        if open_plan {
                            state.toggle_plan_panel(cx);
                        }
                        if open_terminal {
                            state.open_terminal_panel(cx);
                        }
                        if terminal_demo {
                            state.open_terminal_demo(cx);
                        }
                        if open_settings {
                            state.open_settings(cx);
                        }
                        if open_palette {
                            state.open_palette(cx);
                        }
                        if let Some(key) = &open_draft {
                            if let Some(project) = state
                                .projects
                                .iter()
                                .find(|p| p.id == *key || p.name == *key)
                                .cloned()
                            {
                                state.start_draft(project.id, project.root, cx);
                            }
                        }
                        if let Some(message) = debug_git_commit.clone() {
                            state.debug_git_commit(message, cx);
                        }
                        if let Some(name) = debug_git_action.clone() {
                            state.debug_git_action(name, cx);
                        }
                        if debug_git_genmsg {
                            state.debug_git_generate_message(cx);
                        }
                        if debug_git_dialog {
                            state.debug_open_commit_dialog = true;
                        }
                    });
                }
            })
            .detach();
        });
}

#[cfg(test)]
mod locale_tests {
    use std::collections::BTreeSet;

    fn keys(yaml: &str) -> BTreeSet<String> {
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut keys = BTreeSet::new();
        for line in yaml
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let indent = line.len() - line.trim_start().len();
            let Some((name, value)) = line.trim().split_once(':') else {
                continue;
            };
            while stack.last().is_some_and(|(level, _)| *level >= indent) {
                stack.pop();
            }
            let mut path = stack
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>();
            path.push(name.trim());
            if value.trim().is_empty() {
                stack.push((indent, name.trim().to_owned()));
            } else {
                keys.insert(path.join("."));
            }
        }
        keys
    }

    #[test]
    fn locale_keys_match() {
        let en = keys(include_str!("../locales/en.yml"));
        let zh = keys(include_str!("../locales/zh-CN.yml"));
        assert_eq!(en, zh, "English and zh-CN locale keys differ");
    }
}
