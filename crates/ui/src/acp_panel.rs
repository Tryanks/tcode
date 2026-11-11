//! The Add Harness dialog: the ACP agent registry and a custom ACP agent.

use crate::overlay::{DialogActions, OverlayExt as _};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};
use crate::widgets::scrollbar::Scrollbar;
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Render, Styled as _, Subscription,
    Window, div, list, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::rc::Rc;

use tcode_protocol::AcpMarketplaceItem;

use crate::material;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// Which screen the Add Harness dialog is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelView {
    /// The ACP agent marketplace.
    Home,
    /// The custom ACP agent form.
    CustomAcp,
}

/// Long-lived state for the Add agent dialog.
pub struct AcpPanel {
    store: Entity<WorkspaceStore>,
    view: PanelView,
    search: Entity<InputState>,
    custom_name: Entity<InputState>,
    custom_command: Entity<InputState>,
    custom_args: Entity<InputState>,
    /// Measured rows stay valid while the filtered agents keep their ids. The
    /// registry is small enough to measure whole, so the scrollbar and the
    /// touch edge bounce see its full extent.
    market_list: ListState,
    market_ids: Vec<String>,
    /// Agents a search keystroke brings in and takes away, by id, drawn from
    /// what they were as they fold.
    market_rows: crate::motion::list_rows::ListRows<(), AcpMarketplaceItem>,
    market_flip: crate::motion::ListFlip,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AcpPanel {
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()))
        };
        let search = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            observe_store_topics(&store, &[TopicKind::Providers], cx),
            cx.observe(&search, |_, _, cx| cx.notify()),
        ];
        let panel = Self {
            store,
            view: PanelView::Home,
            search,
            custom_name: input(&crate::tr!("providers.acp.name_placeholder"), window, cx),
            custom_command: input("node", window, cx),
            custom_args: input("/path/to/agent.js --acp", window, cx),
            market_list: ListState::new(0, ListAlignment::Top, px(120.)).measure_all(),
            market_rows: crate::motion::list_rows::ListRows::over(crate::motion::KEYSTROKE),
            market_flip: Default::default(),
            market_ids: Vec::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        // The registry is the host owner's: a link whose scope is not known
        // yet is not asked, nor is a space, which the host would refuse.
        panel.store.update(cx, |store, _cx| {
            if store.can_manage_host() {
                store.refresh_acp_registry();
            }
        });
        panel
    }

    pub fn prepare_to_open(&mut self, cx: &mut Context<Self>) {
        self.view = PanelView::Home;
        cx.notify();
    }

    /// Shows `view`. The page being left stays drawn while it fades, so focus
    /// leaves it for the panel.
    fn navigate(&mut self, view: PanelView, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != view {
            self.view = view;
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    fn render_market_row(
        &self,
        agent: &AcpMarketplaceItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = agent.id.clone();
        let state = crate::motion::CrossFade::new(
            gpui::SharedString::from(format!("acp-install-state-{id}")),
            if agent.installed {
                0
            } else if !agent.supported {
                1
            } else {
                2
            },
            3,
            CHANGE,
            window,
            cx,
        );
        let mut layers = Vec::new();
        if state.shows(0) {
            layers.push((
                0,
                material::semantic_chip(
                    crate::tr!("providers.acp.installed").into_owned(),
                    cx.theme().success.opacity(0.12),
                    cx.theme().success_foreground,
                    cx,
                )
                .into_any_element(),
            ));
        }
        if state.shows(1) {
            layers.push((
                1,
                material::semantic_chip(
                    crate::tr!("providers.acp.unsupported").into_owned(),
                    cx.theme().muted,
                    cx.theme().muted_foreground,
                    cx,
                )
                .into_any_element(),
            ));
        }
        if state.shows(2) {
            layers.push((
                2,
                Button::new(gpui::SharedString::from(format!(
                    "acp-install-{}",
                    agent.id
                )))
                .outline()
                .xsmall()
                .loading(agent.installing)
                .label(crate::tr!("providers.acp.install").into_owned())
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |store, _cx| {
                            store.install_acp_agent(id);
                        });
                    }
                }))
                .into_any_element(),
            ));
        }
        material::group_row(cx)
            .id(gpui::SharedString::from(format!("acp-market-row-{id}")))
            .items_start()
            .relative()
            .child(crate::material::row_hover(gpui::px(0.), cx))
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
                            .child(div().text_title(cx).child(agent.name.clone()))
                            .when(!agent.version.is_empty(), |row| {
                                row.child(
                                    div()
                                        .text_meta_mono(false, cx)
                                        .child(format!("v{}", agent.version)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(agent.description.clone()),
                    ),
            )
            .child(state.render(layers).flex_none())
            .into_any_element()
    }

    fn render_marketplace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
        let row_key = |id: &str| {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut hasher);
            hasher.finish()
        };
        let keys: Rc<[u64]> = market.iter().map(|agent| row_key(&agent.id)).collect();
        if !market.iter().map(|agent| &agent.id).eq(&self.market_ids) {
            // The registry arriving is in place at once; a keystroke after
            // it folds agents in and out.
            if !self.market_ids.is_empty() {
                let old_keys: Vec<u64> = self.market_ids.iter().map(|id| row_key(id)).collect();
                let everything = self.store.read(cx).acp_marketplace_items();
                let old_items: Vec<Option<AcpMarketplaceItem>> = self
                    .market_ids
                    .iter()
                    .map(|id| everything.iter().find(|agent| agent.id == *id).cloned())
                    .collect();
                self.market_flip.replace(
                    &self.market_list,
                    |index| old_keys.get(index).copied(),
                    &keys,
                    window,
                );
                self.market_rows.replace(
                    &old_keys,
                    &keys,
                    |_, _| Some(()),
                    |index| old_items.get(index).cloned().flatten(),
                    cx,
                );
            }
            self.market_ids = market.iter().map(|agent| agent.id.clone()).collect();
            self.market_list.reset(market.len());
        }
        self.market_rows
            .settle_list(&self.market_list, &keys, window, cx);
        let error = self.store.read(cx).acp_registry_error();
        let loading = self.store.read(cx).acp_registry_loading();
        let empty = market.is_empty();
        let error = error.filter(|_| empty);
        let state = crate::motion::CrossFade::new(
            "acp-market-state",
            if error.is_some() {
                0
            } else if empty && loading {
                1
            } else {
                2
            },
            3,
            CHANGE,
            window,
            cx,
        )
        .fill();
        let error = crate::motion::shown("acp-market-error", error, SHOW, window, cx)
            .map(|error| error.value);
        let mut layers = Vec::new();
        if let Some(error) = error.filter(|_| state.shows(0)) {
            layers.push((
                0,
                material::empty_row(error, cx)
                    .text_color(cx.theme().danger_foreground)
                    .into_any_element(),
            ));
        }
        if state.shows(1) {
            layers.push((
                1,
                material::empty_row(crate::tr!("providers.acp.loading").into_owned(), cx)
                    .into_any_element(),
            ));
        }
        if state.shows(2) {
            layers.push((
                2,
                crate::scroll::page_viewport(
                    "acp-market-bounce",
                    &self.market_list,
                    list(
                        self.market_list.clone(),
                        cx.processor(move |this, ix: usize, window, cx| {
                            let row = this.render_market_row(&market[ix], window, cx);
                            let key = keys[ix];
                            let around = this.market_rows.around(key, cx);
                            let mut left: Vec<(String, AnyElement)> = around
                                .above
                                .iter()
                                .chain(&around.below)
                                .map(|(_, agent, _)| {
                                    (agent.id.clone(), this.render_market_row(agent, window, cx))
                                })
                                .collect();
                            let row = around.draw(
                                "acp-market-row",
                                key,
                                crate::motion::list_rows::Glide::Rest,
                                row,
                                |agent, _| {
                                    left.iter()
                                        .position(|(id, _)| *id == agent.id)
                                        .map(|at| left.remove(at).1)
                                        .unwrap_or_else(|| gpui::Empty.into_any_element())
                                },
                                window,
                            );
                            this.market_flip.row(ix, ("acp-market-row", key), row)
                        }),
                    )
                    .size_full(),
                )
                .into_any_element(),
            ));
        }
        let rows = state.render(layers).size_full();
        v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(Input::new(&self.search).placeholder(crate::tr!("providers.acp.search")))
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
                h_flex().w_full().child(
                    Button::new("acp-custom-open")
                        .debug_selector(|| "acp-custom-open".into())
                        .ghost()
                        .small()
                        .icon(IconName::Plus)
                        .label(crate::tr!("providers.acp.custom").into_owned())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(PanelView::CustomAcp, window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_custom(&self, cx: &mut Context<Self>) -> AnyElement {
        let (name, command, args) = (
            self.custom_name.clone(),
            self.custom_command.clone(),
            self.custom_args.clone(),
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
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.navigate(PanelView::Home, window, cx);
                    })),
            )
            .child(
                div()
                    .text_title(cx)
                    .child(crate::tr!("providers.acp.custom").into_owned()),
            )
            .child(
                div()
                    .text_secondary(false, cx)
                    .child(crate::tr!("providers.acp.custom_help").into_owned()),
            )
            .child(Input::new(&self.custom_name))
            .child(Input::new(&self.custom_command))
            .child(Input::new(&self.custom_args))
            .child(
                DialogActions::new()
                    .w_full()
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
                                this.store.update(cx, |store, _cx| {
                                    store.add_custom_acp_agent(name, command, args);
                                });
                                window.close_dialog(cx);
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AcpPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        const PAGES: [PanelView; 2] = [PanelView::Home, PanelView::CustomAcp];
        let current = PAGES.iter().position(|page| *page == self.view);
        let pages = crate::motion::CrossFade::new(
            "acp-panel-page",
            current.unwrap_or_default(),
            PAGES.len(),
            CHANGE,
            window,
            cx,
        )
        .fill();
        let mut layers = Vec::new();
        for (index, page) in PAGES.into_iter().enumerate() {
            if !pages.shows(index) {
                continue;
            }
            layers.push((
                index,
                match page {
                    PanelView::Home => v_flex()
                        .size_full()
                        .min_h_0()
                        .child(self.render_marketplace(window, cx))
                        .into_any_element(),
                    PanelView::CustomAcp => self.render_custom(cx),
                },
            ));
        }
        v_flex()
            .size_full()
            .min_h_0()
            .track_focus(&self.focus_handle)
            .child(pages.render(layers).size_full())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// A store holding one installed agent, built while Simplified Chinese is
    /// current.
    fn store_with_agent(cx: &mut TestAppContext) -> (Entity<WorkspaceStore>, gpui::Task<()>) {
        use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};

        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::theme::init);
        let agent = tcode_core::acp::InstalledAcpAgent {
            id: "agent".into(),
            name: "Agent".into(),
            version: "1.0.0".into(),
            icon: None,
            launch: agent::AcpLaunch::Custom {
                command: "agent".into(),
                args: Vec::new(),
                env: Vec::new(),
            },
            enabled: true,
            env: Vec::new(),
            launch_args: None,
        };
        let mut settings = crate::settings::Settings::default();
        settings.acp_agents.insert(agent.id.clone(), agent);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let pump = cx.background_executor.spawn(async move {
            pump_link.pump().await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        incoming
            .try_send(
                encode_line(&HostMessage::Event(EventEnvelope {
                    request_id: None,
                    topic: Topic::Settings,
                    event: ServerEvent::SettingsSnapshot(settings),
                }))
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        cx.run_until_parked();
        (store, pump)
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_search_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (store, _pump) = store_with_agent(cx);
        let (panel, cx) = cx.add_window_view(|window, cx| AcpPanel::new(store, window, cx));
        crate::settings::assert_follows_language_switch(cx, "providers.acp.search", |cx| {
            panel.read_with(cx, |panel, cx| {
                panel
                    .search
                    .read(cx)
                    .presentation()
                    .placeholder()
                    .to_string()
            })
        });
    }
}
