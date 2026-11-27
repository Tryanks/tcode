use super::*;

// ---------------------------------------------------------------------------
// Compact chrome
// ---------------------------------------------------------------------------

/// The least room a nav bar keeps on each side for its controls, so the
/// centered title keeps the same box on every page whose controls fit it: two
/// 44pt icon buttons. A wider back label ([`Destination::back_label`]), which
/// never truncates, takes more.
const NAV_CONTROL_WIDTH: f32 = 104.;

/// A page that is the whole window: one of the compact stack, or the Hosts
/// page a tablet shows before it attaches. It is inset from the seam on
/// every side ([`crate::window_seam::inset_from_seam`]). The page paints
/// there rather than the shell, so those edges show whichever page covers
/// them and slide with it as it is pushed or dragged back.
fn page_surface(window: &mut Window, cx: &mut App) -> Div {
    crate::window_seam::inset_from_seam(
        v_flex()
            .size_full()
            .bg(crate::material::content_surface(cx)),
        "page-seam",
        gpui::Edges::all(true),
        window,
        cx,
    )
}

/// Navigation header with a centered title and at most two trailing actions.
fn nav_bar(
    back: Option<AnyElement>,
    title: SharedString,
    subtitle: Option<AnyElement>,
    actions: Vec<AnyElement>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    debug_assert!(
        actions.len() <= 2,
        "navigation bar allows at most two trailing actions"
    );
    // The compact layout previewed in a desktop window is still a window: it
    // keeps the controls the platform draws out of the content and stays
    // draggable by its top strip.
    let traffic_lights = window_caption::traffic_light_inset(window, cx);
    let hosts_caption = window_caption::compact_hosts_caption();
    let (dense, tight) = (cx.theme().tokens.spacing.sm, cx.theme().tokens.spacing.xs);
    // The sides carry the strip's insets themselves, so their floor is
    // measured from the window's edge (or the platform's controls). The 44pt
    // controls keep their glyphs clear of the edge inside their own hit box,
    // so the inset is the dense one, not the page inset.
    let strip = crate::material::bar("compact-nav", crate::material::Bar::Strip, false, cx)
        .px_0()
        .gap(Pixels::ZERO)
        .pl(traffic_lights);
    v_flex()
        .flex_none()
        .w_full()
        .bg(crate::material::content_surface(cx))
        .child(
            window_drag_area("compact-nav-drag", strip, window, cx)
                // The two sides grow alike from nothing, so the title is centered
                // while it fits; a side never shrinks below its controls, so a
                // label wider than the reserved room pushes the title, which
                // truncates, instead of running into it.
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .when(traffic_lights == Pixels::ZERO, |side| side.pl(dense))
                        .pr(tight)
                        .children(back)
                        .child(window_caption::drag_region(div().flex_1().h_full())),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .max_w_full()
                                .min_w_0()
                                .overflow_hidden()
                                .text_title(cx)
                                .child(crate::widgets::fade_text(title)),
                        )
                        .children(subtitle),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .flex_basis(px(0.))
                        .min_w(px(NAV_CONTROL_WIDTH))
                        .h_full()
                        .items_center()
                        .justify_end()
                        .gap(tight)
                        .when(!hosts_caption, |side| side.pr(dense))
                        .child(window_caption::drag_region(div().flex_1().h_full()))
                        .children(actions)
                        .children(
                            hosts_caption.then(|| window_caption::caption_controls(window, cx)),
                        ),
                ),
        )
        .child(crate::material::faded_hairline(cx))
}

/// Back control labelled with the parent destination's short fixed label,
/// which is never truncated.
pub(super) fn back_button(
    id: &'static str,
    parent: SharedString,
) -> crate::widgets::button::Button {
    use crate::widgets::button::{Button, ButtonVariants as _};
    Button::new(id)
        .ghost()
        .touch(true)
        .icon(Icon::empty().path("icons/chevron-left.svg"))
        .label(parent)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A nav-bar icon button.
fn nav_icon_button(
    id: impl Into<ElementId>,
    aria_label: impl Into<SharedString>,
    icon: IconName,
) -> crate::widgets::button::Button {
    crate::material::toolbar_icon_button(id, icon, aria_label, true)
        // A control press must not arm the ancestor window drag region.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// The muted second line under a nav-bar title: how the machine a thread
/// list belongs to is reached, the project a thread lives in.
fn nav_subtitle(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    div()
        .max_w_full()
        .min_w_0()
        .overflow_hidden()
        .text_secondary(true, cx)
        .child(crate::widgets::fade_text(text.into()))
        .into_any_element()
}

fn compact_label(key: &str) -> String {
    crate::tr!(format!("mobile.{key}")).into_owned()
}

impl AppShell {
    /// The compact stack's topmost thread page, unless a restore is yet to
    /// say which thread it shows.
    pub(super) fn top_thread_page(&self, cx: &App) -> Option<EntityId> {
        if !self.compact(cx)
            || self.departing.is_some()
            || self.pending_navigation_restore.is_some()
        {
            return None;
        }
        let at = self
            .mounted
            .iter()
            .rposition(|destination| *destination == Destination::Thread)?;
        let page = self.nav.read(cx).views().nth(at)?.entity_id();
        Some(page)
    }

    /// Open the agent `thread` over the thread on the compact stack's
    /// thread page, or the panels page standing on it, which goes: the thread
    /// stays as it is on its page, open and held. False where no thread
    /// page can take it, for the caller to show it.
    pub(super) fn open_over_lead(
        &mut self,
        thread: ThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(page) = self.top_thread_page(cx) else {
            return false;
        };
        if self
            .column
            .as_ref()
            .is_none_or(|column| column.page().read(cx).host(cx) != thread.host)
            || !matches!(
                self.destination(cx),
                Destination::Thread | Destination::Panel
            )
            || self.pane_showing(&thread, cx).is_some()
        {
            return false;
        }
        let window_state = self.window_state.clone();
        let Some(column) = &mut self.column else {
            return false;
        };
        let pane = column.page().clone();
        let store = pane.read(cx).store().clone();
        let Some(lead) = pane.read(cx).thread(cx) else {
            return false;
        };
        if column.leads.iter().any(|kept| kept.thread == thread) {
            return false;
        }
        let session_id = thread.session_id;
        store.update(cx, |store, cx| {
            store.lease_timeline(&lead.session_id);
            if !store.is_thread_open(&session_id) {
                store.open_thread(&session_id, cx);
            }
        });
        let state = window_state.clone();
        let chat =
            cx.new(|cx| ChatView::new(store.clone(), state, Some(session_id.clone()), window, cx));
        let lead_chat = pane.update(cx, |pane, cx| pane.replace_chat(chat, None, cx));
        column.leads.push(ThreadPage {
            page,
            frame: lead_chat.read(cx).frame(),
            chat: lead_chat,
            thread: lead,
        });
        store.update(cx, |store, cx| store.show_thread(session_id, cx));
        window_state.update(cx, |state, cx| state.stack_thread(cx));
        true
    }

    /// An agent on the compact stack's top page reached other than from its
    /// lead has the lead put beneath it, for Back and the edge swipe to
    /// uncover.
    pub(super) fn keep_lead_beneath(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.top_thread_page(cx) else {
            return;
        };
        if self.destination(cx) != Destination::Thread
            || self.mounted.last() != Some(&Destination::Thread)
            || self
                .column
                .as_ref()
                .is_none_or(|column| !column.leads.is_empty())
        {
            return;
        }
        let Some((store, lead)) = self.agent_lead(cx) else {
            return;
        };
        store.update(cx, |store, cx| {
            store.lease_timeline(&lead);
            if !store.is_thread_open(&lead) {
                store.open_thread(&lead, cx);
            }
        });
        let thread = store.read(cx).thread_key(&lead);
        let window_state = self.window_state.clone();
        let state = window_state.clone();
        let chat = cx.new(|cx| ChatView::new(store, state, Some(lead), window, cx));
        if let Some(column) = &mut self.column {
            column.leads.push(ThreadPage {
                page,
                frame: chat.read(cx).frame(),
                chat,
                thread,
            });
        }
        window_state.update(cx, |state, cx| state.stack_thread(cx));
        self.sync_nav(NavMotion::Immediate, cx);
    }

    /// Back to the thread under the compact page's agent, whose page comes
    /// back to the page pane, as it was left. The agent closes, as
    /// showing another thread would, and stays drawn, leased, on its page
    /// until that has slid off.
    pub(super) fn pop_lead(&mut self, cx: &mut Context<Self>) {
        let top = self.nav.read(cx).current().map(AnyView::entity_id);
        let window_state = self.window_state.clone();
        let Some(column) = &mut self.column else {
            return;
        };
        let Some(lead) = column.leads.pop() else {
            return;
        };
        let pane = column.page().clone();
        let store = pane.read(cx).store().clone();
        let agent = pane.read(cx).thread(cx);
        if let Some(agent) = &agent {
            store.update(cx, |store, _| store.lease_timeline(&agent.session_id));
        }
        let chat = pane.update(cx, |pane, cx| pane.replace_chat(lead.chat, None, cx));
        store.update(cx, |store, cx| {
            store.selection_moved_on();
            store.release_timeline(&lead.thread.session_id, cx);
            if let Some(agent) = &agent
                && store.is_thread_open(&agent.session_id)
            {
                store.close_thread(&agent.session_id, cx);
            }
        });
        if let (Some(page), Some(thread)) = (top, agent) {
            column.leaving.push(ThreadPage {
                page,
                frame: chat.read(cx).frame(),
                chat,
                thread,
            });
        }
        window_state.update(cx, |state, cx| state.unstack_threads(1, cx));
    }

    /// Let go of the leads from the `at`th on, each closing unless it is
    /// still shown. The page of the `at`th takes the page pane's
    /// conversation again and the thread pages above it go, at once: the
    /// conversation is what it was, whichever page it is on.
    pub(super) fn drop_leads(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(column) = &mut self.column else {
            return;
        };
        let dropped = column.leads.split_off(at.min(column.leads.len()));
        if dropped.is_empty() {
            return;
        }
        for lead in &dropped {
            let shown = self.pane_showing(&lead.thread, cx).is_some();
            let session_id = lead.thread.session_id.as_str();
            lead.chat
                .read(cx)
                .workspace_store()
                .clone()
                .update(cx, |store, cx| {
                    if !shown && store.is_thread_open(session_id) {
                        store.close_thread(session_id, cx);
                    }
                    store.release_timeline(session_id, cx);
                });
        }
        self.window_state
            .update(cx, |state, cx| state.unstack_threads(dropped.len(), cx));
        self.sync_nav(NavMotion::Immediate, cx);
    }

    /// A lead of `store`'s host that closed goes, with the pages above it.
    pub(super) fn reconcile_leads(
        &mut self,
        store: &Entity<WorkspaceStore>,
        cx: &mut Context<Self>,
    ) {
        let Some(column) = &self.column else {
            return;
        };
        if let Some(closed) = column.leads.iter().position(|lead| {
            lead.chat.read(cx).workspace_store() == store
                && !store.read(cx).is_thread_open(&lead.thread.session_id)
        }) {
            self.drop_leads(closed, cx);
        }
    }

    /// Going to `thread` from the compact stack: a lead is Back to it, as an
    /// agent archived falls back to its lead; anywhere else leaves them all.
    pub(super) fn unwind_leads(&mut self, thread: &ThreadKey, cx: &mut Context<Self>) {
        let Some(column) = &self.column else {
            return;
        };
        if column.leads.is_empty() || column.page().read(cx).thread(cx).as_ref() == Some(thread) {
            return;
        }
        match column.leads.iter().rposition(|lead| lead.thread == *thread) {
            Some(at) => {
                self.drop_leads(at + 1, cx);
                self.pop_lead(cx);
            }
            None => self.drop_leads(0, cx),
        }
    }

    /// Let go of the agents that have left.
    pub(super) fn release_leaving(&mut self, cx: &mut Context<Self>) {
        let Some(column) = &mut self.column else {
            return;
        };
        for left in column.leaving.drain(..) {
            left.chat
                .read(cx)
                .workspace_store()
                .clone()
                .update(cx, |store, cx| {
                    store.release_timeline(&left.thread.session_id, cx)
                });
        }
    }

    /// The compact thread page's body, `kept` on a page that keeps its
    /// conversation: otherwise the skeleton while the thread is restored,
    /// then its chat rising over it.
    pub(super) fn compact_thread_body(
        &mut self,
        kept: Option<Entity<crate::chat::ChatFrame>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Under the id and layer of a body at rest, so what the chat keeps by
        // element is the same on either side of the page keeping it.
        if let Some(frame) = kept {
            return crate::motion::Scene::default()
                .sample("compact-thread-body", ThreadBody::Chat, window, cx)
                .render([frame.into_any_element()])
                .into_any_element();
        }
        let pending = self.pending_navigation_restore.is_some();
        let Some(column) = &mut self.column else {
            return div().into_any_element();
        };
        let wanted = if pending {
            ThreadBody::Skeleton
        } else {
            ThreadBody::Chat
        };
        // A chat comes in over what the page shows once it appears; until
        // then it readies itself unseen beneath, and the page holds still.
        let chat = column.chat(cx);
        let chat_frame = column.page().read(cx).frame();
        let holding = column
            .thread_body_shown
            .filter(|shown| *shown != wanted && wanted != ThreadBody::Skeleton)
            .filter(|_| !chat.read(cx).appears(cx));
        let current = holding.unwrap_or(wanted);
        column.thread_body_shown = Some(current);
        ChatView::set_held(&chat, holding.is_some(), cx);
        if holding.is_some() {
            window.request_animation_frame();
        }
        let frame = column
            .thread_body
            .sample("compact-thread-body", current, window, cx);
        let layers: Vec<AnyElement> = frame
            .scenes()
            .map(|key| match key {
                ThreadBody::Skeleton => crate::material::loading_skeleton(cx),
                ThreadBody::Chat => chat_frame.clone().into_any_element(),
            })
            .collect();
        frame
            .render(layers)
            .when(holding.is_some(), |body| {
                body.child(
                    div()
                        .absolute()
                        .inset_0()
                        .opacity(0.)
                        .child(chat_frame.clone())
                        .child(crate::motion::shield()),
                )
            })
            .into_any_element()
    }

    /// The lead thread of the open agent, when one is open.
    pub(super) fn agent_lead(&self, cx: &App) -> Option<(Entity<WorkspaceStore>, String)> {
        let page = self.column.as_ref()?.page().read(cx);
        let store = page.store().clone();
        let active = page.session_id()?.to_owned();
        let parent = {
            let store = store.read(cx);
            let sessions = store.sidebar_sessions();
            let parent = sessions
                .iter()
                .find(|meta| meta.id == active)?
                .parent_session_id
                .clone()?;
            sessions
                .iter()
                .any(|meta| meta.id == parent)
                .then_some(parent)?
        };
        Some((store, parent))
    }

    /// Back to whatever is under this page, labelled with that destination's
    /// short fixed label. `None` at the root, where the platform owns Back.
    pub(super) fn back_control(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let label = self.window_state.read(cx).parent()?.back_label();
        Some(self.back_action(label, cx))
    }

    pub(super) fn back_action(
        &mut self,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        back_button("compact-back", label)
            .debug_selector(|| "compact-back".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.back(window, cx);
            }))
            .into_any_element()
    }

    pub(super) fn machines_action(&self, cx: &mut Context<Self>) -> AnyElement {
        nav_icon_button(
            "compact-machines",
            crate::tr!("hosts.title").into_owned(),
            IconName::Monitor,
        )
        .debug_selector(|| "compact-machines".into())
        .on_click(cx.listener(|this, _, _, cx| this.go(Destination::Hosts, cx)))
        .into_any_element()
    }

    pub(super) fn palette_action(&self, cx: &mut Context<Self>) -> AnyElement {
        nav_icon_button(
            "compact-open-palette",
            crate::tr!("sidebar.search").into_owned(),
            IconName::Search,
        )
        .debug_selector(|| "compact-open-palette".into())
        .on_click(cx.listener(|this, _, window, cx| {
            this.on_toggle_palette(&TogglePalette, window, cx);
        }))
        .into_any_element()
    }

    pub(super) fn render_hosts_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let body = self.hosts_surface(false, window, cx);
        page_surface(window, cx)
            .debug_selector(|| "hosts-page".into())
            .child(nav_bar(
                back,
                crate::tr!("hosts.title").into_owned().into(),
                None,
                self.column
                    .as_ref()
                    .map(|_| self.palette_action(cx))
                    .into_iter()
                    .collect(),
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// The pairing form as its own page, so Back returns to whatever pushed it
    /// — the Hosts list, or the Nearby row that prefilled the endpoint.
    pub(super) fn render_pair_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let body = self.hosts_surface(true, window, cx);
        page_surface(window, cx)
            .child(nav_bar(
                back,
                crate::tr!("hosts.pair.page_title").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    /// Give the section a link elsewhere asked Settings to open to the page
    /// of the host Settings shows.
    pub(super) fn open_requested_settings_section(&mut self, cx: &mut Context<Self>) {
        if self
            .window_state
            .read(cx)
            .pending_settings_section
            .is_none()
        {
            return;
        }
        let Some(page) = self
            .settings_link(cx)
            .map(|link| link.settings_page.clone())
        else {
            return;
        };
        if let Some(section) = self
            .window_state
            .update(cx, |state, _| state.pending_settings_section.take())
        {
            page.update(cx, |page, cx| page.open_section(&section, cx));
        }
    }

    /// Settings, as two pages of the same stack: the section list, then one
    /// section's detail. Both wear the same nav bar as every other page.
    pub(super) fn render_settings_page(
        &mut self,
        destination: Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(settings) = self
            .settings_link(cx)
            .map(|link| link.settings_page.clone())
        else {
            return div().into_any_element();
        };
        let detail = destination != Destination::Settings;
        let title = if destination == Destination::SettingsSection
            && self.destination(cx) == Destination::SettingsThreadRules
        {
            crate::tr!("settings.threads").into_owned().into()
        } else if destination == Destination::SettingsSection
            && self.destination(cx) == Destination::SettingsSourceControlHost
        {
            crate::tr!("settings.source_control").into_owned().into()
        } else if destination == Destination::SettingsSection
            && self.destination(cx) == Destination::SettingsHarness
        {
            crate::tr!("settings.providers").into_owned().into()
        } else if detail {
            settings.read(cx).section_title(cx)
        } else {
            crate::tr!("settings.title").into_owned().into()
        };
        let body = settings.update(cx, |page, cx| {
            if detail {
                page.render_compact_section(destination, window, cx)
            } else {
                page.render_compact_list(cx)
            }
        });
        page_surface(window, cx)
            .child(nav_bar(
                back,
                title,
                None,
                vec![self.palette_action(cx)],
                window,
                cx,
            ))
            .child(body)
            .into_any_element()
    }

    pub(super) fn render_arrange_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.current_link(cx).is_none() {
            return div().into_any_element();
        }
        let sidebar = self.sidebar.clone();
        use crate::widgets::button::{Button, ButtonVariants as _};
        let done = div()
            // A control press must not arm the ancestor window drag region.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("arrange-done")
                    .ghost()
                    .label(crate::tr!("sidebar.arrange_done"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.back(window, cx);
                    })),
            )
            .into_any_element();
        let body = sidebar.update(cx, |sidebar, cx| sidebar.render_arrange(window, cx));
        page_surface(window, cx)
            .debug_selector(|| "compact-arrange-page".into())
            .child(nav_bar(None, "".into(), None, vec![done], window, cx))
            .child(body)
            .into_any_element()
    }

    pub(super) fn render_threads_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(link) = self.current_link(cx) else {
            let body = self.no_hosts(cx);
            return page_surface(window, cx)
                .debug_selector(|| "compact-threads-page".into())
                .child(nav_bar(
                    back,
                    crate::tr!("mobile.threads").into_owned().into(),
                    None,
                    vec![],
                    window,
                    cx,
                ))
                .child(body)
                .into_any_element();
        };
        let store = link.store.read(cx);
        // A single remote machine's list is headed by that machine and its link
        // state; a machine group's list is headed by the group's name; the
        // local list alone is Threads.
        let (title, subtitle): (SharedString, Option<SharedString>) =
            match self.active_group_name(cx) {
                Some(group_name) => (group_name.into(), None),
                None => match store.remote_host_name().filter(|_| self.links.len() == 1) {
                    Some(_) => (
                        store.machine_label().into(),
                        Some(if store.view_only() {
                            format!(
                                "{} · {}",
                                crate::remote::connection_label(&store.connection_state()),
                                crate::tr!("machines.access.mark")
                            )
                            .into()
                        } else {
                            crate::remote::connection_label(&store.connection_state()).into()
                        }),
                    ),
                    None => (crate::tr!("mobile.threads").into_owned().into(), None),
                },
            };
        let sidebar = self.sidebar.clone();
        // The root's leading control leads to Machines.
        let back = back.or_else(|| Some(self.machines_action(cx)));
        let new_thread_label = compact_label("new_thread");
        // Over several hosts the user picks the host with the project: none
        // is the window's to guess.
        let new_thread = if self.links.len() > 1 {
            let trigger = crate::material::toolbar_icon_button(
                "compact-new-thread",
                IconName::Plus,
                new_thread_label,
                true,
            )
            .debug_selector(|| "compact-new-thread".into());
            // The press must reach the chooser, and not arm the ancestor
            // window drag region.
            div()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.sidebar.read(cx).new_thread_chooser(trigger, true, cx))
                .into_any_element()
        } else {
            nav_icon_button("compact-new-thread", new_thread_label, IconName::Plus)
                .debug_selector(|| "compact-new-thread".into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.start_thread(cx);
                }))
                .into_any_element()
        };
        // No thread starts on a machine that lets this device only view it.
        let starts = self
            .links
            .iter()
            .any(|link| !link.store.read(cx).view_only());
        let actions = starts
            .then_some(new_thread)
            .into_iter()
            .chain([nav_icon_button(
                "compact-settings",
                compact_label("settings"),
                IconName::Settings,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }))
            .into_any_element()])
            .collect();
        page_surface(window, cx)
            .debug_selector(|| "compact-threads-page".into())
            .child(nav_bar(
                back,
                title,
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                actions,
                window,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.layout_part("sidebar", sidebar)),
            )
            .into_any_element()
    }

    /// What the threads are while the window has no host: the ways to one.
    pub(super) fn no_hosts(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::widgets::button::{Button, ButtonVariants as _};
        let pairs = !self.client.fixed_machine();
        v_flex()
            .debug_selector(|| "no-hosts".into())
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .gap(cx.theme().tokens.spacing.md)
            .px(px(crate::material::COMPACT_PAGE_INSET))
            .child(div().text_title(cx).child(crate::tr!("mobile.no_hosts")))
            .child(
                div()
                    .text_secondary(self.compact(cx), cx)
                    .text_center()
                    .child(crate::tr!("mobile.no_hosts_help")),
            )
            .child(
                h_flex()
                    .gap(cx.theme().tokens.spacing.sm)
                    .when(pairs, |row| {
                        row.child(
                            Button::new("no-hosts-pair")
                                .debug_selector(|| "no-hosts-pair".into())
                                .primary()
                                .label(crate::tr!("hosts.pair.page_title"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.go(Destination::Pair, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("no-hosts-machines")
                            .debug_selector(|| "no-hosts-machines".into())
                            .ghost()
                            .outline()
                            .label(crate::tr!("hosts.title"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.go(Destination::Hosts, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// A new thread in a project starts on the project's host.
    pub(super) fn on_start_draft_for_project(
        &mut self,
        action: &StartDraftForProject,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.link_of(&action.0.host).map(|link| link.store.clone()) else {
            return;
        };
        let Some(project) = store.read(cx).project(&action.0.project_id).cloned() else {
            return;
        };
        store.update(cx, |store, cx| {
            store.start_draft(project.id, project.root, cx)
        });
    }

    pub(super) fn on_quick_chat(
        &mut self,
        action: &QuickChat,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(link) = self.link_of(&action.0) {
            link.store
                .update(cx, |store, cx| store.start_scratch_draft(cx));
        }
    }

    /// New thread over one host: one project starts a draft directly, several
    /// go through the palette, which already owns "new thread in <project>"
    /// and can search.
    /// On a phone the palette opens without the keyboard: the projects are
    /// there to tap, and the search pill still raises it on demand.
    pub(super) fn start_thread(&mut self, cx: &mut Context<Self>) {
        let Some(link) = self.current_link(cx) else {
            return;
        };
        let projects = link.store.read(cx).projects();
        let Some(project) = projects.first().cloned().filter(|_| projects.len() == 1) else {
            let mobile = crate::window_seam::is_mobile(cx);
            self.window_state.update(cx, |state, cx| {
                if mobile {
                    state.open_palette_without_keyboard(cx);
                } else {
                    state.open_palette(cx);
                }
            });
            return;
        };
        link.store.update(cx, |store, cx| {
            store.start_draft(project.id.clone(), project.root.clone(), cx)
        });
    }

    pub(super) fn render_thread_page(
        &mut self,
        page: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kept = self
            .column
            .as_ref()
            .and_then(|column| column.thread_page(page))
            .cloned();
        // By the page's own place: a lead's page keeps the label it lands
        // with, and one sliding off, which has no place, was over a thread.
        let beneath = self
            .nav
            .read(cx)
            .views()
            .position(|view| view.entity_id() == page)
            .and_then(|at| self.mounted.get(at.checked_sub(1)?))
            .copied();
        let back = self.back_action(beneath.unwrap_or(Destination::Thread).back_label(), cx);
        let body =
            self.compact_thread_body(kept.as_ref().map(|kept| kept.frame.clone()), window, cx);
        let link = match &kept {
            Some(kept) => self.link_of(&kept.thread.host),
            None => self.current_link(cx),
        };
        let Some(link) = link else {
            return div().into_any_element();
        };
        let store = link.store.read(cx);
        let active = match &kept {
            Some(kept) => store.chat_active_session_for(Some(&kept.thread.session_id), cx),
            None => store.chat_active_session_for(
                self.page_thread(cx)
                    .map(|thread| thread.session_id)
                    .as_deref(),
                cx,
            ),
        };
        let project = active
            .as_ref()
            .and_then(|(_, cwd, _)| {
                store
                    .projects()
                    .into_iter()
                    .find(|project| project.root == *cwd)
            })
            .map(|project| project.name);
        // Over a remote link the project shares its line with the link's state.
        let subtitle = if !store.scope().is_full() {
            Some(store.machine_label())
        } else if self.links.len() > 1 {
            // Over several hosts it names the thread's host instead, and the
            // link's state only while the link is not up.
            let state = store.connection_state();
            let down = !matches!(
                state,
                tcode_client::ConnectionState::Connected { .. }
                    | tcode_client::ConnectionState::Syncing { .. }
            );
            let parts: Vec<String> = project
                .into_iter()
                .chain([store.host_title().to_string()])
                .chain(down.then(|| crate::remote::connection_label(&state)))
                .collect();
            Some(parts.join(" · "))
        } else {
            match (project, store.remote_host_name()) {
                (Some(project), Some(_)) => Some(format!(
                    "{project} · {}",
                    crate::remote::connection_label(&store.connection_state())
                )),
                (None, Some(_)) => Some(crate::remote::connection_label(&store.connection_state())),
                (project, None) => project,
            }
        };
        let title = active
            .map(|(title, _, draft)| {
                if draft {
                    compact_label("new_thread")
                } else {
                    title
                }
            })
            .unwrap_or_else(|| {
                compact_label(if self.pending_navigation_restore.is_some() {
                    "thread"
                } else {
                    "new_thread"
                })
            });
        page_surface(window, cx)
            .debug_selector(|| "compact-thread-page".into())
            .child(nav_bar(
                Some(back),
                title.into(),
                subtitle.map(|subtitle| nav_subtitle(subtitle, cx)),
                vec![
                    self.palette_action(cx),
                    nav_icon_button(
                        "compact-panels",
                        crate::tr!("chat.panels").into_owned(),
                        IconName::PanelRight,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_panels(cx);
                    }))
                    .into_any_element(),
                ],
                window,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.layout_part("chat", body)),
            )
            .into_any_element()
    }

    /// The terminal, diff/plan and preview at full width. They are the same
    /// entities the wide layout puts in the split — a compact window has no
    /// room beside the timeline, not less of a product.
    pub(super) fn render_panel_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(column) = &self.column else {
            return div().into_any_element();
        };
        let Some(link) = self.current_link(cx) else {
            return div().into_any_element();
        };
        let store = link.store.read(cx);
        let session = self.page_thread(cx).map(|thread| thread.session_id);
        let panel = store.panel_state_for(session.as_deref());
        let selected_panel = self
            .pending_navigation_restore
            .as_ref()
            .map(|pending| pending.panel.id());
        let terminal = selected_panel.map_or(self.panel_shows_terminal, |id| id == "terminal");
        let show_agents = session.as_deref().is_some_and(|id| {
            !crate::agents_panel::Agents::of(store, id, cx).is_empty()
                || panel.right_tab == RightTab::Agents
                || selected_panel == Some("agents")
        });
        let show_preview = store.offers_preview();
        let composer = store.composer_state_for(session.as_deref(), cx);
        let checkout = composer
            .checkout
            .filter(|_| !composer.conversation_read_only);
        let workspace_store = link.store.clone();
        let page_pane = column.page().clone();
        page_pane.update(cx, |pane, cx| {
            pane.select_compact_preview(
                self.pending_navigation_restore.is_none()
                    && !terminal
                    && panel.right_tab == RightTab::Preview,
                window,
                cx,
            );
        });
        let shell = cx.entity();
        let segments = [
            (
                "terminal",
                crate::tr!("terminal.title").into_owned(),
                terminal,
            ),
            (
                "diff",
                crate::tr!("diff.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Diff, |id| {
                    id == "diff"
                }),
            ),
            (
                "plan",
                crate::tr!("tasks.tab").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Plan, |id| {
                    id == "plan"
                }),
            ),
            (
                "agents",
                crate::tr!("agents.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Agents, |id| {
                    id == "agents"
                }),
            ),
            (
                "pr",
                crate::tr!("pull_requests.title").into_owned(),
                selected_panel.map_or(
                    !terminal && panel.right_tab == RightTab::PullRequests,
                    |id| id == "pr",
                ),
            ),
            (
                "preview",
                crate::tr!("preview.title").into_owned(),
                selected_panel.map_or(!terminal && panel.right_tab == RightTab::Preview, |id| {
                    id == "preview"
                }),
            ),
        ]
        .into_iter()
        .filter(|(id, _, _)| (*id != "preview" || show_preview) && (*id != "agents" || show_agents))
        .map(|(id, label, selected)| {
            let shell = shell.clone();
            TabItem::new(SharedString::from(format!("compact-panel-{id}")), label)
                .selected(selected)
                .on_click(move |_, _, cx| {
                    shell.update(cx, |shell, cx| shell.show_panel(id, cx));
                })
        })
        .collect::<Vec<_>>();
        let segments = TabBar::new("compact-panel-track")
            .segmented()
            .compact(true)
            .items(segments);

        // Each body keeps its own layer, so the one left fades out under the
        // one chosen; a page leaving the Preview fades natively as it is let go.
        let current = if self.pending_navigation_restore.is_some() {
            0
        } else if terminal {
            1
        } else if panel.right_tab == RightTab::Preview {
            2
        } else {
            3
        };
        let fade =
            crate::motion::CrossFade::new("compact-panel-body", current, 4, CHANGE, window, cx);
        let layers: Vec<(usize, AnyElement)> = (0..4)
            .filter(|layer| fade.shows(*layer))
            .map(|layer| {
                let body = match layer {
                    0 => crate::material::loading_skeleton(cx),
                    1 => column
                        .chat(cx)
                        .read(cx)
                        .terminal_drawer()
                        .into_any_element(),
                    2 => page_pane
                        .update(cx, |pane, cx| pane.drawer_preview(window, cx))
                        .into_any_element(),
                    _ => page_pane
                        .update(cx, |pane, cx| pane.drawer_tabs(cx))
                        .into_any_element(),
                };
                (layer, body)
            })
            .collect();
        let body = fade.fill().render(layers).size_full();
        // The thread's workspace and branch, which a compact window's
        // composer leaves to this page; the row carries the space under it.
        let checkout = crate::motion::shown("compact-panel-checkout", checkout, SHOW, window, cx)
            .map(|shown| {
                let row = crate::composer::checkout_row(
                    &workspace_store,
                    session.as_deref(),
                    &shown.value,
                    true,
                    crate::composer::CheckoutRowAt::PanelsHead,
                    cx,
                );
                shown.fold(
                    div()
                        .debug_selector(|| "compact-panel-checkout".into())
                        .w_full()
                        .px(px(crate::material::COMPACT_PAGE_INSET))
                        .pb(cx.theme().tokens.spacing.sm)
                        .child(row),
                )
            });

        page_surface(window, cx)
            .child(nav_bar(
                back,
                crate::tr!("chat.panels").into_owned().into(),
                None,
                vec![],
                window,
                cx,
            ))
            .child(
                div()
                    .flex_none()
                    .px(px(crate::material::COMPACT_PAGE_INSET))
                    .py(cx.theme().tokens.spacing.sm)
                    .child(segments),
            )
            .children(checkout)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }

    pub(super) fn render_pull_request_page(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back = self.back_control(cx);
        let Some(column) = &self.column else {
            return div().into_any_element();
        };
        let view = column.chat(cx).read(cx).pull_request_view(cx);
        let session = self.page_thread(cx).map(|thread| thread.session_id);
        let key = session.as_deref().and_then(|session| {
            self.current_link(cx)?
                .store
                .read(cx)
                .open_pull_request(session)
                .cloned()
        });
        if let (Some(session), Some(key)) = (session, key) {
            view.update(cx, |view, cx| view.show(session, key, cx));
        } else if self.destination(cx) == Destination::PullRequest {
            // The thread no longer shows the pull request: back to the thread.
            let window_state = self.window_state.clone();
            window.defer(cx, move |_, cx| {
                window_state.update(cx, |state, cx| state.back(cx));
            });
        }
        let (title, actions) = view.update(cx, |view, cx| (view.nav_title(), view.nav_actions(cx)));
        page_surface(window, cx)
            .child(nav_bar(back, title, None, actions, window, cx))
            .child(div().flex_1().min_h_0().child(view))
            .into_any_element()
    }

    /// Push the panel destination with something actually on it.
    pub(super) fn open_panels(&mut self, cx: &mut Context<Self>) {
        let showing = self.current_link(cx).is_some_and(|link| {
            let session = self.page_thread(cx).map(|thread| thread.session_id);
            let panel = link.store.read(cx).panel_state_for(session.as_deref());
            if self.panel_shows_terminal {
                panel.terminal_open
            } else {
                panel.right_panel_open
            }
        });
        if !showing {
            self.show_panel(
                if self.panel_shows_terminal {
                    "terminal"
                } else {
                    "diff"
                },
                cx,
            );
        }
        self.go(Destination::Panel, cx);
    }

    pub(super) fn show_panel(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if let Some(pending) = &mut self.pending_navigation_restore {
            pending.panel = match id {
                "terminal" => NavigationPanel::Terminal,
                "plan" => NavigationPanel::Plan,
                "agents" => NavigationPanel::Agents,
                "pr" => NavigationPanel::PullRequests,
                "preview" => NavigationPanel::Preview,
                _ => NavigationPanel::Diff,
            };
            pending.panel_open = true;
        }
        let Some(store) = self.store(cx) else {
            return;
        };
        let session = self.page_thread(cx).map(|thread| thread.session_id);
        let panel = store.read(cx).panel_state_for(session.as_deref());
        self.panel_shows_terminal = id == "terminal";
        store.update(cx, |store, cx| match id {
            "terminal" => {
                if !panel.terminal_open
                    && let Some(session) = &session
                {
                    store.toggle_terminal_panel_for(session.clone(), cx);
                }
            }
            tab => {
                let tab = match tab {
                    "plan" => RightTab::Plan,
                    "agents" => RightTab::Agents,
                    "pr" => RightTab::PullRequests,
                    "preview" => RightTab::Preview,
                    _ => RightTab::Diff,
                };
                if !(panel.right_panel_open && panel.right_tab == tab)
                    && let Some(session) = &session
                {
                    store.toggle_tab_panel_for(session, tab, cx);
                }
            }
        });
        self.schedule_navigation_save(cx);
        cx.notify();
    }

    pub(super) fn begin_edge_back(
        &mut self,
        origin: gpui::Point<Pixels>,
        width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .page_back
            .as_ref()
            .is_some_and(|page| page.target.is_some())
        {
            return;
        }
        let sheet = crate::overlay::active_sheet(window, cx);
        self.page_back = if sheet.is_none()
            && self.compact(cx)
            && !crate::window_seam::keyboard_covers_window(window)
        {
            let mut views = self.nav.read(cx).views().cloned().collect::<Vec<_>>();
            views.pop().and_then(|current| {
                views.pop().map(|previous| PageBack {
                    previous,
                    current,
                    progress: 0.,
                    target: None,
                })
            })
        } else {
            None
        };
        self.back_swipe = Some(BackSwipe {
            origin,
            progress: crate::edge_back::BackProgress {
                width,
                ..Default::default()
            },
            sheet,
            last_moved_at: cx.background_executor().now(),
            finished: false,
        });
    }

    pub(super) fn move_edge_back(
        &mut self,
        phase: TouchPhase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(swipe) = &mut self.back_swipe else {
            return;
        };
        if swipe.finished {
            return;
        }
        let ended = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let commit = phase == TouchPhase::Ended && swipe.progress.commits();
        let progress = swipe.progress.progress();
        if let Some(sheet) = &swipe.sheet {
            let close = sheet.update(cx, |sheet, cx| {
                sheet.dragging = !ended;
                sheet.progress = if ended {
                    if commit { 1. } else { 0. }
                } else {
                    progress
                };
                App::notify(cx, sheet.parent);
                sheet.close.clone()
            });
            if commit {
                close(window, cx);
            }
        } else if let Some(page) = &mut self.page_back {
            page.progress = progress;
            if ended {
                page.target = Some(if commit { 1. } else { 0. });
            }
        } else if commit {
            self.back(window, cx);
        }
        if ended && let Some(swipe) = &mut self.back_swipe {
            swipe.finished = true;
        }
        cx.notify();
    }

    // gpui-base has no edge recognizer; observe GPUI's original pan contact so
    // ordinary taps still reach the Back control. UIKit supplies this on iOS.
    pub(super) fn swipe_back_stack(
        &self,
        stack: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let shell = cx.entity().downgrade();
        // Edge capture must run before ScrollBounce can consume the release packet.
        div().relative().size_full().children([
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, _| {
                    // Claiming TouchDrag would swallow edge taps. GPUI keeps
                    // a pan's original contact in ScrollEvent.position.
                    window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
                        if !phase.capture() {
                            return;
                        }
                        let gpui::ScrollDelta::Pixels(delta) = event.delta else {
                            return;
                        };
                        let Some(shell) = shell.upgrade() else { return };
                        shell.update(cx, |shell, cx| {
                            if delta.y != px(0.) {
                                shell.move_edge_back(TouchPhase::Cancelled, window, cx);
                                return;
                            }
                            if event.touch_phase == TouchPhase::Started {
                                shell.back_swipe = None;
                                if (crate::overlay::active_sheet(window, cx).is_none()
                                    && (!hitbox.should_handle_scroll(window)
                                        || gpui_base::GlobalState::is_in_deferred_context(cx)
                                        || shell.window_state.read(cx).palette_mounted()
                                        || !shell.compact(cx)
                                        || shell.nav.read(cx).depth() <= 1))
                                    || !bounds.contains(&event.position)
                                    || event.position.x - bounds.left()
                                        > crate::edge_back::EDGE_WIDTH
                                {
                                    return;
                                }
                                shell.begin_edge_back(
                                    event.position,
                                    bounds.size.width,
                                    window,
                                    cx,
                                );
                            }
                            let Some(swipe) = shell.back_swipe.as_mut() else {
                                return;
                            };
                            if event.position != swipe.origin {
                                return;
                            }
                            cx.stop_propagation();
                            if swipe.finished {
                                return;
                            }
                            let now = cx.background_executor().now();
                            swipe.progress.travel += delta.x;
                            if event.touch_phase == TouchPhase::Moved && delta.x != px(0.) {
                                let elapsed = now.duration_since(swipe.last_moved_at).as_secs_f32();
                                swipe.progress.velocity = f32::from(delta.x) / elapsed.max(0.001);
                                swipe.last_moved_at = now;
                            }
                            if !matches!(
                                event.touch_phase,
                                TouchPhase::Ended | TouchPhase::Cancelled
                            ) {
                                shell.move_edge_back(event.touch_phase, window, cx);
                            }
                            let Some(swipe) = shell.back_swipe.as_mut() else {
                                return;
                            };
                            if matches!(
                                event.touch_phase,
                                TouchPhase::Ended | TouchPhase::Cancelled
                            ) {
                                if now.duration_since(swipe.last_moved_at)
                                    > Duration::from_millis(100)
                                {
                                    swipe.progress.velocity = 0.;
                                }
                                let origin = swipe.origin;
                                shell.move_edge_back(event.touch_phase, window, cx);
                                if event.touch_phase == TouchPhase::Ended {
                                    crate::edge_back::catch_fling(origin, window, cx);
                                }
                            }
                        });
                    });
                },
            )
            .absolute()
            .inset_0()
            .into_any_element(),
            stack.into_any_element(),
        ])
    }

    pub(super) fn render_compact(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.nav.read(cx).is_empty() {
            self.sync_nav(NavMotion::Immediate, cx);
        }
        if self.pages_changing.take() {
            window.request_animation_frame();
        } else {
            self.keep_lead_beneath(window, cx);
            self.release_leaving(cx);
        }
        let width = window.viewport_size().width;
        // gpui-base exposes NavPage::progress read-only and mounts the
        // previous entry only after pop. Bind interactive progress here, at the
        // navigation owner, until base supplies that API. Retain the same views;
        // compact ChatView excludes the terminal, so Thread and Panel never
        // mount the composer or terminal twice.
        if self.page_back.is_none() {
            crate::edge_back::settle("page-edge-back", 0., true, window, cx);
        }
        let interactive = self.page_back.as_ref().map(|page| {
            let progress = crate::edge_back::settle(
                "page-edge-back",
                page.target.unwrap_or(page.progress),
                page.target.is_none(),
                window,
                cx,
            );
            (
                page.previous.clone(),
                page.current.clone(),
                progress,
                page.target,
            )
        });
        let interactive = interactive.and_then(|(previous, current, progress, target)| {
            if target == Some(progress) {
                self.page_back = None;
                if target == Some(1.) {
                    self.back(window, cx);
                    self.sync_nav(NavMotion::Immediate, cx);
                }
                None
            } else {
                Some((previous, current, progress))
            }
        });
        // 200ms lateral push/pop. GPUI has no paint transform, so each page is
        // offset by its own insets; setting `left` and `right` in opposite
        // directions slides it without resizing it.
        let stack = if let Some((previous, current, progress)) = interactive {
            let offset = crate::edge_back::BackProgress::offset(progress, width);
            let underneath = (offset - width) * 0.25;
            div()
                .relative()
                .size_full()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(underneath)
                        .right(-underneath)
                        .child(previous)
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(gpui::black().opacity(0.15 * (1. - progress))),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(offset)
                        .right(-offset)
                        .child(current),
                )
                .child(div().absolute().inset_0().occlude())
                .into_any_element()
        } else {
            let pages_changing = self.pages_changing.clone();
            NavStack::new(&self.nav)
                .size_full()
                .overflow_hidden()
                .transition(Transition::new(NAV_TRANSITION))
                .item(move |page, _, _| {
                    if page.phase() == PresencePhase::Exiting {
                        pages_changing.set(true);
                    }
                    let rest = 1. - page.progress();
                    let (offset, opacity) = match (page.phase(), page.operation()) {
                        (PresencePhase::Entering, Some(NavOperation::Pop)) => {
                            (width * -0.25 * rest, 1.)
                        }
                        (PresencePhase::Entering, Some(_)) => (width * rest, page.progress()),
                        (PresencePhase::Exiting, Some(NavOperation::Pop)) => {
                            (width * page.progress(), 1.)
                        }
                        (PresencePhase::Exiting, Some(_)) => (width * -0.25 * page.progress(), 1.),
                        _ => (px(0.), 1.),
                    };
                    crate::motion::faded::faded(
                        opacity,
                        page.left(offset).right(px(0.) - offset).opacity(opacity),
                    )
                    .into_any_element()
                })
                .into_any_element()
        };
        v_flex()
            .id("app-shell")
            .relative()
            .size_full()
            .bg(crate::material::content_surface(cx))
            .text_color(cx.theme().foreground)
            .text_token(&cx.theme().tokens.typography.md)
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_open_source_control))
            .key_context("TcodeShell")
            .on_action(cx.listener(Self::on_thread_undo))
            .on_action(cx.listener(Self::on_open_thread_anchor))
            .on_action(cx.listener(Self::on_open_machine_thread_anchor))
            .on_action(cx.listener(Self::on_start_draft_for_project))
            .on_action(cx.listener(Self::on_quick_chat))
            // Every compact page, settings included, is one entry of the same
            // stack: one nav bar, one Back, one transition.
            .child(stack)
            .when(self.current_link(cx).is_some(), |el| {
                el.child(self.palette.clone())
            })
            .into_any_element()
    }
}
