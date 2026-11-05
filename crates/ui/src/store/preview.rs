//! This client's side of the host-held previews: the threads it renders, and
//! each shown thread's preview as the host holds it.
//!
//! One client renders a thread's pages ([`tcode_protocol::PreviewSession`]).
//! Showing a preview no client renders takes it; showing one rendered
//! elsewhere shows where, until the user asks for it here.

use std::collections::{HashMap, HashSet};

use gpui::{App, Context, Entity};
use tcode_protocol::{
    Command, DevServer, PreviewClient, PreviewSession, PreviewTab, ServerEvent, Subscription, Topic,
};

use super::WorkspaceStore;
use super::thread::Revision;

/// The page of a thread is no longer this client's to render: the host
/// handed it to another client, or this one lost its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewReleased {
    pub session_id: String,
}

/// Where a shown thread's preview renders, from this client's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewPlace {
    /// Its pages are this client's.
    Here,
    /// Another client renders them; this one can take them.
    Elsewhere(String),
    /// Not known yet, or this client is asking to render it.
    Settling,
}

pub(super) struct PreviewReplica {
    /// How this client names itself on its [`Topic::Preview`].
    client: PreviewClient,
    /// This client subscribed to its [`Topic::Preview`]: it can render.
    offered: bool,
    sessions: HashMap<String, HeldPreview>,
    rendering: HashSet<String>,
    /// Threads this client asked to render, until the host answers: with
    /// `true` it took them over and renders them meanwhile.
    asked: HashMap<String, bool>,
    /// The host's dev servers of each thread whose picks something here
    /// shows.
    dev_servers: HashMap<String, DevServersShown>,
}

struct DevServersShown {
    /// How many surfaces here show them.
    shown: usize,
    /// None until the host's first read answers.
    servers: Option<Vec<DevServer>>,
    revision: Entity<Revision>,
}

struct HeldPreview {
    session: PreviewSession,
    /// Whether `session` is the host's, rather than only what this client
    /// wrote before the host's arrived.
    replicated: bool,
    revision: Entity<Revision>,
}

impl PreviewReplica {
    pub(super) fn new(name: String) -> Self {
        Self {
            client: PreviewClient {
                id: uuid::Uuid::new_v4().to_string(),
                name,
            },
            offered: false,
            sessions: HashMap::new(),
            rendering: HashSet::new(),
            asked: HashMap::new(),
            dev_servers: HashMap::new(),
        }
    }

    fn held(&mut self, session_id: &str, cx: &mut App) -> &mut HeldPreview {
        self.sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| HeldPreview {
                session: PreviewSession {
                    session_id: session_id.to_owned(),
                    ..PreviewSession::default()
                },
                replicated: false,
                revision: Revision::new(cx),
            })
    }
}

fn topic(session_id: &str) -> Topic {
    Topic::PreviewSession {
        session_id: session_id.to_owned(),
    }
}

fn dev_servers_topic(session_id: &str) -> Topic {
    Topic::DevServers {
        session_id: session_id.to_owned(),
    }
}

impl WorkspaceStore {
    /// Offer this client's pages to the host's agents.
    pub(super) fn offer_preview(&mut self) {
        self.preview.offered = true;
        let _ = self.host.subscribe(Subscription {
            known_index: None,
            topic: Topic::Preview {
                client: self.preview.client.clone(),
            },
            after: None,
        });
    }

    /// Name this client anew on its preview channel; it keeps rendering.
    pub(super) fn rename_preview_client(&mut self) {
        let name = self.client_device_name();
        if !self.preview.offered || self.preview.client.name == name {
            self.preview.client.name = name;
            return;
        }
        let id = self.preview.client.id.clone();
        let old = std::mem::replace(&mut self.preview.client, PreviewClient { id, name });
        self.offer_preview();
        let _ = self.host.unsubscribe(Subscription {
            known_index: None,
            topic: Topic::Preview { client: old },
            after: None,
        });
    }

    /// Follow the host's preview of a thread a surface shows. Its replica
    /// is held from now on, so what reads it is told when it arrives.
    pub(super) fn follow_preview(&mut self, session_id: &str, cx: &mut App) {
        if self.offers_preview() {
            self.preview.held(session_id, cx);
            let _ = self.host.subscribe(Subscription {
                known_index: None,
                topic: topic(session_id),
                after: None,
            });
        }
    }

    pub(super) fn unfollow_preview(&mut self, session_id: &str) {
        if self.offers_preview() {
            // Unfollowed, what is held of it goes stale.
            if !self.preview.rendering.contains(session_id)
                && let Some(held) = self.preview.sessions.get_mut(session_id)
            {
                held.replicated = false;
            }
            let _ = self.host.unsubscribe(Subscription {
                known_index: None,
                topic: topic(session_id),
                after: None,
            });
        }
    }

    /// Apply a preview event: the threads whose replica changed.
    pub(super) fn apply_preview_event(
        &mut self,
        topic: &Topic,
        event: &ServerEvent,
        cx: &mut Context<Self>,
    ) -> Option<Vec<String>> {
        match (topic, event) {
            (
                Topic::PreviewSession { session_id },
                ServerEvent::PreviewSessionReplaced(session),
            ) if session.session_id == *session_id => {
                if self.preview.asked.get(session_id) == Some(&false) {
                    self.preview.asked.remove(session_id);
                }
                let rendering = self.preview.rendering.contains(session_id);
                let held = self.preview.held(session_id, cx);
                // The rendering client's own pages are newer than the host's
                // echo of them.
                let tabs = if rendering {
                    std::mem::take(&mut held.session.tabs)
                } else {
                    session.tabs.clone()
                };
                held.session = PreviewSession {
                    tabs,
                    ..session.clone()
                };
                held.replicated = true;
                Some(vec![session_id.clone()])
            }
            (Topic::Preview { client }, ServerEvent::PreviewRenderingReplaced(sessions))
                if client.id == self.preview.client.id =>
            {
                let mut changed = Vec::new();
                for session in sessions {
                    let id = &session.session_id;
                    self.preview.asked.remove(id);
                    let adopt = self.preview.rendering.insert(id.clone());
                    let held = self.preview.held(id, cx);
                    // Taken over here, its pages already are this client's.
                    if adopt {
                        held.session = session.clone();
                        held.replicated = true;
                    } else {
                        held.session.renderer = session.renderer.clone();
                    }
                    changed.push(id.clone());
                }
                let released: Vec<String> = self
                    .preview
                    .rendering
                    .iter()
                    .filter(|id| {
                        !sessions.iter().any(|session| &session.session_id == *id)
                            && self.preview.asked.get(*id) != Some(&true)
                    })
                    .cloned()
                    .collect();
                for session_id in released {
                    self.preview.rendering.remove(&session_id);
                    cx.emit(PreviewReleased {
                        session_id: session_id.clone(),
                    });
                    changed.push(session_id);
                }
                Some(changed)
            }
            (Topic::DevServers { session_id }, ServerEvent::DevServersReplaced(servers)) => {
                if let Some(shown) = self.preview.dev_servers.get_mut(session_id) {
                    shown.servers = Some(servers.clone());
                    Revision::bump(&shown.revision, cx);
                }
                // Their readers watch the list's own revision.
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Show the host's dev servers for `session_id` on a surface here,
    /// until [`Self::hide_dev_servers`]. The host reads its listeners only
    /// while some client shows them.
    pub(crate) fn show_dev_servers(&mut self, session_id: &str, cx: &mut App) {
        if !self.scope.is_full() {
            return;
        }
        let shown = self
            .preview
            .dev_servers
            .entry(session_id.to_owned())
            .or_insert_with(|| DevServersShown {
                shown: 0,
                servers: None,
                revision: Revision::new(cx),
            });
        shown.shown += 1;
        if shown.shown == 1 {
            let _ = self.host.subscribe(Subscription {
                known_index: None,
                topic: dev_servers_topic(session_id),
                after: None,
            });
        }
    }

    pub(crate) fn hide_dev_servers(&mut self, session_id: &str) {
        let Some(shown) = self.preview.dev_servers.get_mut(session_id) else {
            return;
        };
        shown.shown -= 1;
        if shown.shown == 0 {
            self.preview.dev_servers.remove(session_id);
            let _ = self.host.unsubscribe(Subscription {
                known_index: None,
                topic: dev_servers_topic(session_id),
                after: None,
            });
        }
    }

    /// The host's dev servers for `session_id`, most relevant first, once
    /// the host answered, read as a view depending on them.
    #[cfg(preview_backend)]
    pub(crate) fn dev_servers(&self, session_id: &str, cx: &App) -> Option<Vec<DevServer>> {
        let shown = self.preview.dev_servers.get(session_id)?;
        Revision::watch(&shown.revision, cx);
        shown.servers.clone()
    }

    pub(super) fn bump_preview(&self, session_id: &str, cx: &mut App) {
        if let Some(held) = self.preview.sessions.get(session_id) {
            Revision::bump(&held.revision, cx);
        }
    }

    /// Whether this client renders `session_id`'s pages.
    #[cfg(preview_backend)]
    pub(crate) fn renders_preview(&self, session_id: &str) -> bool {
        self.preview.rendering.contains(session_id)
    }

    /// Where `session_id`'s preview renders, read as a view depending on it.
    pub(crate) fn preview_place(&self, session_id: &str, cx: &App) -> PreviewPlace {
        let held = self.preview.sessions.get(session_id);
        if let Some(held) = held {
            Revision::watch(&held.revision, cx);
        }
        if self.preview.rendering.contains(session_id) {
            return PreviewPlace::Here;
        }
        let Some(held) = held else {
            return PreviewPlace::Settling;
        };
        match &held.session.renderer {
            Some(renderer) if held.replicated && renderer.id != self.preview.client.id => {
                PreviewPlace::Elsewhere(renderer.name.clone())
            }
            _ => PreviewPlace::Settling,
        }
    }

    /// The thread's preview is shown here: render it, unless another client
    /// does.
    pub(crate) fn claim_preview(&mut self, session_id: &str) {
        if !self.preview.offered
            || self.preview.rendering.contains(session_id)
            || self.preview.asked.contains_key(session_id)
            || !self
                .preview
                .sessions
                .get(session_id)
                .is_some_and(|held| held.replicated && held.session.renderer.is_none())
        {
            return;
        }
        self.preview.asked.insert(session_id.to_owned(), false);
        self.dispatch(Command::ShowPreviewHere {
            session_id: session_id.to_owned(),
            client_id: self.preview.client.id.clone(),
            take_over: false,
        });
    }

    /// Render the thread's preview here, taking it from any client that
    /// does.
    pub(crate) fn show_preview_here(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if !self.preview.offered || self.preview.rendering.contains(session_id) {
            return;
        }
        self.preview.held(session_id, cx);
        self.preview.asked.insert(session_id.to_owned(), true);
        self.preview.rendering.insert(session_id.to_owned());
        self.dispatch(Command::ShowPreviewHere {
            session_id: session_id.to_owned(),
            client_id: self.preview.client.id.clone(),
            take_over: true,
        });
        self.bump_preview(session_id, cx);
        cx.notify();
    }

    /// The user acted on the page this client renders for `session_id`,
    /// which the agent's next status reports.
    pub(crate) fn report_preview_input(&mut self, session_id: &str) {
        if self.preview.rendering.contains(session_id) {
            self.dispatch(Command::ReportPreviewInput {
                session_id: session_id.to_owned(),
                client_id: self.preview.client.id.clone(),
            });
        }
    }

    /// The user acted inside the page `key` names.
    #[cfg(preview_backend)]
    pub(crate) fn report_page_input(&mut self, key: &str) {
        let session_id = self.preview_session_of(key);
        self.report_preview_input(&session_id);
    }

    /// The thread whose preview `key` names: a project draft's is its
    /// open draft thread's.
    fn preview_session_of(&self, key: &str) -> String {
        self.threads
            .iter()
            .find(|(_, thread)| {
                thread
                    .destination
                    .as_ref()
                    .is_some_and(|destination| destination.preference_key() == key)
            })
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| key.to_owned())
    }

    fn preview_tab(&self, key: &str) -> Option<&PreviewTab> {
        self.preview
            .sessions
            .get(&self.preview_session_of(key))?
            .session
            .tabs
            .first()
    }

    /// Change the page of the preview `key` names, which another client's
    /// rendering leaves alone; the host learns of it from the rendering
    /// client.
    fn edit_preview_tabs(
        &mut self,
        key: &str,
        edit: impl FnOnce(&mut Vec<PreviewTab>),
        cx: &mut Context<Self>,
    ) {
        let session_id = self.preview_session_of(key);
        if matches!(
            self.preview_place(&session_id, cx),
            PreviewPlace::Elsewhere(_)
        ) {
            return;
        }
        let held = self.preview.held(&session_id, cx);
        let before = held.session.tabs.clone();
        edit(&mut held.session.tabs);
        held.session
            .tabs
            .retain(|tab| tab.url.is_some() || tab.canvas.is_some());
        if held.session.tabs == before {
            return;
        }
        let tabs = held.session.tabs.clone();
        if self.preview.rendering.contains(&session_id) {
            self.dispatch(Command::ReportPreviewTabs {
                session_id: session_id.clone(),
                client_id: self.preview.client.id.clone(),
                tabs,
            });
        }
        self.bump_preview(&session_id, cx);
        cx.notify();
    }

    pub fn preview_url(&self, key: &str) -> Option<String> {
        self.preview_tab(key)?.url.clone()
    }

    pub fn set_preview_url(&mut self, key: &str, url: String, cx: &mut Context<Self>) {
        self.edit_preview_tabs(
            key,
            |tabs| match tabs.first_mut() {
                Some(tab) => tab.url = Some(url),
                None => tabs.push(PreviewTab {
                    url: Some(url),
                    canvas: None,
                }),
            },
            cx,
        );
    }

    pub fn preview_canvas(&self, key: &str) -> Option<(u32, u32)> {
        self.preview_tab(key)?.canvas
    }

    pub fn set_preview_canvas(
        &mut self,
        key: &str,
        canvas: Option<(u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        self.edit_preview_tabs(
            key,
            |tabs| match tabs.first_mut() {
                Some(tab) => tab.canvas = canvas,
                None => tabs.push(PreviewTab { url: None, canvas }),
            },
            cx,
        );
    }

    pub fn clear_preview_chrome(&mut self, key: &str, cx: &mut Context<Self>) {
        self.edit_preview_tabs(key, Vec::clear, cx);
    }
}
