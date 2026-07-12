//! Settings → Providers → "ACP Agents": the marketplace and the installed cards.
//!
//! Sits below the two native provider cards. The marketplace lists every agent
//! in the ACP registry *except* `claude-acp` / `codex-acp` (adapters over the
//! very CLIs tcode already drives natively — see [`crate::acp_registry`]), plus
//! a "Custom agent…" escape hatch for anything not in the index. Installed
//! agents each get a card with the same knobs as the native ones: an enable
//! switch, environment variables, and launch arguments.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    switch::Switch,
    v_flex,
};

use crate::acp_registry::{InstalledAgent, RegistryAgent, platform_key, resolve_recipe};
use crate::app::AppState;

pub struct AcpPanel {
    app_state: Entity<AppState>,
    search: Entity<InputState>,
    /// The "Custom agent…" form, shown when the user expands it.
    custom_open: bool,
    custom_name: Entity<InputState>,
    custom_command: Entity<InputState>,
    custom_args: Entity<InputState>,
    custom_env: Entity<InputState>,
    /// Per-installed-agent editors (launch args + env), keyed by registry id.
    editors: Vec<(String, Entity<InputState>, Entity<InputState>)>,
    _subscriptions: Vec<Subscription>,
}

impl AcpPanel {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.observe(&app_state, |this, _, cx| {
            this.editors.clear();
            cx.notify();
        })];
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = placeholder.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let seed = app_state.read(cx).debug_acp_search.clone();
        let search = input(&rust_i18n::t!("providers.acp.search"), window, cx);
        if let Some(seed) = seed {
            search.update(cx, |input, cx| input.set_value(seed, window, cx));
        }
        let panel = Self {
            search,
            custom_open: false,
            custom_name: input("My agent", window, cx),
            custom_command: input("node", window, cx),
            custom_args: input("/path/to/agent.js --acp", window, cx),
            custom_env: input("KEY=value KEY2=value2", window, cx),
            editors: Vec::new(),
            app_state,
            _subscriptions: subscriptions,
        };
        // The registry is cheap when cached and refreshes in the background.
        panel
            .app_state
            .clone()
            .update(cx, |state, cx| state.refresh_acp_registry(cx));
        panel
    }

    /// The launch-args + env inputs for one installed agent, created lazily so a
    /// fresh install picks up its persisted values.
    fn editor(
        &mut self,
        agent: &InstalledAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<InputState>, Entity<InputState>) {
        if let Some((_, args, env)) = self.editors.iter().find(|(id, _, _)| *id == agent.id) {
            return (args.clone(), env.clone());
        }
        let args = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(rust_i18n::t!("providers.acp.args_hint"));
            input.set_value(agent.launch_args.clone().unwrap_or_default(), window, cx);
            input
        });
        let env = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(rust_i18n::t!("providers.acp.env_hint"));
            input.set_value(format_env(&agent.env), window, cx);
            input
        });
        self.editors
            .push((agent.id.clone(), args.clone(), env.clone()));
        (args, env)
    }

    fn render_installed(
        &mut self,
        agent: &InstalledAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (args_input, env_input) = self.editor(agent, window, cx);
        let id = agent.id.clone();
        let (commit_id, commit_args, commit_env) =
            (id.clone(), args_input.clone(), env_input.clone());
        let remove_id = id.clone();
        let toggle_id = id.clone();
        let subtitle = launch_summary(agent);

        v_flex()
            .w_full()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::empty()
                            .path("icons/box.svg")
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_medium()
                                    .child(agent.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!("acp-remove-{id}")))
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("providers.acp.remove").into_owned())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = remove_id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.remove_acp_agent(&id, cx));
                            })),
                    )
                    .child(
                        Switch::new(gpui::SharedString::from(format!("acp-enable-{id}")))
                            .checked(agent.enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                let (id, checked) = (toggle_id.clone(), *checked);
                                this.app_state.update(cx, |state, cx| {
                                    state.update_acp_agent(&id, |a| a.enabled = checked, cx)
                                });
                            })),
                    ),
            )
            .child(field_label(
                rust_i18n::t!("providers.acp.args").into_owned(),
                cx,
            ))
            .child(Input::new(&args_input).xsmall())
            .child(field_label(
                rust_i18n::t!("providers.acp.env").into_owned(),
                cx,
            ))
            .child(Input::new(&env_input).xsmall())
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new(gpui::SharedString::from(format!("acp-save-{commit_id}")))
                        .outline()
                        .xsmall()
                        .label(rust_i18n::t!("providers.acp.save").into_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let args = commit_args.read(cx).value().to_string();
                            let env = parse_env(&commit_env.read(cx).value());
                            let id = commit_id.clone();
                            this.app_state.update(cx, |state, cx| {
                                state.update_acp_agent(
                                    &id,
                                    |agent| {
                                        let args = args.trim().to_string();
                                        agent.launch_args = (!args.is_empty()).then_some(args);
                                        agent.env = env;
                                    },
                                    cx,
                                )
                            });
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_market_row(&self, agent: &RegistryAgent, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let installed = state.settings.acp_agent(&agent.id).is_some();
        let installing = state.acp_installing.contains(&agent.id);
        let runnable = resolve_recipe(agent, &platform_key()).is_some();
        let id = agent.id.clone();

        h_flex()
            .w_full()
            .p_3()
            .gap_3()
            .items_start()
            .child(
                Icon::empty()
                    .path("icons/box.svg")
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_medium()
                                    .child(agent.name.clone()),
                            )
                            .when(!agent.version.is_empty(), |row| {
                                row.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("v{}", agent.version)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(agent.description.clone()),
                    ),
            )
            .child(if installed {
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("providers.acp.installed").into_owned())
                    .into_any_element()
            } else if !runnable {
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("providers.acp.unsupported").into_owned())
                    .into_any_element()
            } else {
                Button::new(gpui::SharedString::from(format!(
                    "acp-install-{}",
                    agent.id
                )))
                .outline()
                .xsmall()
                .loading(installing)
                .label(rust_i18n::t!("providers.acp.install").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let id = id.clone();
                    this.app_state
                        .update(cx, |state, cx| state.install_acp_agent(id, cx));
                }))
                .into_any_element()
            })
            .into_any_element()
    }

    fn render_custom(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.custom_open {
            return h_flex()
                .id("acp-custom-open")
                .w_full()
                .p_3()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().accent))
                .child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                .child(
                    div()
                        .text_size(px(13.))
                        .child(rust_i18n::t!("providers.acp.custom").into_owned()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.custom_open = true;
                    cx.notify();
                }))
                .into_any_element();
        }
        let (name, command, args, env) = (
            self.custom_name.clone(),
            self.custom_command.clone(),
            self.custom_args.clone(),
            self.custom_env.clone(),
        );
        v_flex()
            .w_full()
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_size(px(13.))
                    .font_medium()
                    .child(rust_i18n::t!("providers.acp.custom").into_owned()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("providers.acp.custom_help").into_owned()),
            )
            .child(Input::new(&self.custom_name).xsmall())
            .child(Input::new(&self.custom_command).xsmall())
            .child(Input::new(&self.custom_args).xsmall())
            .child(Input::new(&self.custom_env).xsmall())
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("acp-custom-cancel")
                            .ghost()
                            .xsmall()
                            .label(rust_i18n::t!("settings.cancel").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.custom_open = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("acp-custom-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(rust_i18n::t!("providers.acp.add").into_owned())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let name = name.read(cx).value().to_string();
                                let command = command.read(cx).value().to_string();
                                let args: Vec<String> = args
                                    .read(cx)
                                    .value()
                                    .split_whitespace()
                                    .map(str::to_string)
                                    .collect();
                                let env = parse_env(&env.read(cx).value());
                                this.app_state.update(cx, |state, cx| {
                                    state.add_custom_acp_agent(name, command, args, env, cx)
                                });
                                this.custom_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AcpPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let installed: Vec<InstalledAgent> = state
            .settings
            .installed_acp_agents()
            .into_iter()
            .cloned()
            .collect();
        let market: Vec<RegistryAgent> = state
            .acp_marketplace()
            .into_iter()
            .filter(|agent| {
                query.is_empty()
                    || agent.name.to_lowercase().contains(&query)
                    || agent.id.contains(&query)
                    || agent.description.to_lowercase().contains(&query)
            })
            .collect();
        let error = state.acp_registry_error.clone();
        let loading = state.acp_registry_loading;
        let empty = market.is_empty();
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        let mut list = v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(border)
            .overflow_hidden();

        // Installed agents first: they are what the model picker will offer.
        for (index, agent) in installed.iter().enumerate() {
            let row = self.render_installed(agent, window, cx);
            list = list.child(
                v_flex()
                    .w_full()
                    .items_stretch()
                    .when(index > 0, |d| d.border_t_1().border_color(border))
                    .child(row),
            );
        }

        let mut rows = v_flex().w_full();
        if let Some(error) = error.filter(|_| empty) {
            rows = rows.child(
                div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(error),
            );
        } else if empty && loading {
            rows = rows.child(
                div()
                    .p_3()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child(rust_i18n::t!("providers.acp.loading").into_owned()),
            );
        }
        for agent in &market {
            rows = rows.child(
                v_flex()
                    .w_full()
                    .items_stretch()
                    .border_t_1()
                    .border_color(border)
                    .child(self.render_market_row(agent, cx)),
            );
        }
        let custom = self.render_custom(cx);
        rows = rows.child(
            v_flex()
                .w_full()
                .items_stretch()
                .border_t_1()
                .border_color(border)
                .child(custom),
        );

        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .pt_4()
                    .pb_2()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .font_medium()
                            .text_color(muted)
                            .child(rust_i18n::t!("providers.acp.section").into_owned()),
                    )
                    .child(div().w(px(200.)).child(Input::new(&self.search).xsmall())),
            )
            .child(list.child(rows))
    }
}

fn field_label(label: String, cx: &Context<AcpPanel>) -> AnyElement {
    div()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(label)
        .into_any_element()
}

/// One line describing how an installed agent starts, so the card shows what it
/// will actually run.
fn launch_summary(agent: &InstalledAgent) -> String {
    match &agent.launch {
        agent::AcpLaunch::Npx { package, args, .. } => format!("npx {package} {}", args.join(" "))
            .trim_end()
            .to_string(),
        agent::AcpLaunch::Binary { command, args, .. } => {
            format!("{} {}", command.display(), args.join(" "))
                .trim_end()
                .to_string()
        }
        agent::AcpLaunch::Custom { command, args, .. } => format!("{command} {}", args.join(" "))
            .trim_end()
            .to_string(),
    }
}

/// `KEY=value KEY2=value2` → pairs (the same shorthand the custom-agent form
/// takes). Entries without `=` are dropped.
fn parse_env(raw: &str) -> Vec<(String, String)> {
    raw.split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| !key.trim().is_empty())
        .map(|(key, value)| (key.trim().to_string(), value.to_string()))
        .collect()
}

fn format_env(env: &[(String, String)]) -> String {
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_shorthand_round_trips() {
        let pairs = parse_env("  API_KEY=abc  BASE_URL=https://x/y  bogus ");
        assert_eq!(
            pairs,
            vec![
                ("API_KEY".to_string(), "abc".to_string()),
                ("BASE_URL".to_string(), "https://x/y".to_string()),
            ]
        );
        assert_eq!(format_env(&pairs), "API_KEY=abc BASE_URL=https://x/y");
    }

    #[test]
    fn launch_summary_shows_the_real_command() {
        let npx = InstalledAgent {
            id: "gemini".into(),
            name: "Gemini".into(),
            version: "1".into(),
            icon: None,
            launch: agent::AcpLaunch::Npx {
                package: "@google/gemini-cli@0.50.0".into(),
                args: vec!["--acp".into()],
                env: Vec::new(),
            },
            archive_sha256: None,
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        };
        assert_eq!(launch_summary(&npx), "npx @google/gemini-cli@0.50.0 --acp");
    }
}
