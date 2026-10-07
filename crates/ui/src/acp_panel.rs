//! First-class ACP provider cards and the modal agent marketplace.

use crate::overlay::OverlayExt as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::switch::Switch;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ListAlignment, ListState, ParentElement as _, Render, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{Scrollbar, StyledExt as _, h_flex, v_flex};
use std::rc::Rc;

use tcode_core::acp::InstalledAcpAgent;
use tcode_protocol::AcpMarketplaceItem;

use crate::material;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

/// One installed ACP agent, rendered with the same anatomy as a native provider card.
pub struct AcpAgentCard {
    store: Entity<WorkspaceStore>,
    agent_id: String,
    expanded: bool,
    args: Entity<InputState>,
    env: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl AcpAgentCard {
    pub fn new(
        store: Entity<WorkspaceStore>,
        agent: &InstalledAcpAgent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let args = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(crate::tr!("providers.acp.args_hint"));
            input.set_value(agent.launch_args.clone().unwrap_or_default(), window, cx);
            input
        });
        let env = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder(crate::tr!("providers.acp.env_hint"));
            input.set_value(format_env(&agent.env), window, cx);
            input
        });
        let subscriptions = vec![observe_store_topics(&store, &[TopicKind::Settings], cx)];
        Self {
            store,
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
                    .tooltip(crate::tr!("providers.toggle_details", name = name.clone()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new(gpui::SharedString::from(format!("acp-enable-{id}")))
                    .checked(agent.enabled)
                    .tooltip(crate::tr!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let (id, checked) = (toggle_id.clone(), *checked);
                        this.store.update(cx, |store, _cx| {
                            store.update_acp_agent(
                                id,
                                tcode_core::acp::AcpAgentPatch::SetEnabled { enabled: checked },
                            );
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
                crate::tr!("providers.acp.args").into_owned(),
                Input::new(&self.args).xsmall().into_any_element(),
                cx,
            ))
            .child(
                self.field_block(
                    crate::tr!("providers.acp.env").into_owned(),
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
                                .label(crate::tr!("providers.acp.save").into_owned())
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        let launch_args = args.read(cx).value().trim().to_string();
                                        let parsed_env = parse_env(&env.read(cx).value());
                                        let id = save_id.clone();
                                        this.store.update(cx, |store, _cx| {
                                            store.update_acp_agent(
                                                id,
                                                tcode_core::acp::AcpAgentPatch::SetLaunchOptions {
                                                    launch_args: (!launch_args.is_empty())
                                                        .then_some(launch_args),
                                                    env: parsed_env,
                                                },
                                            );
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
                        .label(crate::tr!("providers.acp.remove").into_owned())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = remove_id.clone();
                            this.store.update(cx, |store, _cx| {
                                store.remove_acp_agent(id);
                            });
                        })),
                ),
            )
            .into_any_element()
    }
}

impl Render for AcpAgentCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let agent = self.store.read(cx).installed_acp_agent(&self.agent_id);
        v_flex()
            .w_full()
            .rounded(material::radius_card(cx))
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
    store: Entity<WorkspaceStore>,
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
    /// Measured rows stay valid while the filtered agents keep their ids. The
    /// registry is small enough to measure whole, so the scrollbar and the
    /// touch edge bounce see its full extent.
    market_list: ListState,
    market_ids: Vec<String>,
    _subscriptions: Vec<Subscription>,
}

impl AcpPanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()))
        };
        let search = input(&crate::tr!("providers.acp.search"), window, cx);
        let subscriptions = vec![
            observe_store_topics(&store, &[TopicKind::Providers], cx),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        let panel = Self {
            store,
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
            tp_key: input(&crate::tr!("providers.third_party.key_hint"), window, cx),
            market_list: ListState::new(0, ListAlignment::Top, px(120.)).measure_all(),
            market_ids: Vec::new(),
            _subscriptions: subscriptions,
        };
        panel
            .store
            .update(cx, |store, _cx| store.refresh_acp_registry());
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
            .debug_selector({
                let id = id.clone();
                move || format!("acp-market-row-{id}")
            })
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
                    .child(crate::tr!("providers.acp.installed").into_owned())
                    .into_any_element()
            } else if !agent.supported {
                div()
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("providers.acp.unsupported").into_owned())
                    .into_any_element()
            } else {
                Button::new(gpui::SharedString::from(format!(
                    "acp-install-{}",
                    agent.id
                )))
                .outline()
                .xsmall()
                .loading(agent.installing)
                .label(crate::tr!("providers.acp.install").into_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let id = id.clone();
                    this.store.update(cx, |store, _cx| {
                        store.install_acp_agent(id);
                    });
                }))
                .into_any_element()
            })
            .into_any_element()
    }

    fn render_marketplace(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let market: Rc<Vec<AcpMarketplaceItem>> = Rc::new(
            self.store
                .read(cx)
                .acp_marketplace_items()
                .into_iter()
                .filter(|agent| {
                    query.is_empty()
                        || agent.name.to_lowercase().contains(&query)
                        || agent.id.to_lowercase().contains(&query)
                        || agent.description.to_lowercase().contains(&query)
                })
                .collect(),
        );
        if !market.iter().map(|agent| &agent.id).eq(&self.market_ids) {
            self.market_ids = market.iter().map(|agent| agent.id.clone()).collect();
            self.market_list.reset(market.len());
        }
        let error = self.store.read(cx).acp_registry_error();
        let loading = self.store.read(cx).acp_registry_loading();
        let empty = market.is_empty();
        let rows = if let Some(error) = error.filter(|_| empty) {
            div()
                .p_3()
                .text_size(px(13.))
                .text_color(cx.theme().danger_foreground)
                .child(error)
                .into_any_element()
        } else if empty && loading {
            div()
                .p_3()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("providers.acp.loading").into_owned())
                .into_any_element()
        } else {
            crate::scroll::page_viewport(
                "acp-market-bounce",
                crate::wheel_easing::Handle::List(self.market_list.clone()),
                list(
                    self.market_list.clone(),
                    cx.processor(move |this, ix: usize, _, cx| {
                        this.render_market_row(&market[ix], cx)
                    }),
                )
                .size_full(),
            )
            .into_any_element()
        };
        v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(Input::new(&self.search).small())
            .child(
                v_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .rounded(material::radius_card(cx))
                    .bg(cx.theme().muted)
                    .child(rows)
                    .when(!window.is_inspector_picking(cx), |rows| {
                        rows.child(
                            Scrollbar::vertical(&self.market_list).id("acp-market-scrollbar"),
                        )
                    }),
            )
            .child(
                h_flex()
                    .id("acp-custom-open")
                    .debug_selector(|| "acp-custom-open".into())
                    .w_full()
                    .pt_3()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .child(Icon::new(IconName::Plus).text_color(cx.theme().muted_foreground))
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(crate::tr!("providers.acp.custom").into_owned()),
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
                .rounded(material::radius_card(cx))
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
                        .child(crate::tr!("providers.third_party.soon").into_owned()),
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
                    .child(crate::tr!("providers.third_party.section").into_owned()),
            )
            .child(entry(
                "provider-entry-claude",
                crate::provider_card::provider_glyph(agent::ProviderKind::ClaudeCode).small(),
                crate::tr!("providers.third_party.claude_title").into_owned(),
                crate::tr!("providers.third_party.claude_subtitle").into_owned(),
                true,
                cx,
            ))
            .child(entry(
                "provider-entry-codex",
                crate::provider_card::provider_glyph(agent::ProviderKind::Codex).small(),
                crate::tr!("providers.third_party.codex_title").into_owned(),
                crate::tr!("providers.third_party.codex_subtitle").into_owned(),
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
                .rounded(material::radius_button(cx))
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
                    .label(crate::tr!("settings.back").into_owned())
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
                        crate::tr!("providers.acp.custom").into_owned(),
                        TpPreset::Custom,
                        cx,
                    )),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(if is_kimi {
                        crate::tr!("providers.third_party.kimi_help").into_owned()
                    } else {
                        crate::tr!("providers.third_party.custom_help").into_owned()
                    }),
            )
            .child(field(
                crate::tr!("providers.third_party.name").into_owned(),
                Input::new(&self.tp_name).xsmall().into_any_element(),
            ))
            .child(field(
                crate::tr!("providers.third_party.base_url").into_owned(),
                Input::new(&self.tp_base_url).xsmall().into_any_element(),
            ))
            .child(field(
                crate::tr!("providers.third_party.model").into_owned(),
                Input::new(&self.tp_model).xsmall().into_any_element(),
            ))
            .child(field(
                crate::tr!("providers.third_party.key").into_owned(),
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
                            .label(crate::tr!("settings.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("tp-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(crate::tr!("providers.third_party.connect").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = this.tp_name.read(cx).value().to_string();
                                let base_url = this.tp_base_url.read(cx).value().to_string();
                                let model = this.tp_model.read(cx).value().to_string();
                                let key = this.tp_key.read(cx).value().to_string();
                                // Endpoint + key are required; the rest can default.
                                if base_url.trim().is_empty() || key.trim().is_empty() {
                                    return;
                                }
                                this.store.update(cx, |store, _cx| {
                                    store.create_third_party_profile(
                                        name,
                                        base_url,
                                        Some(model),
                                        key,
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
                    .label(crate::tr!("settings.back").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = PanelView::Home;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_medium()
                    .child(crate::tr!("providers.acp.custom").into_owned()),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("providers.acp.custom_help").into_owned()),
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
                            .label(crate::tr!("settings.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("acp-custom-add")
                            .with_variant(ButtonVariant::Primary)
                            .xsmall()
                            .label(crate::tr!("providers.acp.add").into_owned())
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
                                this.store.update(cx, |store, _cx| {
                                    store.add_custom_acp_agent(name, command, args, env);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().min_h_0().child(match self.view {
            PanelView::Home => v_flex()
                .size_full()
                .min_h_0()
                .gap_3()
                .child(self.render_provider_entries(cx))
                .child(self.render_marketplace(window, cx))
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
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_marketplace_paints_only_the_visible_agents(cx: &mut TestAppContext) {
        use tcode_protocol::{
            EventEnvelope, HostMessage, ProvidersStatus, ServerEvent, Topic, encode_line,
        };

        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let acp_marketplace_items = (0..50)
            .map(|index| AcpMarketplaceItem {
                id: format!("agent-{index}"),
                name: format!("Agent {index}"),
                version: "1.0.0".into(),
                description: "An agent from the ACP registry".into(),
                installed: false,
                installing: false,
                supported: true,
            })
            .collect();
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Providers,
                    event: ServerEvent::ProvidersReplaced(ProvidersStatus {
                        acp_marketplace_items,
                        ..Default::default()
                    }),
                }))
                .unwrap(),
            )
            .unwrap();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let (_, cx) = cx.add_window_view(|window, cx| AcpPanel::new(store.clone(), window, cx));
        cx.simulate_resize(gpui::size(px(588.), px(456.)));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("acp-market-row-agent-0").is_some());
        assert!(
            cx.debug_bounds("acp-market-row-agent-45").is_none(),
            "the marketplace must not paint every registry agent"
        );

        let position = cx.debug_bounds("acp-market-row-agent-0").unwrap().center();
        for _ in 0..10 {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position,
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-1000.))),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds("acp-market-row-agent-49").is_some());
        assert!(cx.debug_bounds("acp-market-row-agent-0").is_none());
    }

    #[test]
    fn env_shorthand_preserves_values_and_ignores_entries_without_keys() {
        let pairs = parse_env("  API_KEY=abc=def  BASE_URL=https://x/y  EMPTY= =ignored bogus ");
        assert_eq!(
            pairs,
            vec![
                ("API_KEY".to_string(), "abc=def".to_string()),
                ("BASE_URL".to_string(), "https://x/y".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
        assert_eq!(
            format_env(&pairs),
            "API_KEY=abc=def BASE_URL=https://x/y EMPTY="
        );
    }
}
