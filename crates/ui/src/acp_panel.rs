//! First-class ACP provider cards and the modal agent marketplace.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
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

use tcode_core::acp::InstalledAcpAgent;
use tcode_runtime::app::AppState;
use tcode_runtime::ui_facade::AcpMarketplaceItem;

use crate::material;

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
        agent: &InstalledAcpAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let args = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(tcode_i18n::tr!("providers.acp.args_hint"));
            input.set_value(agent.launch_args.clone().unwrap_or_default(), window, cx);
            input
        });
        let env = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(tcode_i18n::tr!("providers.acp.env_hint"));
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

    fn render_header(&self, agent: &InstalledAcpAgent, cx: &mut Context<Self>) -> AnyElement {
        let id = agent.id.clone();
        let toggle_id = id.clone();
        let name = agent.name.clone();
        let muted = cx.theme().muted_foreground;
        let dot_color = if agent.enabled {
            cx.theme().success
        } else {
            cx.theme().warning
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
                    .text_size(px(15.))
                    .font_semibold()
                    .child(agent.name.clone()),
            )
            .when(!agent.version.is_empty(), |row| {
                row.child(
                    div()
                        .font_family("monospace")
                        .text_size(px(13.))
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
                        .text_size(px(13.))
                        .text_color(muted)
                        .child(launch_summary(agent)),
                ),
            )
            .child(
                Button::new(gpui::SharedString::from(format!("acp-details-{id}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip(tcode_i18n::tr!(
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
                    .tooltip(tcode_i18n::tr!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let (id, checked) = (toggle_id.clone(), *checked);
                        this.app_state.update(cx, |state, cx| {
                            state.update_acp_agent(&id, |agent| agent.enabled = checked, cx)
                        });
                    })),
            )
            .into_any_element()
    }

    fn field_block(&self, label: String, control: AnyElement, _cx: &Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_1p5()
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
                tcode_i18n::tr!("providers.acp.args").into_owned(),
                Input::new(&self.args).xsmall().into_any_element(),
                cx,
            ))
            .child(
                self.field_block(
                    tcode_i18n::tr!("providers.acp.env").into_owned(),
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
                                .label(tcode_i18n::tr!("providers.acp.save").into_owned())
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
                h_flex().w_full().justify_end().px_4().py_3().child(
                    Button::new(gpui::SharedString::from(format!("acp-remove-{remove_id}")))
                        .outline()
                        .danger()
                        .small()
                        .label(tcode_i18n::tr!("providers.acp.remove").into_owned())
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
        v_flex()
            .w_full()
            .rounded(material::radius_card())
            .bg(cx.theme().secondary)
            .overflow_hidden()
            .when_some(agent, |card, agent| {
                card.child(self.render_header(&agent, cx))
                    .when(self.expanded, |card| card.child(self.render_details(cx)))
            })
    }
}

/// Kimi's Anthropic-compatible coding endpoint (the built-in third-party preset).
const KIMI_BASE_URL: &str = "https://api.kimi.com/coding/";
const KIMI_MODEL: &str = "k3[1m]";
const KIMI_NAME: &str = "Kimi";

/// Which screen the Add-agent dialog is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelView {
    /// Third-party endpoint entries + the ACP agent marketplace.
    Home,
    /// The third-party Claude Code endpoint form (Kimi preset or custom).
    ThirdParty,
    /// The custom ACP agent form.
    CustomAcp,
}

/// Which third-party preset the endpoint form is seeded from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TpPreset {
    /// Bundled Kimi preset: base URL + model pre-filled, only the key needed.
    Kimi,
    /// A blank Anthropic-compatible endpoint the user fills in fully.
    Custom,
}

/// Long-lived state for the Add agent dialog.
pub struct AcpPanel {
    app_state: Entity<AppState>,
    view: PanelView,
    search: Entity<InputState>,
    custom_name: Entity<InputState>,
    custom_command: Entity<InputState>,
    custom_args: Entity<InputState>,
    custom_env: Entity<InputState>,
    /// Third-party Claude Code endpoint form.
    tp_preset: TpPreset,
    tp_name: Entity<InputState>,
    tp_base_url: Entity<InputState>,
    tp_model: Entity<InputState>,
    tp_key: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AcpPanel {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()))
        };
        let search = input(&tcode_i18n::tr!("providers.acp.search"), window, cx);
        if let Some(seed) = app_state.read(cx).debug_acp_search.clone() {
            search.update(cx, |input, cx| input.set_value(seed, window, cx));
        }
        let subscriptions = vec![
            cx.observe(&app_state, |_, _, cx| cx.notify()),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        let panel = Self {
            app_state,
            view: PanelView::Home,
            search,
            custom_name: input("My agent", window, cx),
            custom_command: input("node", window, cx),
            custom_args: input("/path/to/agent.js --acp", window, cx),
            custom_env: input("KEY=value KEY2=value2", window, cx),
            tp_preset: TpPreset::Kimi,
            tp_name: input(KIMI_NAME, window, cx),
            tp_base_url: input(KIMI_BASE_URL, window, cx),
            tp_model: input(KIMI_MODEL, window, cx),
            tp_key: input(
                &tcode_i18n::tr!("providers.third_party.key_hint"),
                window,
                cx,
            ),
            _subscriptions: subscriptions,
        };
        panel
            .app_state
            .update(cx, |state, cx| state.refresh_acp_registry(cx));
        panel
    }

    pub fn prepare_to_open(&mut self, cx: &mut Context<Self>) {
        self.view = PanelView::Home;
        cx.notify();
    }

    /// Seed the third-party form's inputs from a preset and switch to it.
    fn open_third_party(&mut self, preset: TpPreset, window: &mut Window, cx: &mut Context<Self>) {
        self.tp_preset = preset;
        let (name, base_url, model) = match preset {
            TpPreset::Kimi => (KIMI_NAME, KIMI_BASE_URL, KIMI_MODEL),
            TpPreset::Custom => ("", "", ""),
        };
        self.tp_name
            .update(cx, |i, cx| i.set_value(name, window, cx));
        self.tp_base_url
            .update(cx, |i, cx| i.set_value(base_url, window, cx));
        self.tp_model
            .update(cx, |i, cx| i.set_value(model, window, cx));
        self.tp_key.update(cx, |i, cx| i.set_value("", window, cx));
        self.view = PanelView::ThirdParty;
        cx.notify();
    }

    fn render_market_row(&self, agent: &AcpMarketplaceItem, cx: &mut Context<Self>) -> AnyElement {
        let id = agent.id.clone();
        h_flex()
            .id(gpui::SharedString::from(format!("acp-market-row-{id}")))
            .w_full()
            .p_3()
            .gap_3()
            .items_start()
            .hover(|row| row.bg(cx.theme().list_hover))
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
                                    .text_size(px(15.))
                                    .font_medium()
                                    .child(agent.name.clone()),
                            )
                            .when(!agent.version.is_empty(), |row| {
                                row.child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(13.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("v{}", agent.version)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(agent.description.clone()),
                    ),
            )
            .child(if agent.installed {
                div()
                    .rounded_full()
                    .bg(cx.theme().success.opacity(0.12))
                    .text_size(px(13.))
                    .text_color(cx.theme().success_foreground)
                    .child(tcode_i18n::tr!("providers.acp.installed").into_owned())
                    .into_any_element()
            } else if !agent.supported {
                div()
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("providers.acp.unsupported").into_owned())
                    .into_any_element()
            } else {
                Button::new(gpui::SharedString::from(format!(
                    "acp-install-{}",
                    agent.id
                )))
                .outline()
                .xsmall()
                .loading(agent.installing)
                .label(tcode_i18n::tr!("providers.acp.install").into_owned())
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
        let market: Vec<AcpMarketplaceItem> = state
            .acp_marketplace_items()
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
        let mut rows = v_flex().w_full();
        if let Some(error) = error.filter(|_| empty) {
            rows = rows.child(
                div()
                    .flex_none()
                    .p_3()
                    .text_size(px(13.))
                    .text_color(cx.theme().danger_foreground)
                    .child(error),
            );
        } else if empty && loading {
            rows = rows.child(
                div()
                    .flex_none()
                    .p_3()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("providers.acp.loading").into_owned()),
            );
        }
        for agent in &market {
            rows = rows.child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .child(self.render_market_row(agent, cx)),
            );
        }
        v_flex()
            .w_full()
            .gap_3()
            .child(Input::new(&self.search).small())
            .child(
                div()
                    .w_full()
                    .h(px(360.))
                    .overflow_y_scrollbar()
                    .rounded(material::radius_card())
                    .bg(cx.theme().muted)
                    .child(div().size_full().child(rows)),
            )
            .child(
                h_flex()
                    .id("acp-custom-open")
                    .w_full()
                    .pt_3()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(tcode_i18n::tr!("providers.acp.custom").into_owned()),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = PanelView::CustomAcp;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The provider entries shown above the ACP marketplace: Claude Code (opens
    /// the third-party endpoint form) and Codex (present but disabled for now).
    fn render_provider_entries(&self, cx: &mut Context<Self>) -> AnyElement {
        let entry = |id: &'static str,
                     glyph: Icon,
                     title: String,
                     subtitle: String,
                     enabled: bool,
                     cx: &mut Context<Self>|
         -> AnyElement {
            let muted = cx.theme().muted_foreground;
            let mut row = h_flex()
                .id(id)
                .w_full()
                .p_3()
                .gap_3()
                .items_center()
                .rounded(material::radius_card())
                .bg(cx.theme().muted)
                .child(glyph.text_color(if enabled {
                    cx.theme().foreground
                } else {
                    muted
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_medium()
                                .text_color(if enabled {
                                    cx.theme().foreground
                                } else {
                                    muted
                                })
                                .child(title),
                        )
                        .child(div().text_size(px(12.)).text_color(muted).child(subtitle)),
                );
            if enabled {
                row = row
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .child(Icon::new(IconName::ChevronRight).small().text_color(muted))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_third_party(TpPreset::Kimi, window, cx);
                    }));
            } else {
                row = row.child(
                    div()
                        .rounded_full()
                        .px_2()
                        .text_size(px(12.))
                        .bg(cx.theme().muted)
                        .text_color(muted)
                        .child(tcode_i18n::tr!("providers.third_party.soon").into_owned()),
                );
            }
            row.into_any_element()
        };

        v_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("providers.third_party.section").into_owned()),
            )
            .child(entry(
                "provider-entry-claude",
                crate::provider_card::provider_glyph(agent::ProviderKind::ClaudeCode).small(),
                tcode_i18n::tr!("providers.third_party.claude_title").into_owned(),
                tcode_i18n::tr!("providers.third_party.claude_subtitle").into_owned(),
                true,
                cx,
            ))
            .child(entry(
                "provider-entry-codex",
                crate::provider_card::provider_glyph(agent::ProviderKind::Codex).small(),
                tcode_i18n::tr!("providers.third_party.codex_title").into_owned(),
                tcode_i18n::tr!("providers.third_party.codex_subtitle").into_owned(),
                false,
                cx,
            ))
            .into_any_element()
    }

    /// The third-party Claude Code endpoint form (Kimi preset or fully custom).
    fn render_third_party(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let preset_tab = |this: &Self,
                          id: &'static str,
                          label: String,
                          preset: TpPreset,
                          cx: &mut Context<Self>|
         -> AnyElement {
            let active = this.tp_preset == preset;
            h_flex()
                .id(id)
                .px_3()
                .py_1p5()
                .rounded(material::radius_button())
                .cursor_pointer()
                .when(active, |s| s.bg(cx.theme().accent).font_medium())
                .hover(|s| s.bg(cx.theme().accent))
                .child(div().text_size(px(13.)).child(label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_third_party(preset, window, cx);
                }))
                .into_any_element()
        };
        let field = |label: String, control: AnyElement| -> AnyElement {
            v_flex()
                .w_full()
                .gap_1()
                .child(div().text_size(px(12.)).font_medium().child(label))
                .child(control)
                .into_any_element()
        };
        let is_kimi = self.tp_preset == TpPreset::Kimi;

        v_flex()
            .w_full()
            .gap_3()
            .child(
                Button::new("tp-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .label(tcode_i18n::tr!("settings.back").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = PanelView::Home;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(preset_tab(
                        self,
                        "tp-preset-kimi",
                        KIMI_NAME.to_string(),
                        TpPreset::Kimi,
                        cx,
                    ))
                    .child(preset_tab(
                        self,
                        "tp-preset-custom",
                        tcode_i18n::tr!("providers.acp.custom").into_owned(),
                        TpPreset::Custom,
                        cx,
                    )),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(if is_kimi {
                        tcode_i18n::tr!("providers.third_party.kimi_help").into_owned()
                    } else {
                        tcode_i18n::tr!("providers.third_party.custom_help").into_owned()
                    }),
            )
            .child(field(
                tcode_i18n::tr!("providers.third_party.name").into_owned(),
                Input::new(&self.tp_name).xsmall().into_any_element(),
            ))
            .child(field(
                tcode_i18n::tr!("providers.third_party.base_url").into_owned(),
                Input::new(&self.tp_base_url).xsmall().into_any_element(),
            ))
            .child(field(
                tcode_i18n::tr!("providers.third_party.model").into_owned(),
                Input::new(&self.tp_model).xsmall().into_any_element(),
            ))
            // The API key is the point of the whole flow — give it prominence.
            .child(field(
                tcode_i18n::tr!("providers.third_party.key").into_owned(),
                Input::new(&self.tp_key).small().into_any_element(),
            ))
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("tp-cancel")
                            .ghost()
                            .xsmall()
                            .label(tcode_i18n::tr!("settings.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("tp-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(tcode_i18n::tr!("providers.third_party.connect").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = this.tp_name.read(cx).value().to_string();
                                let base_url = this.tp_base_url.read(cx).value().to_string();
                                let model = this.tp_model.read(cx).value().to_string();
                                let key = this.tp_key.read(cx).value().to_string();
                                // Endpoint + key are required; the rest can default.
                                if base_url.trim().is_empty() || key.trim().is_empty() {
                                    return;
                                }
                                this.app_state.update(cx, |state, cx| {
                                    state.create_third_party_profile(
                                        &name,
                                        &base_url,
                                        Some(&model),
                                        &key,
                                        cx,
                                    );
                                });
                                this.view = PanelView::Home;
                                window.close_dialog(cx);
                                cx.notify();
                            })),
                    ),
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
                    .label(tcode_i18n::tr!("settings.back").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = PanelView::Home;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_medium()
                    .child(tcode_i18n::tr!("providers.acp.custom").into_owned()),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(tcode_i18n::tr!("providers.acp.custom_help").into_owned()),
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
                            .label(tcode_i18n::tr!("settings.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("acp-custom-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(tcode_i18n::tr!("providers.acp.add").into_owned())
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
                                this.view = PanelView::Home;
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
        v_flex().w_full().child(match self.view {
            PanelView::Home => v_flex()
                .w_full()
                .gap_3()
                .child(self.render_provider_entries(cx))
                .child(self.render_marketplace(cx))
                .into_any_element(),
            PanelView::ThirdParty => self.render_third_party(cx),
            PanelView::CustomAcp => self.render_custom(cx),
        })
    }
}

fn launch_summary(agent: &InstalledAcpAgent) -> String {
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
        let npx = InstalledAcpAgent {
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
