use super::*;
use crate::widgets::TooltipTrigger as _;

/// How long a value drawn here takes to glide to a new one.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// Show or hide one of the panels a thread's header toggles, as its button
/// or its row in the header's overflow menu does.
#[derive(Action, Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_chat, no_json)]
pub(super) enum TogglePanel {
    Terminal,
    Plan,
    Agents,
    Preview,
    Diff,
    PullRequests,
}

/// Whether each panel the header toggles is showing; `None` for one this
/// thread does not offer.
#[derive(Clone, Copy)]
struct HeaderPanels {
    terminal: bool,
    plan: bool,
    agents: Option<bool>,
    preview: Option<bool>,
    diff: bool,
    pull_requests: Option<bool>,
}

/// Below this header width the git and Open split buttons drop their text and
/// keep their icon and chevron. The header with its longest git label, five
/// panel toggles and a title kept to a few words beside a "+10" pull request
/// badge measures under 672pt in English, the widest shipped locale, and the
/// pane menu adds 28pt. [`header_extra_width_for`] adds what a thread's header holds beyond
/// that.
const HEADER_LABELS_MIN_WIDTH: f32 = 700.;

/// At 16pt padding the compact header fits: that title and badge, the git
/// and Open split buttons as icon and chevron, and five panel toggles measure
/// 531pt; the pane menu adds its 24pt button and 4pt gap. Below it the panel
/// toggles, the pull request badge, the Open button and the pane menu fold
/// into one overflow menu.
pub(crate) const HEADER_COMPACT_MIN_WIDTH: f32 = 560.;

/// At 8pt padding a side the overflow header fits: the sidebar toggle, the title's
/// 168pt stretch, the git split button as icon and chevron, and the overflow
/// button measure 295pt. It holds no text but the title, so it measures the
/// same in every locale.
pub(crate) const HEADER_OVERFLOW_MIN_WIDTH: f32 = 300.;

/// Among several panes every tier also holds the pane's close button at its
/// right end: a 24pt button and the 4pt gap before it.
pub(crate) const PANE_CLOSE_WIDTH: f32 = 28.;

/// How far a narrow pane's title tint stops short of the header's top and
/// bottom edges.
const PANE_TINT_INSET: f32 = 10.;

/// The way back to the parent thread, each with the 8pt gap that follows it.
/// Its arrow alone measures 20pt. With the parent's title cut to its
/// 24-character maximum it measured 215pt for an ordinary title; 265pt leaves
/// room for a title of wide letters.
const PARENT_LINK_COMPACT_WIDTH: f32 = 28.;

const PARENT_LINK_LABELLED_WIDTH: f32 = 273.;

/// The thread `session_id`'s was reached from: its id and title.
fn parent_session(store: &WorkspaceStore, session_id: Option<&str>) -> Option<(String, String)> {
    let active = session_id?;
    let sessions = store.sidebar_sessions();
    let parent_id = sessions
        .iter()
        .find(|meta| meta.id == active)?
        .parent_session_id
        .clone()?;
    let parent = sessions.iter().find(|meta| meta.id == parent_id)?;
    Some((parent_id, parent.title.clone()))
}

/// The header's left inset in place of the bar's own. Collapsed, the sidebar
/// has zero width and the header starts at the window's left edge, where macOS
/// draws the traffic lights — but only when it draws them: they are hidden in
/// fullscreen, and other platforms never had them. The budget counts the whole
/// inset, not its excess over the bar's, which leaves the widths a little slack.
fn traffic_light_clearance(sidebar_collapsed: bool, fullscreen: bool) -> f32 {
    if cfg!(target_os = "macos") && sidebar_collapsed && !fullscreen {
        crate::window_caption::TRAFFIC_LIGHT_INSET
    } else {
        0.
    }
}

/// How much wider than the plain header `session_id`'s header is, with
/// its split buttons labelled or not: the padding that clears the macOS
/// traffic lights, and the parent link.
pub(crate) fn header_extra_width_for(
    store: &WorkspaceStore,
    session_id: Option<&str>,
    sidebar_collapsed: bool,
    fullscreen: bool,
    labelled: bool,
) -> f32 {
    let parent_link = match (parent_session(store, session_id), labelled) {
        (None, _) => 0.,
        (Some(_), true) => PARENT_LINK_LABELLED_WIDTH,
        (Some(_), false) => PARENT_LINK_COMPACT_WIDTH,
    };
    traffic_light_clearance(sidebar_collapsed, fullscreen) + parent_link
}

/// The header's pane commands, for the pane it heads.
fn pane_menu(
    panes: crate::shell::PaneControls,
    motion: &crate::motion::owned::Owned,
) -> impl IntoElement {
    Button::new("pane-menu")
        .motion(motion)
        .debug_selector(|| "pane-menu".into())
        .ghost()
        .small()
        .compact()
        .icon(IconName::Columns2)
        .tooltip(crate::tr!("panes.menu"))
        .dropdown_menu(move |menu, _, _| {
            pane_items(menu, panes).after_choose(crate::shell::focus_pane_composer)
        })
}

/// The agents' cue on the button that shows the Agents panel: it fades in
/// while an agent runs or waits to be settled, and changes colour between the
/// two, on the header's clock `motion`.
fn agents_cue_dot(
    cue: Option<gpui::Hsla>,
    motion: &crate::motion::owned::Owned,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let shown = motion.shown("agents-cue", cue, SHOW, window, cx)?;
    let color = motion.transition("agents-cue-color", shown.value, GLIDE, window, cx);
    Some(
        div()
            .absolute()
            .top(px(3.))
            .right(px(3.))
            .size(px(6.))
            .rounded_full()
            .bg(color)
            .opacity(shown.progress())
            .debug_selector(|| "agents-cue".into())
            .into_any_element(),
    )
}

/// The agents' cue inside the phone's Agents pill, widening it as it comes
/// and narrowing it as it goes; its colour glides between running and
/// waiting.
fn cue_in_pill(
    cue: Option<gpui::Hsla>,
    motion: &crate::motion::owned::Owned,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let shown = motion.shown("agents-pill-cue", cue, SHOW, window, cx)?;
    let color = motion.transition("agents-pill-cue-color", shown.value, GLIDE, window, cx);
    let gap = gpui::rems(0.25).to_pixels(window.rem_size());
    Some(shown.fold_across_gap(gap, div().size(px(6.)).rounded_full().bg(color)))
}

/// The pane commands `panes` allows, as menu rows.
fn pane_items(menu: PopupMenu, panes: crate::shell::PaneControls) -> PopupMenu {
    use crate::panes::{
        CloseOtherPanes, ClosePane, MoveThreadToPane, SplitDown, SplitRight, SwapPane, ZoomPane,
    };
    let (zoom, zoom_icon) = if panes.zoomed {
        (crate::tr!("panes.unzoom"), IconName::Minimize2)
    } else {
        (crate::tr!("panes.zoom"), IconName::Maximize2)
    };
    menu.menu_with_icon_and_enable(
        crate::tr!("panes.split_right").into_owned(),
        IconName::Columns2,
        Box::new(SplitRight),
        panes.split_right,
    )
    .menu_with_icon_and_enable(
        crate::tr!("panes.split_down").into_owned(),
        IconName::Rows2,
        Box::new(SplitDown),
        panes.split_down,
    )
    .separator()
    .menu_with_icon_and_enable(zoom.into_owned(), zoom_icon, Box::new(ZoomPane), panes.zoom)
    .menu_with_icon_and_enable(
        crate::tr!("panes.close").into_owned(),
        IconName::Close,
        Box::new(ClosePane),
        panes.close,
    )
    .menu_with_icon_and_enable(
        crate::tr!("panes.close_others").into_owned(),
        IconName::Close,
        Box::new(CloseOtherPanes),
        panes.close_others,
    )
    // Only the sides with a pane: four rows each that do nothing would
    // bury the ones that do.
    .when(panes.neighbours.contains(&true), |menu| {
        let swaps = panes.sides().fold(menu.separator(), |menu, side| {
            let swap = SwapPane(side);
            menu.menu_with_icon(swap.label().into_owned(), side.icon(), Box::new(swap))
        });
        panes
            .sides()
            .filter(|_| panes.moves)
            .fold(swaps, |menu, side| {
                let to = MoveThreadToPane(side);
                menu.menu_with_icon(to.label().into_owned(), side.icon(), Box::new(to))
            })
    })
}

/// A press on a control ends here: it does not arm the header's window drag,
/// and neither the control nor what holds it takes the keyboard.
pub(super) fn stop_press(_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut App) {
    cx.stop_propagation();
}

impl ChatView {
    /// The phone's row over the foot of the timeline: the Agents pill first,
    /// then the linked pull requests, scrolling sideways when they overflow.
    /// It floats over the chat instead of taking height from it, so each chip
    /// carries its own backing; the last row's padding clears it at the tail.
    /// It rises in with the first of them and sinks out, as it last stood,
    /// with the last; the second element is how far in it is.
    pub(super) fn render_phone_strip(
        &self,
        agents: Option<&crate::agents_panel::Agents>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, f32)> {
        let compact = self.window_state.read(cx).compact;
        let links = self
            .active_pull_requests(cx)
            .filter(|links| compact && !links.is_empty());
        let agents = agents
            .filter(|_| compact)
            .map(|agents| (agents.count(), agents.cue(cx)));
        let strip = self.motion.shown(
            "phone-strip",
            (agents.is_some() || links.is_some()).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        // Coming or going, the strip moves as a whole; while it stays, a
        // pill that comes or goes widens into it or narrows out of it, and
        // the other moves aside.
        let whole = strip.progress() < 1.;
        let gap = cx.theme().tokens.spacing.xs;
        let radius = crate::material::radius_chip(cx);
        let agents = self
            .motion
            .shown("phone-agents-pill-fold", agents, SHOW, window, cx)
            .map(|pill| {
                let (count, cue) = pill.value;
                let chip = floating_backing(radius, cx)
                    .child(self.render_agents_pill(count, cue, window, cx));
                if whole {
                    chip.into_any_element()
                } else {
                    pill.fold_across(chip)
                }
            });
        let pull_requests = self
            .motion
            .shown("phone-pr-pill-fold", links, SHOW, window, cx)
            .and_then(|pill| {
                let chip = floating_backing(radius, cx)
                    .child(self.render_pull_request_pill(&pill.value, cx)?);
                Some(if whole {
                    chip.into_any_element()
                } else {
                    pill.fold_across_gap(gap, chip)
                })
            });
        let progress = strip.progress();
        Some((
            crate::material::bar("phone-strip", crate::material::Bar::Toolbar, true, cx)
                .debug_selector(|| "phone-strip".into())
                .absolute()
                .bottom(-crate::motion::lift(progress))
                .left_0()
                .px_0()
                .opacity(progress)
                .child(
                    h_flex()
                        .id("phone-strip-scroll")
                        .size_full()
                        .items_center()
                        .px(px(crate::material::COMPACT_PAGE_INSET))
                        .gap(gap)
                        .children(agents)
                        .children(pull_requests)
                        .overflow_x_scroll_area(),
                )
                .when(strip.leaving(), |strip| {
                    strip.child(crate::motion::shield())
                })
                .into_any_element(),
            progress,
        ))
    }

    /// The phone entry to the Agents view: a pill that opens the sheet, shown
    /// while the thread has agents.
    fn render_agents_pill(
        &self,
        count: usize,
        cue: Option<gpui::Hsla>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label: SharedString = match count {
            1 => crate::tr!("agents.pill_one").into_owned(),
            count => crate::tr!("agents.pill", count = count).into_owned(),
        }
        .into();
        let panel = self.agents.clone();
        let shown = self.agents.downgrade();
        let pill = Button::new("agents-pill")
            .motion(&self.motion)
            .debug_selector(|| "agents-pill".into())
            .ghost()
            .outline()
            .chip()
            .text_body(false, cx)
            .icon(IconName::Bot)
            .label(label)
            // The cue widens the pill as it comes, and its label moves aside.
            .children(cue_in_pill(cue, &self.motion, window, cx));
        crate::widgets::Popover::new("agents-sheet")
            .bottom_sheet(crate::tr!("agents.title"))
            .on_open_change(move |open, _, cx| {
                // Opening an agent dismisses the sheet from inside
                // the panel's own update.
                let (shown, open) = (shown.clone(), *open);
                cx.defer(move |cx| {
                    let _ = shown.update(cx, |panel, cx| panel.set_shown(open, cx));
                });
            })
            .trigger_with(move |_, _, _, _| pill.into_any_element())
            .content(move |_, _, cx| {
                let sheet = cx.entity().downgrade();
                panel.update(cx, |panel, _| panel.in_sheet(sheet));
                div().w_full().child(panel.clone()).into_any_element()
            })
            .into_any_element()
    }

    /// The way back from an agent to the thread it was reached from. Unlabelled,
    /// it is its arrow, named by its tooltip and its accessibility label.
    fn parent_link(&self, labelled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (parent_id, title) =
            parent_session(self.workspace_store.read(cx), self.session_id.as_deref())?;
        let name: SharedString = crate::tr!("agents.back_to_parent", title = title)
            .into_owned()
            .into();
        Some(
            Button::new("agent-parent-link")
                .motion(&self.motion)
                .debug_selector(|| "agent-parent-link".into())
                .on_mouse_down(gpui::MouseButton::Left, stop_press)
                .ghost()
                .xsmall()
                .icon(IconName::ArrowLeft)
                // The parent's title fades rather than pushing this thread's
                // title out of the header.
                .when(labelled, |link| {
                    link.max_w(px(160.)).child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(title.clone())),
                    )
                })
                .aria_label(name.clone())
                .tooltip(name)
                .on_click({
                    let store = self.workspace_store.clone();
                    let agent = self.session_id.clone();
                    move |_, _, cx| {
                        crate::agents_panel::back_to_lead(
                            &store,
                            parent_id.clone(),
                            agent.clone(),
                            cx,
                        );
                    }
                })
                .into_any_element(),
        )
    }

    fn active_pull_requests(
        &self,
        cx: &App,
    ) -> Option<Vec<tcode_core::pull_request::ThreadPullRequestLink>> {
        let id = self.session_id.as_deref()?;
        Some(self.workspace_store.read(cx).pull_requests(id).to_vec())
    }

    /// The pull request page a phone pushes over the thread.
    pub fn pull_request_view(&self, cx: &App) -> Entity<crate::pull_requests::PullRequestView> {
        self.pull_requests.read(cx).detail()
    }

    /// The phone's entry to the linked pull requests: one chip naming every
    /// number, which opens them in a bottom sheet.
    fn render_pull_request_pill(
        &self,
        links: &[tcode_core::pull_request::ThreadPullRequestLink],
        cx: &App,
    ) -> Option<AnyElement> {
        let numbers = crate::pull_requests::numbers(links, cx)?;
        let label = crate::pull_requests::badge_label(links, cx)?;
        let panel = self.pull_requests.clone();
        Some(
            crate::widgets::Popover::new("phone-pull-requests")
                .bottom_sheet(crate::tr!("pull_requests.title").into_owned())
                .trigger(
                    Button::new("phone-pr-pill")
                        .motion(&self.motion)
                        .debug_selector(|| "phone-pr-pill".into())
                        .ghost()
                        .outline()
                        .chip()
                        .aria_label(label)
                        .child(numbers),
                )
                .content(move |_, _, _| panel.clone())
                .into_any_element(),
        )
    }

    pub(super) fn on_link_dialog(
        &mut self,
        _: &crate::pull_requests::OpenLinkDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.session_id.clone() {
            crate::pull_requests::open_link_dialog(self.workspace_store.clone(), id, window, cx);
        }
    }

    pub(super) fn on_move_thread_dialog(
        &mut self,
        _: &OpenMoveThreadDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.session_id.clone() {
            crate::move_thread_dialog::open(self.workspace_store.clone(), id, window, cx);
        }
    }

    pub(super) fn on_revoke_self_settle(
        &mut self,
        _: &RevokeSelfSettle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.session_id.clone() {
            self.workspace_store
                .update(cx, |store, _| store.revoke_self_settle(id));
        }
    }

    pub(super) fn on_show_agent_tools(
        &mut self,
        _: &crate::pull_requests::ShowAgentTools,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::pull_requests::open_agent_sees(
            self.workspace_store.clone(),
            self.session_id.clone(),
            window,
            cx,
        );
    }

    /// The header, laid out for the width it is given in this frame: whether
    /// its split buttons are labelled depends on that width.
    pub(super) fn render_header(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        agents: Option<&crate::agents_panel::Agents>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let agents_cue = agents.map(|agents| agents.cue(cx));
        let header = cx.processor(move |this, width: gpui::Pixels, window, cx| {
            this.render_header_bar(
                title.clone(),
                is_draft,
                cwd.clone(),
                agents_cue,
                f32::from(width),
                window,
                cx,
            )
        });
        gpui::container_query(move |size, window, cx| header(size.width, window, cx))
            .flex_none()
            .h(px(crate::material::bar_height(
                crate::material::Bar::Strip,
                false,
            )))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_header_bar(
        &self,
        title: Option<String>,
        is_draft: bool,
        cwd: Option<PathBuf>,
        agents_cue: Option<Option<gpui::Hsla>>,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = self.window_state.read(cx).sidebar_collapsed;
        // Windows: with no right panel open this header is the window's
        // top-right corner, so it hosts the caption buttons — flush to the
        // right edge, past the header's usual inset.
        let drawer = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.drawer_subject());
        let hosts_caption = self.edges.right
            && window_caption::hosts_caption_for_state(
                window_caption::CaptionSurface::Chat,
                self.window_state.read(cx).route(),
                drawer.right_panel_open,
                drawer.right_tab,
            );
        let under_traffic_lights = collapsed && self.edges.left;
        // The pane's close button holds the right end in every tier, so each
        // tier makes room for it while it is drawn.
        let close_width = if self.pane_close.is_some() {
            PANE_CLOSE_WIDTH
        } else {
            0.
        };
        let labelled_extra = header_extra_width_for(
            self.workspace_store.read(cx),
            self.session_id.as_deref(),
            under_traffic_lights,
            window.is_fullscreen(),
            true,
        ) + close_width;
        let labelled = width - labelled_extra >= HEADER_LABELS_MIN_WIDTH;
        let compact_extra = header_extra_width_for(
            self.workspace_store.read(cx),
            self.session_id.as_deref(),
            under_traffic_lights,
            window.is_fullscreen(),
            false,
        ) + close_width
            + if hosts_caption {
                window_caption::RIGHT_PANEL_CAPTION_WIDTH
            } else {
                0.
            };
        let overflow = width - compact_extra < HEADER_COMPACT_MIN_WIDTH;
        let traffic_lights = window_caption::traffic_light_inset(window);
        let motion = self.motion.clone();
        // Controls that come and go with what the thread has widen into
        // their rows, so the title and their neighbours move aside; the
        // badge also narrows out as the header folds into its overflow
        // tier, which has no room for it. The toggles are sampled in every
        // tier and fade with the tier that draws them. Their clocks are the
        // view's, so a pane re-parented by the dock carries them on.
        let badge = motion.shown(
            "header-pr-badge-fold",
            self.active_pull_requests(cx)
                .filter(|links| !links.is_empty() && !overflow),
            SHOW,
            window,
            cx,
        );
        // Among several panes, a header folded into its overflow tier tints
        // its title area with its pane's colour, fading with the tier in its
        // own colour.
        let tint = motion
            .shown(
                "pane-tint",
                self.session_id
                    .as_deref()
                    .and_then(|session_id| crate::shell::pane_colour(session_id, cx))
                    .filter(|_| overflow)
                    .map(|colour| colour.alpha(1.)),
                SHOW,
                window,
                cx,
            )
            .map(|tint| {
                let colour = motion.transition("pane-tint-colour", tint.value, GLIDE, window, cx);
                let progress = tint.progress();
                (crate::material::pane_tint(colour, progress, cx), progress)
            });
        let toggle_gap = gpui::rems(0.25).to_pixels(window.rem_size());
        let agents_toggle = motion.shown(
            "agents-toggle-fold",
            agents_cue.is_some().then_some(()),
            SHOW,
            window,
            cx,
        );
        let preview_toggle = motion.shown(
            "preview-toggle-fold",
            self.workspace_store.read(cx).offers_preview().then_some(()),
            SHOW,
            window,
            cx,
        );
        // The clearance for the traffic lights glides in and out as the
        // sidebar collapses or a pane moves to or off the window's left edge.
        let left_inset = self.motion.transition(
            "traffic-light-inset",
            if under_traffic_lights && traffic_lights > px(0.) {
                traffic_lights
            } else {
                cx.theme().tokens.spacing.sm
            },
            GLIDE,
            window,
            cx,
        );
        let base = crate::material::bar("chat-header", crate::material::Bar::Strip, false, cx)
            .pl(left_inset)
            .when(hosts_caption, |this| this.pr_0())
            .min_w(px(HEADER_OVERFLOW_MIN_WIDTH + compact_extra));

        // The sidebar toggle: the header's first control, immediately left of
        // the title. It lives here rather than in the sidebar because a
        // collapsed sidebar occupies no width at all (`crate::shell`), so a
        // control mounted inside it would have nowhere to be.
        let sidebar_toggle = Button::new("toggle-sidebar")
            .motion(&self.motion)
            .debug_selector(|| "toggle-sidebar".into())
            .on_mouse_down(gpui::MouseButton::Left, stop_press)
            .ghost()
            .small()
            .compact()
            .icon(if collapsed {
                IconName::PanelLeftOpen
            } else {
                IconName::PanelLeft
            })
            .tooltip(if collapsed {
                crate::tr!("sidebar.expand")
            } else {
                crate::tr!("sidebar.collapse")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                let workspace_store = this.workspace_store.clone();
                this.window_state.update(cx, |state, cx| {
                    state.toggle_sidebar_collapsed(&workspace_store, cx)
                });
            }));

        // An agent's header leads back to the thread it was reached from.
        let parent_link = self.labelled_swap(
            "parent-link-labels",
            labelled,
            window,
            cx,
            |this, labelled, cx| this.parent_link(labelled, cx),
        );

        // A draft shows a muted "New thread" label; an open thread its title;
        // nothing active shows "No active thread". The title stretch carries no
        // controls, so it doubles as the window's native drag handle where the
        // platform needs one.
        let on_tint = |text: gpui::Hsla| match tint {
            Some((tint, _)) => crate::material::text_over_tint(text, tint, cx),
            None => text,
        };
        // Beside the current pane the title recedes; the pane's content stays
        // as it is.
        let (muted, foreground) = (
            on_tint(cx.theme().muted_foreground),
            on_tint(motion.transition(
                "title-colour",
                if self.title_recedes {
                    cx.theme().muted_foreground
                } else {
                    cx.theme().foreground
                },
                GLIDE,
                window,
                cx,
            )),
        );
        let title_el = if is_draft {
            crate::material::bar_title(crate::tr!("chat.new_thread"), cx).text_color(muted)
        } else {
            match &title {
                Some(title) => crate::material::bar_title(title.clone(), cx)
                    .debug_selector(|| "chat-title".into())
                    .text_color(foreground)
                    // Keep a few words of the title even when the diff panel and
                    // the git/Open buttons squeeze the header; without a floor it
                    // collapses to a lone "I…".
                    .min_w(px(120.)),
                None => crate::material::bar_title(crate::tr!("chat.no_active_thread"), cx)
                    .text_color(muted),
            }
        };

        // The right-side cluster (Open split-button + panel toggles) shows for
        // any active thread, including a draft.
        let show_actions = is_draft || title.is_some();
        let panel = self
            .workspace_store
            .read(cx)
            .panel_state_for(self.session_id.as_deref());
        let right_panel_open = panel.right_panel_open;
        let right_tab = panel.right_tab;
        let plan_showing = right_panel_open && right_tab == RightTab::Plan;
        let agents_showing = right_panel_open && right_tab == RightTab::Agents;
        let preview_showing = right_panel_open && right_tab == RightTab::Preview;
        let terminal_open = panel.terminal_open && !self.window_state.read(cx).compact;
        let diff_showing = right_panel_open && right_tab == RightTab::Diff;
        let agent_tools = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref())
            .is_some_and(|status| status.injected_tools.is_some());
        let self_settles = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref())
            .and_then(|status| status.control.as_ref())
            .is_some_and(|control| control.self_settle.is_some());
        let movable = !is_draft && {
            let active = self.session_id.as_deref();
            self.workspace_store
                .read(cx)
                .sidebar_sessions()
                .iter()
                .any(|meta| Some(meta.id.as_str()) == active && meta.worktree.is_none())
        };
        let title_menu = {
            let title = title.clone();
            let cwd = cwd.clone();
            move |menu: PopupMenu, _: &mut Window, _: &mut Context<PopupMenu>| {
                let menu = menu.when_some(title.clone(), |menu, title| {
                    menu.menu(
                        crate::tr!("chat.copy_title").into_owned(),
                        Box::new(crate::widgets::menu::CopyText(title)),
                    )
                });
                let menu = menu
                    .separator()
                    .menu(
                        crate::tr!("pull_requests.link_menu").into_owned(),
                        Box::new(crate::pull_requests::OpenLinkDialog),
                    )
                    .when(movable, |menu| {
                        menu.menu(
                            crate::tr!("sidebar.ctx_move_thread").into_owned(),
                            Box::new(OpenMoveThreadDialog),
                        )
                    })
                    .when(self_settles, |menu| {
                        menu.menu(
                            crate::tr!("control.self_settle.revoke").into_owned(),
                            Box::new(RevokeSelfSettle),
                        )
                    })
                    .when(agent_tools, |menu| {
                        menu.menu(
                            crate::tr!("pull_requests.agent_sees").into_owned(),
                            Box::new(crate::pull_requests::ShowAgentTools),
                        )
                    });
                match &cwd {
                    Some(cwd) => menu.separator().path_items(&cwd.to_string_lossy(), None),
                    None => menu,
                }
            }
        };
        let header = window_drag_area("chat-header-drag", base, window, cx)
            .child(sidebar_toggle)
            .children(parent_link)
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(168.))
                    .h_full()
                    // The title keeps its floor; numbers past the stretch's end
                    // clip here rather than run under the git button.
                    .overflow_hidden()
                    .gap_2()
                    .items_center()
                    .relative()
                    .when_some(tint, |stretch, (tint, progress)| {
                        stretch.px(cx.theme().tokens.spacing.sm * progress).child(
                            div()
                                .debug_selector(|| "pane-tint".into())
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(PANE_TINT_INSET))
                                .bottom(px(PANE_TINT_INSET))
                                .rounded(crate::material::radius_input(cx))
                                .bg(tint),
                        )
                    })
                    .child(
                        // Sized by its text, not `bar_title`'s zero basis, so the title
                        // runs its full length and the badge follows it.
                        window_caption::drag_region(title_el.flex_initial())
                            .context_menu(title_menu),
                    )
                    .when_some(
                        badge.and_then(|badge| {
                            let numbers = crate::pull_requests::numbers(&badge.value, cx)?;
                            let label = crate::pull_requests::badge_label(&badge.value, cx)?;
                            Some((badge, numbers, label))
                        }),
                        |header, (badge, numbers, label)| {
                            // Every number shows until the numbers reach half the
                            // title's stretch; past that they clip, so the title
                            // keeps the other half however many are linked. A
                            // badge that comes or goes widens into the row.
                            let gap = gpui::rems(0.5).to_pixels(window.rem_size());
                            let button = Button::new("header-pr-badge")
                                .motion(&self.motion)
                                .debug_selector(|| "header-pr-badge".into())
                                .on_mouse_down(gpui::MouseButton::Left, stop_press)
                                .ghost()
                                .small()
                                .compact()
                                .selected(right_panel_open && right_tab == RightTab::PullRequests)
                                .tooltip(crate::tr!("pull_requests.toggle"))
                                .aria_label(label)
                                .child(numbers)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_toggle_panel(&TogglePanel::PullRequests, window, cx)
                                }));
                            // Another number linked widens the badge as its chip
                            // widens in (`pull_requests::numbers`).
                            header.child(
                                h_flex()
                                    .flex_none()
                                    .max_w(gpui::relative(0.5))
                                    .overflow_hidden()
                                    .ml(-gap * (1. - badge.progress()))
                                    .child(badge.fold_across(button)),
                            )
                        },
                    ),
            )
            .when(show_actions, |this| {
                this.children(self.labelled_swap(
                    "git-labels",
                    labelled && !overflow,
                    window,
                    cx,
                    |this, labelled, cx| this.render_git_button(labelled, cx),
                ))
            });
        // A change of tier cross-fades the controls after the git button:
        // the panel toggles, the Open button and the pane menu give way to
        // the overflow button and back, and the title's stretch narrows or
        // widens with them.
        let tier = show_actions.then(|| {
            let swap = motion
                .swap("header-tier", usize::from(overflow), 2, CHANGE, window, cx)
                .across();
            let mut layers = Vec::new();
            if swap.shows(1) {
                layers.push((
                    1,
                    self.render_header_overflow(
                        HeaderPanels {
                            terminal: terminal_open,
                            plan: plan_showing,
                            agents: agents_cue.map(|_| agents_showing),
                            preview: self
                                .workspace_store
                                .read(cx)
                                .offers_preview()
                                .then_some(preview_showing),
                            diff: diff_showing,
                            pull_requests: self
                                .active_pull_requests(cx)
                                .and_then(|links| crate::pull_requests::badge_label(&links, cx))
                                .map(|_| right_panel_open && right_tab == RightTab::PullRequests),
                        },
                        agents_cue_dot(agents_cue.flatten(), &motion, window, cx),
                        cwd.clone(),
                        window,
                        cx,
                    ),
                ));
            }
            if swap.shows(0) {
                let mut cue = agents_cue_dot(agents_cue.flatten(), &motion, window, cx);
                let open = cwd.clone().and_then(|cwd| {
                    self.labelled_swap(
                        "open-labels",
                        labelled,
                        window,
                        cx,
                        move |this, labelled, cx| {
                            Some(this.render_open_button(cwd.clone(), labelled, cx))
                        },
                    )
                });
                layers.push((
                    0,
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(cx.theme().tokens.spacing.xs)
                        .children(open)
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1()
                                .child(
                                    Button::new("panel-layout")
                                        .motion(&self.motion)
                                        .on_mouse_down(gpui::MouseButton::Left, stop_press)
                                        .ghost()
                                        .small()
                                        .compact()
                                        .icon(IconName::PanelBottom)
                                        .selected(terminal_open)
                                        .tooltip(crate::tr!("chat.toggle_terminal"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.on_toggle_panel(&TogglePanel::Terminal, window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("plan-panel")
                                        .motion(&self.motion)
                                        .debug_selector(|| "plan-panel".into())
                                        .on_mouse_down(gpui::MouseButton::Left, stop_press)
                                        .ghost()
                                        .small()
                                        .compact()
                                        .icon(IconName::Map)
                                        .selected(plan_showing)
                                        .tooltip(crate::tr!("chat.toggle_tasks"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.on_toggle_panel(&TogglePanel::Plan, window, cx)
                                        })),
                                )
                                .children(agents_toggle.map(|toggle| {
                                    toggle.fold_across_gap(
                                        toggle_gap,
                                        div()
                                            .relative()
                                            .child(
                                                Button::new("agents-panel")
                                                    .motion(&self.motion)
                                                    .debug_selector(|| "agents-panel".into())
                                                    .on_mouse_down(
                                                        gpui::MouseButton::Left,
                                                        stop_press,
                                                    )
                                                    .ghost()
                                                    .small()
                                                    .compact()
                                                    .icon(IconName::Bot)
                                                    .selected(agents_showing)
                                                    .tooltip(crate::tr!("agents.toggle"))
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.on_toggle_panel(
                                                                &TogglePanel::Agents,
                                                                window,
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                            )
                                            .children(cue.take()),
                                    )
                                }))
                                .children(preview_toggle.map(|toggle| {
                                    toggle.fold_across_gap(
                                        toggle_gap,
                                        Button::new("preview-panel")
                                            .motion(&self.motion)
                                            .on_mouse_down(gpui::MouseButton::Left, stop_press)
                                            .ghost()
                                            .small()
                                            .compact()
                                            .icon(IconName::Globe)
                                            .selected(preview_showing)
                                            .tooltip(crate::tr!("chat.toggle_preview"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.on_toggle_panel(
                                                    &TogglePanel::Preview,
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                }))
                                .child(
                                    Button::new("diff-panel")
                                        .motion(&self.motion)
                                        .debug_selector(|| "diff-panel".into())
                                        .on_mouse_down(gpui::MouseButton::Left, stop_press)
                                        .ghost()
                                        .small()
                                        .compact()
                                        .icon(IconName::PanelRight)
                                        .selected(diff_showing)
                                        .tooltip(crate::tr!("chat.toggle_diff"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.on_toggle_panel(&TogglePanel::Diff, window, cx)
                                        })),
                                )
                                .children(
                                    crate::shell::pane_controls(self.pane(), window, cx)
                                        .map(|panes| pane_menu(panes, &self.motion)),
                                ),
                        )
                        .into_any_element(),
                ));
            }
            swap.render(layers)
        });
        let header = header
            .children(tier)
            // After every tier's controls, so it keeps the right end however
            // the header folds.
            .children(self.pane_close.as_ref().map(|close| {
                close.fade(
                    crate::panes::close_button("pane-close", close.value.clone())
                        .motion(&self.motion),
                )
            }))
            // Last child, so the header's own actions keep their places to the
            // left of it.
            .children(hosts_caption.then(|| window_caption::caption_controls(window, cx)));
        div()
            .id("chat-header-scroll")
            .debug_selector(|| "chat-header-scroll".into())
            .flex_none()
            .w_full()
            .h(px(crate::material::bar_height(
                crate::material::Bar::Strip,
                false,
            )))
            .child(header)
            .overflow_x_scroll_area()
            .track_scroll(&self.header_scroll)
            .into_any_element()
    }

    /// A header control that drops its label when the header narrows past
    /// [`HEADER_LABELS_MIN_WIDTH`], built by `build` with or without it:
    /// the one cross-fades into the other on the view's clock while its
    /// width glides, so the title and its neighbours move aside rather than
    /// jump.
    fn labelled_swap(
        &self,
        key: &'static str,
        labelled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl Fn(&Self, bool, &mut Context<Self>) -> Option<AnyElement>,
    ) -> Option<AnyElement> {
        let swap = self
            .motion
            .swap(key, usize::from(labelled), 2, CHANGE, window, cx)
            .across();
        let layers = (0..2)
            .filter(|layer| swap.shows(*layer))
            .map(|layer| Some((layer, build(self, layer == 1, cx)?)))
            .collect::<Option<Vec<_>>>()?;
        Some(swap.render(layers))
    }

    pub(super) fn on_toggle_panel(
        &mut self,
        panel: &TogglePanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let tab = match panel {
            TogglePanel::Terminal => None,
            TogglePanel::Plan => Some(RightTab::Plan),
            TogglePanel::Agents => Some(RightTab::Agents),
            TogglePanel::Preview => Some(RightTab::Preview),
            TogglePanel::Diff => Some(RightTab::Diff),
            TogglePanel::PullRequests => Some(RightTab::PullRequests),
        };
        if tab.is_some() {
            self.close_drawer_agent(cx);
        }
        self.workspace_store.update(cx, |store, cx| match tab {
            None => store.toggle_terminal_panel_for(session_id, cx),
            Some(tab) => store.toggle_tab_panel_for(&session_id, tab, cx),
        });
    }

    /// The overflow header's one button: a menu of the panel toggles, each
    /// checked while its panel shows, then the Open button's choices and the
    /// pane commands. The agents' cue rides on the button.
    fn render_header_overflow(
        &self,
        panels: HeaderPanels,
        agents_cue: Option<AnyElement>,
        cwd: Option<PathBuf>,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        use crate::widgets::menu::{CopyText, OpenPathInZed, RevealPath};
        let panes = crate::shell::pane_controls(self.pane(), window, cx);
        let cwd = cwd.map(|cwd| cwd.to_string_lossy().into_owned());
        // The panels every thread has come first, so their rows keep their
        // places.
        let rows = [
            (
                "chat.toggle_terminal",
                Some(panels.terminal),
                TogglePanel::Terminal,
            ),
            ("chat.toggle_tasks", Some(panels.plan), TogglePanel::Plan),
            ("chat.toggle_diff", Some(panels.diff), TogglePanel::Diff),
            ("agents.toggle", panels.agents, TogglePanel::Agents),
            ("chat.toggle_preview", panels.preview, TogglePanel::Preview),
            (
                "pull_requests.toggle",
                panels.pull_requests,
                TogglePanel::PullRequests,
            ),
        ];
        div()
            .relative()
            .flex_none()
            .child(
                Button::new("header-overflow")
                    .motion(&self.motion)
                    .debug_selector(|| "header-overflow".into())
                    .ghost()
                    .small()
                    .compact()
                    .icon(IconName::Ellipsis)
                    .tooltip(crate::tr!("chat.header_more"))
                    .dropdown_menu(move |menu, _, _| {
                        let menu =
                            rows.iter()
                                .fold(menu, |menu, (key, checked, panel)| match checked {
                                    Some(checked) => menu.menu_with_check(
                                        crate::tr!(*key).into_owned(),
                                        *checked,
                                        Box::new(*panel),
                                    ),
                                    None => menu,
                                });
                        let menu = match &cwd {
                            Some(cwd) => menu
                                .separator()
                                .menu_with_icon(
                                    crate::tr!("chat.open_zed").into_owned(),
                                    IconName::ExternalLink,
                                    Box::new(OpenPathInZed(cwd.clone())),
                                )
                                .menu_with_icon(
                                    crate::tr!("chat.reveal_in_file_manager").into_owned(),
                                    IconName::FolderOpen,
                                    Box::new(RevealPath(cwd.clone())),
                                )
                                .menu_with_icon(
                                    crate::tr!("chat.copy_path").into_owned(),
                                    IconName::Copy,
                                    Box::new(CopyText(cwd.clone())),
                                ),
                            None => menu,
                        };
                        match panes {
                            Some(panes) => pane_items(menu.separator(), panes),
                            None => menu,
                        }
                        .after_choose(crate::shell::focus_pane_composer)
                    }),
            )
            .children(agents_cue)
            .into_any_element()
    }

    /// Git quick-action split button whose primary action and dropdown choices
    /// follow the current git status.
    /// Unlabelled, the main part is its icon, named by its tooltip (or the
    /// disabled hint) and its accessibility label.
    fn render_git_button(&self, labelled: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (quick, items) = self
            .workspace_store
            .read(cx)
            .chat_git_controls_for(self.session_id.as_deref())?;
        let border = cx.theme().border;

        let label: SharedString = crate::tr!(git_action_label_key(quick.label))
            .into_owned()
            .into();
        let main_icon = quick
            .action
            .map(git_action_icon)
            .unwrap_or_else(|| Icon::empty().path("icons/git-branch.svg"));
        let mut main = Button::new("git-main")
            .motion(&self.motion)
            .debug_selector(|| "git-main".into())
            .ghost()
            .small()
            .rounded_corners(segment_corners(true, cx))
            .disabled(quick.disabled)
            .aria_label(label.clone())
            .icon(main_icon)
            .when(labelled, |main| main.label(label.clone()));
        if quick.disabled {
            if let Some(hint) = quick.hint {
                main = main.tooltip(crate::tr!(git_hint_key(hint)).into_owned());
            }
        } else if let Some(action) = quick.action {
            main = main
                .when(!labelled, |main| main.tooltip(label.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.trigger_git_action(action, window, cx);
                }));
        }

        // Dropdown listing the applicable subset. Menu rows dispatch through the
        // ChatView entity (the popover content runs at App level, not in a view
        // context, so `cx.listener` is unavailable here).
        let chat = cx.entity();
        let chevron = crate::material::overlay_popover("git-menu", cx)
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("git-menu-trigger")
                    .motion(&self.motion)
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(false, cx))
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let muted = cx.theme().muted_foreground;
                let popover = cx.entity();
                let mut menu = v_flex().w(px(210.)).p_1().gap_0p5();
                for (index, item) in items.clone().into_iter().enumerate() {
                    let label: SharedString = crate::tr!(git_action_label_key(item.action))
                        .into_owned()
                        .into();
                    let action = item.action;
                    let disabled = item.disabled;
                    let popover = popover.clone();
                    let chat = chat.clone();
                    let fill = if disabled {
                        crate::material::ItemFill::Inert
                    } else {
                        crate::material::ItemFill::Hover
                    };
                    let frame = crate::material::menu_item_frame(false, fill, cx)
                        .w_full()
                        .items_center()
                        .text_body(false, cx)
                        .child(git_action_icon(action).text_color(muted))
                        .child(div().flex_1().child(label.clone()));
                    let row = if disabled {
                        let mut row = frame
                            .id(("git-menu-item", index))
                            .role(Role::MenuItem)
                            .aria_label(label)
                            .text_color(muted);
                        if let Some(hint) = item.hint {
                            let text: SharedString =
                                crate::tr!(git_hint_key(hint)).into_owned().into();
                            row = row.with_tooltip(move |_, _| Tooltip::new(text.clone()));
                        }
                        row.into_any_element()
                    } else {
                        crate::material::accessible_clickable(
                            frame,
                            ("git-menu-item", index),
                            Role::MenuItem,
                            label,
                            cx,
                        )
                        .cursor_pointer()
                        .on_click(move |_, window, cx| {
                            popover.update(cx, |st, cx| st.dismiss(window, cx));
                            chat.update(cx, |this, cx| this.trigger_git_action(action, window, cx));
                        })
                        .into_any_element()
                    };
                    menu = menu.child(row);
                }
                menu.into_any_element()
            });

        Some(
            h_flex()
                .flex_none()
                .items_center()
                .rounded(crate::material::radius_input(cx))
                .text_body(false, cx)
                .border_1()
                .border_color(border)
                .overflow_hidden()
                .child(main)
                // A control press must not arm the header window drag region.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().w_px().h(px(16.)).bg(border))
                .child(chevron)
                .into_any_element(),
        )
    }

    /// Dispatch a git quick-action: commit-style actions open the commit dialog;
    /// everything else runs in the background with a progress toast.
    fn trigger_git_action(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.opens_commit_dialog() {
            self.open_commit_dialog(action, window, cx);
        } else {
            let session_id = self.session_id.clone().unwrap_or_default();
            self.workspace_store.update(cx, |store, _cx| {
                store.run_git_action_for(session_id, action, None, None, None)
            });
        }
    }

    /// Open the commit dialog for `action` (Commit or Commit & push).
    fn open_commit_dialog(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = self.session_id.clone().unwrap_or_default();
        let dialog = cx.new(|cx| {
            CommitDialog::new(self.workspace_store.clone(), session_id, action, window, cx)
        });
        self.commit_dialog = Some(dialog.clone());
        window.open_dialog(cx, move |dlg, window, cx| {
            let content = dialog.clone();
            let footer_dialog = dialog.clone();
            dlg.title(crate::tr!("git.commit.title").into_owned())
                .wide()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(render_commit_footer(&footer_dialog, window, cx))
        });
    }

    /// Open the session cwd in Zed, or choose a directory action from the menu.
    /// Unlabelled, the main part is its icon, named by its tooltip and its
    /// accessibility label.
    fn render_open_button(
        &self,
        cwd: PathBuf,
        labelled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let border = cx.theme().border;
        let main_cwd = cwd.clone();
        let menu_cwd = cwd;

        let chevron = crate::material::overlay_popover("open-menu", cx)
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("open-menu-trigger")
                    .motion(&self.motion)
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(false, cx))
                    .icon(IconName::ChevronDown),
            )
            .content(move |_state, _window, cx| {
                let zed_cwd = menu_cwd.clone();
                let reveal_cwd = menu_cwd.clone();
                let copy_cwd = menu_cwd.clone();
                let popover = cx.entity();
                let p1 = popover.clone();
                let p2 = popover.clone();
                let p3 = popover.clone();
                let muted = cx.theme().muted_foreground;
                let menu_item = move |id: &'static str, icon: IconName, label: SharedString| {
                    crate::material::accessible_clickable(
                        crate::material::menu_item_frame(
                            false,
                            crate::material::ItemFill::Hover,
                            cx,
                        ),
                        id,
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .w_full()
                    .items_center()
                    .cursor_pointer()
                    .text_body(false, cx)
                    .child(Icon::new(icon).text_color(muted))
                    .child(label)
                };
                v_flex()
                    .w(px(180.))
                    .p_1()
                    .gap_0p5()
                    .child(
                        menu_item(
                            "open-zed",
                            IconName::ExternalLink,
                            crate::tr!("chat.open_zed").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            open_in_zed(&zed_cwd, window, cx);
                            p1.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "reveal-in-file-manager",
                            IconName::FolderOpen,
                            crate::tr!("chat.reveal_in_file_manager")
                                .into_owned()
                                .into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.reveal_path(&reveal_cwd);
                            p2.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .child(
                        menu_item(
                            "copy-path",
                            IconName::Copy,
                            crate::tr!("chat.copy_path").into_owned().into(),
                        )
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                copy_cwd.display().to_string(),
                            ));
                            p3.update(cx, |st, cx| st.dismiss(window, cx));
                        }),
                    )
                    .into_any_element()
            });

        h_flex()
            .flex_none()
            // A control press must not arm the header window drag region.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .items_center()
            .rounded(crate::material::radius_input(cx))
            .text_body(false, cx)
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .child(
                Button::new("open-main")
                    .motion(&self.motion)
                    .ghost()
                    .small()
                    .rounded_corners(segment_corners(true, cx))
                    .aria_label(crate::tr!("chat.open"))
                    .icon(Icon::new(IconName::ExternalLink).text_color(cx.theme().muted_foreground))
                    .map(|main| {
                        if labelled {
                            main.label(crate::tr!("chat.open"))
                        } else {
                            main.tooltip(crate::tr!("chat.open"))
                        }
                    })
                    .on_click(cx.listener(move |_, _, window, cx| {
                        open_in_zed(&main_cwd, window, cx);
                    })),
            )
            .child(div().w_px().h(px(16.)).bg(border))
            .child(chevron)
            .into_any_element()
    }
}

fn git_action_icon(action: GitAction) -> Icon {
    match action {
        GitAction::Push => Icon::new(IconName::ArrowUp),
        GitAction::Pull => Icon::empty().path("icons/download.svg"),
        _ => Icon::empty().path("icons/git-branch.svg"),
    }
}

/// The commit dialog's footer action row (Cancel / Commit[& push]). Built inside
/// the `open_dialog` builder so the buttons can close the dialog on click.
fn render_commit_footer(
    dialog: &Entity<CommitDialog>,
    _window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let confirm_label = dialog.update(cx, |d, cx| d.confirm_label(cx));
    let cancel_dialog = dialog.clone();
    let confirm_dialog = dialog.clone();
    crate::overlay::DialogActions::new()
        .child(
            Button::new("commit-cancel")
                .ghost()
                .label(crate::tr!("git.commit.cancel"))
                .on_click(move |_, window, cx| {
                    let _ = &cancel_dialog;
                    window.close_dialog(cx);
                }),
        )
        .child(
            Button::new("commit-confirm")
                .primary()
                .label(confirm_label)
                .on_click(move |_, window, cx| {
                    let should_close = confirm_dialog.update(cx, |d, cx| d.confirm(window, cx));
                    if should_close {
                        window.close_dialog(cx);
                    }
                }),
        )
        .into_any_element()
}

/// The two ends of a segmented control's border: a half takes the control's
/// radius on its outer side only, so its hover fill stays inside the border.
fn segment_corners(leading: bool, cx: &App) -> gpui::Corners<gpui::Pixels> {
    let (radius, flat) = (crate::material::radius_input(cx), px(0.));
    if leading {
        gpui::Corners {
            top_left: radius,
            top_right: flat,
            bottom_right: flat,
            bottom_left: radius,
        }
    } else {
        gpui::Corners {
            top_left: flat,
            top_right: radius,
            bottom_right: radius,
            bottom_left: flat,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw;
    use gpui::TestAppContext;

    /// Whether the header's split buttons carry their labels follows the width
    /// it is laid out at, from the frame a resize draws.
    #[gpui::test]
    fn header_labels_follow_the_width_of_the_frame(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(1));
        let (_view, cx) = cx.add_window_view(|window, cx| {
            ChatView::new(store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);
        // A resize draws its frame at the new size.
        let git_main = |width: f32, cx: &mut VisualTestContext| {
            cx.simulate_resize(size(px(width), px(700.)));
            cx.debug_bounds("git-main")
                .expect("the git button")
                .size
                .width
        };
        let labelled = git_main(1_024., cx);
        let narrow = git_main(600., cx);
        assert!(narrow < labelled, "a 600pt header kept its labels");
        assert_eq!(
            git_main(1_024., cx),
            labelled,
            "a 1024pt header lost its labels"
        );
    }

    /// A chat drawn under one parent or another, as the dock draws a pane's
    /// group under a new parent whenever the panes around it change.
    struct Reparented {
        chat: gpui::Entity<ChatView>,
        moved: bool,
    }

    impl gpui::Render for Reparented {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
            let chat = self.chat.clone();
            if self.moved {
                gpui::div()
                    .size_full()
                    .child(gpui::div().id("new-parent").size_full().child(chat))
            } else {
                gpui::div().size_full().child(chat)
            }
        }
    }

    /// `at` on each frame of a change `change` starts, the chat moved to a
    /// new parent on the third frame when `moved`, until it has come to rest.
    fn follow(
        root: &gpui::Entity<Reparented>,
        change: impl FnOnce(&mut gpui::VisualTestContext),
        moved: bool,
        at: impl Fn(&mut gpui::VisualTestContext) -> gpui::Pixels,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<gpui::Pixels> {
        let mut seen = vec![at(cx)];
        change(cx);
        draw(cx);
        seen.push(at(cx));
        for frame in 0..20 {
            if moved && frame == 2 {
                root.update(cx, |root, cx| {
                    root.moved = !root.moved;
                    cx.notify();
                });
            }
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
            seen.push(at(cx));
        }
        crate::test_frames::draw_settled(cx);
        seen.push(at(cx));
        seen
    }

    /// Whether `seen` moves one way from its first value to its last, through
    /// at least three values between them.
    fn glides(seen: &[gpui::Pixels]) -> bool {
        let (from, to) = (seen[0], *seen.last().unwrap());
        from != to
            && seen.windows(2).all(|pair| {
                if to > from {
                    pair[1] >= pair[0]
                } else {
                    pair[1] <= pair[0]
                }
            })
            && seen.iter().filter(|at| **at != from && **at != to).count() >= 3
    }

    /// Over a collapsed sidebar the header clears the traffic lights; as its
    /// pane moves off the window's left edge and back the clearance glides
    /// over several frames, one way each time, carrying on when the dock
    /// draws the pane under a new parent part way; with motion reduced it
    /// changes at once.
    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn the_traffic_light_clearance_glides_as_a_pane_leaves_the_edge(cx: &mut TestAppContext) {
        use crate::chat::WindowEdges;
        use crate::test_frames::draw_settled;
        use gpui::{AppContext as _, VisualTestContext, px, size};

        let (store, window_state, session_id) = seed_chat(cx, synthetic_markdown_timeline(1));
        let (root, cx) = cx.add_window_view(|window, cx| Reparented {
            chat: cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    window_state.clone(),
                    Some(session_id),
                    window,
                    cx,
                )
            }),
            moved: false,
        });
        let cx: &mut VisualTestContext = cx;
        let view = root.read_with(cx, |root, _| root.chat.clone());
        cx.simulate_resize(size(px(1_024.), px(700.)));
        window_state.update(cx, |state, cx| state.toggle_sidebar_collapsed(&store, cx));
        draw_settled(cx);
        let toggle = |cx: &mut VisualTestContext| {
            cx.debug_bounds("toggle-sidebar")
                .expect("the sidebar toggle")
                .left()
        };
        let cleared = toggle(cx);
        for reduce in [false, true] {
            cx.update(|_, cx| cx.set_reduce_motion(reduce));
            for left in [false, true] {
                let edges = |cx: &mut VisualTestContext| {
                    view.update(cx, |chat, cx| {
                        chat.set_edges(WindowEdges { left, right: true }, cx)
                    })
                };
                let seen = follow(&root, edges, !reduce, toggle, cx);
                assert_eq!(left, *seen.last().unwrap() == cleared, "{seen:?}");
                if reduce {
                    assert_eq!(seen[1], seen[seen.len() - 1], "it glided: {seen:?}");
                } else {
                    assert!(glides(&seen), "the clearance jumped: {seen:?}");
                }
            }
        }
    }

    /// The Agents toggle widens into the header's toolbar with the thread's
    /// first agent and narrows out with its last, over several frames,
    /// carrying on when the dock draws the pane under a new parent part way.
    #[gpui::test]
    fn the_agents_toggle_folds_on_through_a_new_parent(cx: &mut TestAppContext) {
        use agent::{ItemContent, ItemStatus};
        use gpui::{AppContext as _, VisualTestContext, px, size};
        use tcode_core::session::EntryContent;

        let plain = synthetic_markdown_timeline(1);
        let mut with_agent = plain.clone();
        with_agent.entries.insert(
            1,
            entry(
                "agent-1",
                EntryContent::Item(ItemContent::Subagent {
                    agent_type: "Explore".into(),
                    description: "Survey the code".into(),
                    status: ItemStatus::Completed,
                    summary: None,
                    model: None,
                    effort: None,
                }),
            ),
        );
        let (store, window_state, session_id) = seed_chat(cx, plain.clone());
        let (root, cx) = cx.add_window_view(|window, cx| Reparented {
            chat: cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    window_state,
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            }),
            moved: false,
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        crate::test_frames::draw_settled(cx);
        // The button before the toggle, which it moves aside.
        let plan = |cx: &mut VisualTestContext| {
            cx.debug_bounds("plan-panel")
                .expect("the plan button")
                .left()
        };
        for timeline in [&with_agent, &plain] {
            let replace = |cx: &mut VisualTestContext| {
                store.update(cx, |store, cx| {
                    store.set_session_replica_for_test(session_id.clone(), timeline.clone(), cx)
                })
            };
            let seen = follow(&root, replace, true, plan, cx);
            assert!(glides(&seen), "the toggle jumped: {seen:?}");
        }
    }
}
