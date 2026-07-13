//! First-class ACP provider cards and the modal agent marketplace.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    switch::Switch,
    v_flex,
};

use crate::acp_registry::{InstalledAgent, RegistryAgent, platform_key, resolve_recipe};
use crate::app::AppState;

/// One installed ACP agent, rendered with the same anatomy as a native provider card.
pub struct AcpAgentCard {
    app_state: Entity<AppState>,
    agent_id: String,
    expanded: bool,
    args: Entity<InputState>,
    env: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AcpAgentCard {
    pub fn new(
        app_state: Entity<AppState>,
        agent: &InstalledAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let subscriptions = vec![cx.observe(&app_state, |_, _, cx| cx.notify())];
        Self {
            app_state,
            agent_id: agent.id.clone(),
            expanded: false,
            args,
            env,
            _subscriptions: subscriptions,
        }
    }

    fn render_header(&self, agent: &InstalledAgent, cx: &mut Context<Self>) -> AnyElement {
        let id = agent.id.clone();
        let toggle_id = id.clone();
        let name = agent.name.clone();
        let muted = cx.theme().muted_foreground;
        let dot_color = if agent.enabled {
            cx.theme().success
        } else {
            rgb(0xf59e0b).into()
        };
        let glyph = div()
            .relative()
            .flex_none()
            .size(px(20.))
            .child(
                Icon::empty()
                    .path("icons/box.svg")
                    .small()
                    .text_color(cx.theme().foreground),
            )
            .child(
                div()
                    .absolute()
                    .left(px(-3.))
                    .top(px(-3.))
                    .size(px(7.))
                    .rounded_full()
                    .bg(dot_color),
            );
        let title = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_size(px(14.))
                    .font_semibold()
                    .child(agent.name.clone()),
            )
            .when(!agent.version.is_empty(), |row| {
                row.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(format!("v{}", agent.version.trim_start_matches('v'))),
                )
            });

        h_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_3()
            .items_center()
            .child(glyph)
            .child(
                v_flex().flex_1().min_w_0().gap_0p5().child(title).child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(launch_summary(agent)),
                ),
            )
            .child(
                Button::new(gpui::SharedString::from(format!("acp-details-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip(rust_i18n::t!(
                        "providers.toggle_details",
                        name = name.clone()
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new(gpui::SharedString::from(format!("acp-enable-{id}")))
                    .checked(agent.enabled)
                    .tooltip(rust_i18n::t!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let (id, checked) = (toggle_id.clone(), *checked);
                        this.app_state.update(cx, |state, cx| {
                            state.update_acp_agent(&id, |agent| agent.enabled = checked, cx)
                        });
                    })),
            )
            .into_any_element()
    }

    fn field_block(&self, label: String, control: AnyElement, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_1p5()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(div().text_size(px(13.)).font_medium().child(label))
            .child(control)
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let (args, env) = (self.args.clone(), self.env.clone());
        let save_id = self.agent_id.clone();
        let remove_id = self.agent_id.clone();
        v_flex()
            .w_full()
            .child(self.field_block(
                rust_i18n::t!("providers.acp.args").into_owned(),
                Input::new(&self.args).xsmall().into_any_element(),
                cx,
            ))
            .child(
                self.field_block(
                    rust_i18n::t!("providers.acp.env").into_owned(),
                    v_flex()
                        .w_full()
                        .gap_2()
                        .child(Input::new(&self.env).xsmall())
                        .child(
                            h_flex().w_full().justify_end().child(
                                Button::new(gpui::SharedString::from(format!(
                                    "acp-save-{save_id}"
                                )))
                                .outline()
                                .xsmall()
                                .label(rust_i18n::t!("providers.acp.save").into_owned())
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let launch_args = args.read(cx).value().trim().to_string();
                                        let parsed_env = parse_env(&env.read(cx).value());
                                        let id = save_id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.update_acp_agent(
                                                &id,
                                                |agent| {
                                                    agent.launch_args = (!launch_args.is_empty())
                                                        .then_some(launch_args);
                                                    agent.env = parsed_env;
                                                },
                                                cx,
                                            )
                                        });
                                    },
                                )),
                            ),
                        )
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new(gpui::SharedString::from(format!("acp-remove-{remove_id}")))
                            .outline()
                            .danger()
                            .small()
                            .label(rust_i18n::t!("providers.acp.remove").into_owned())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let id = remove_id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.remove_acp_agent(&id, cx));
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AcpAgentCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent = self
            .app_state
            .read(cx)
            .settings
            .acp_agent(&self.agent_id)
            .cloned();
        v_flex().w_full().when_some(agent, |card, agent| {
            card.child(self.render_header(&agent, cx))
                .when(self.expanded, |card| card.child(self.render_details(cx)))
        })
    }
}

/// Long-lived state for the Add agent dialog.
pub struct AcpPanel {
    app_state: Entity<AppState>,
    search: Entity<InputState>,
    custom_open: bool,
    custom_name: Entity<InputState>,
    custom_command: Entity<InputState>,
    custom_args: Entity<InputState>,
    custom_env: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AcpPanel {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()))
        };
        let search = input(&rust_i18n::t!("providers.acp.search"), window, cx);
        if let Some(seed) = app_state.read(cx).debug_acp_search.clone() {
            search.update(cx, |input, cx| input.set_value(seed, window, cx));
        }
        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        let panel = Self {
            app_state,
            search,
            custom_open: false,
            custom_name: input("My agent", window, cx),
            custom_command: input("node", window, cx),
            custom_args: input("/path/to/agent.js --acp", window, cx),
            custom_env: input("KEY=value KEY2=value2", window, cx),
            _subscriptions: subscriptions,
        };
        panel
            .app_state
            .update(cx, |state, cx| state.refresh_acp_registry(cx));
        panel
    }

    pub fn prepare_to_open(&mut self, cx: &mut Context<Self>) {
        self.custom_open = false;
        cx.notify();
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
                                        .font_family("monospace")
                                        .text_size(px(12.))
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

    fn render_marketplace(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.app_state.read(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let market: Vec<RegistryAgent> = state
            .acp_marketplace()
            .into_iter()
            .filter(|agent| {
                query.is_empty()
                    || agent.name.to_lowercase().contains(&query)
                    || agent.id.to_lowercase().contains(&query)
                    || agent.description.to_lowercase().contains(&query)
            })
            .collect();
        let error = state.acp_registry_error.clone();
        let loading = state.acp_registry_loading;
        let empty = market.is_empty();
        let border = cx.theme().border;
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
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("providers.acp.loading").into_owned()),
            );
        }
        for (index, agent) in market.iter().enumerate() {
            rows = rows.child(
                v_flex()
                    .w_full()
                    .when(index > 0, |row| row.border_t_1().border_color(border))
                    .child(self.render_market_row(agent, cx)),
            );
        }
        v_flex()
            .w_full()
            .gap_3()
            .child(Input::new(&self.search).small())
            .child(
                v_flex()
                    .w_full()
                    .max_h(px(360.))
                    .overflow_y_scrollbar()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(border)
                    .child(rows),
            )
            .child(
                h_flex()
                    .id("acp-custom-open")
                    .w_full()
                    .pt_3()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(border)
                    .cursor_pointer()
                    .child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(rust_i18n::t!("providers.acp.custom").into_owned()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.custom_open = true;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_custom(&self, cx: &mut Context<Self>) -> AnyElement {
        let (name, command, args, env) = (
            self.custom_name.clone(),
            self.custom_command.clone(),
            self.custom_args.clone(),
            self.custom_env.clone(),
        );
        v_flex()
            .w_full()
            .gap_3()
            .child(
                Button::new("acp-custom-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .label(rust_i18n::t!("settings.back").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.custom_open = false;
                        cx.notify();
                    })),
            )
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
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("acp-custom-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(rust_i18n::t!("providers.acp.add").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = name.read(cx).value().to_string();
                                let command = command.read(cx).value().to_string();
                                if name.trim().is_empty() || command.trim().is_empty() {
                                    return;
                                }
                                let args = args
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
                                window.close_dialog(cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AcpPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .when(!self.custom_open, |view| {
                view.child(self.render_marketplace(cx))
            })
            .when(self.custom_open, |view| view.child(self.render_custom(cx)))
    }
}

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
