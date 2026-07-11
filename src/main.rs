// Windows: run as a GUI app so launching tcode does not open a console window.
// Debug builds keep the console so `RUST_LOG` output stays visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod acp_registry;
mod app;
mod assets;
mod checkpoints;
mod git;
mod process;
mod provider_models;
mod provider_status;
mod session;
mod settings;
mod smoke;
mod store;
mod ui;
mod version_check;

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
    let debug_diff_scope = std::env::args()
        .skip_while(|arg| arg != "--debug-diff-scope")
        .nth(1);
    let debug_diff_split = std::env::args().any(|arg| arg == "--debug-diff-split");
    let debug_diff_scope_menu = std::env::args().any(|arg| arg == "--debug-diff-scope-menu");
    let debug_review_comment = std::env::args().any(|arg| arg == "--debug-review-comment");
    // Screenshot-only: start the opened session's provider process (without
    // sending a turn) so provider-supplied state — the `/` and `$` command feed
    // — is reachable headlessly. Implies --open-latest. Note Claude's CLI only
    // emits its system-init (carrying `slash_commands`) once it receives a user
    // message, so use `--debug-send` to populate its command feed.
    let debug_live = std::env::args().any(|arg| arg == "--debug-live");
    // Screenshot-only: send one turn on launch (without exiting, unlike --smoke)
    // so provider state that only arrives with the first message is reachable.
    let debug_send = std::env::args()
        .skip_while(|arg| arg != "--debug-send")
        .nth(1);
    // Screenshot / E2E only: `--debug-queue "msg1|msg2"` sends each `|`-separated
    // message through the ordinary `send_turn` path on launch. Because the
    // provider is still starting, they all land in the queue; the first flushes
    // when it goes live and the rest stay queued — which both renders the queue
    // strip for screenshots and exercises the real dispatch-on-completion FIFO
    // (pair it with `--debug-send` to occupy the provider with a long turn).
    let debug_queue = std::env::args()
        .skip_while(|arg| arg != "--debug-queue")
        .nth(1);
    // Screenshot-only: seed the command palette query (pairs with --open-palette).
    let debug_palette = std::env::args()
        .skip_while(|arg| arg != "--debug-palette")
        .nth(1);
    // Screenshot-only: open a specific Settings section (pairs with --open-settings).
    let debug_settings_section = std::env::args()
        .skip_while(|arg| arg != "--debug-settings-section")
        .nth(1);
    // Screenshot-only: seed the ACP marketplace's search box (typing cannot be
    // driven headlessly), so the filtered list can be captured.
    let debug_acp_search = std::env::args()
        .skip_while(|arg| arg != "--debug-acp-search")
        .nth(1);
    // Screenshot-only: expand one provider card (pairs with the above).
    let debug_provider_expanded = std::env::args()
        .skip_while(|arg| arg != "--debug-provider-expanded")
        .nth(1);
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

            // Global ⌘K / Ctrl-K opens/closes the command palette (handled by
            // AppShell). `secondary` is gpui's platform modifier: command on
            // macOS, control on Windows/Linux — where a literal `cmd-` binding
            // would mean the Super/Win key, which the OS intercepts.
            cx.bind_keys([KeyBinding::new("secondary-k", ui::TogglePalette, None)]);

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
            // Check provider CLI versions on launch (unless disabled), toasting
            // when a newer version is available (s3 §6).
            app_state.update(cx, |state, cx| {
                if state.provider_update_checks_enabled() {
                    state.check_provider_versions(cx);
                }
                // Probe each provider's install + auth state for the Settings →
                // Providers cards (independent of the update-check toggle).
                state.refresh_provider_status(cx);
            });
            {
                let dc = debug_compose.clone();
                let di = debug_image.clone();
                let dscope = debug_diff_scope.clone();
                let dp = debug_palette.clone();
                let dsec = debug_settings_section.clone();
                let dacp = debug_acp_search.clone();
                let dexp = debug_provider_expanded.clone();
                app_state.update(cx, |state, _| {
                    state.debug_compose = dc;
                    state.debug_image = di;
                    state.debug_diff_scope = dscope;
                    state.debug_diff_split = debug_diff_split;
                    state.debug_diff_scope_menu = debug_diff_scope_menu;
                    state.debug_review_comment = debug_review_comment;
                    state.debug_palette = dp;
                    state.debug_settings_section = dsec;
                    state.debug_acp_search = dacp;
                    state.debug_provider_expanded = dexp;
                });
            }
            let debug_seed = debug_compose.is_some()
                || debug_image.is_some()
                || debug_cwd.is_some()
                || debug_diff_scope.is_some()
                || debug_diff_split
                || debug_diff_scope_menu
                || debug_review_comment
                || debug_live
                || debug_send.is_some()
                || debug_queue.is_some();
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
                    app_state.update(cx, |state, _| state.shutdown_active());
                    async {}
                }
            });
            quit_subscription.detach();

            let window_options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1200.), px(800.)), cx)),
                window_min_size: Some(size(px(900.), px(600.))),
                // macOS: seamless titlebar — transparent, with the traffic lights
                // nudged down to sit vertically centered in the 52px top strip.
                //
                // Windows/Linux: a transparent titlebar means a *client-decorated*
                // window, and we draw no minimize/maximize/close controls of our
                // own (traffic_light_position is a macOS no-op) — the window would
                // have no way to be closed from the chrome. So there we keep the
                // native system titlebar; our top strip simply sits below it.
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: cfg!(target_os = "macos"),
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
                    cx.update(|cx| smoke::drive(spec, app_state, cx));
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
                    app_state.update(cx, |state, cx| {
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
                        if let Some(key) = &open_draft
                            && let Some(project) = state
                                .projects
                                .iter()
                                .find(|p| p.id == *key || p.name == *key)
                                .cloned()
                        {
                            state.start_draft(project.id, project.root, cx);
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
                        if debug_live {
                            state.debug_start_provider(cx);
                        }
                        if let Some(text) = debug_send.clone() {
                            state.send_turn(text, Vec::new(), cx);
                        }
                        if let Some(queued) = debug_queue.clone() {
                            for message in queued.split('|').filter(|m| !m.trim().is_empty()) {
                                state.send_turn(message.trim().to_string(), Vec::new(), cx);
                            }
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
