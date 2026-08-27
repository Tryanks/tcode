use super::*;

impl AppState {
    /// Reroute provider-native subagent transcript items into read-only mirror
    /// sessions. Returns true when the event was consumed instead of entering
    /// the parent session's ordinary event path.
    pub(super) fn reroute_native_subagent_event(
        &mut self,
        parent_session_id: &str,
        event: &AgentEvent,
        cx: &mut HostCx,
    ) -> bool {
        let Some(item) = lifecycle_item(event) else {
            return false;
        };

        // Check child ownership before content: Codex child transcript items can
        // themselves be Subagent-shaped, but they remain plain mirror content.
        if let Some(parent_item_id) = item.parent_item_id.as_deref() {
            let mirror_id =
                self.ensure_native_subagent_mirror(parent_session_id, parent_item_id, None, cx);
            let Some(mirror_id) = mirror_id else {
                log::warn!(
                    "dropping native subagent child item {}: parent session {} is missing",
                    item.id,
                    parent_session_id
                );
                return true;
            };
            self.sync_mirror_turn(&mirror_id, true, parent_item_id, TurnStatus::Completed, cx);
            self.record_event(&mirror_id, &strip_parent_item_id(event), cx);
            return true;
        }

        let ItemContent::Subagent {
            agent_type,
            description,
            status,
            ..
        } = &item.content
        else {
            return false;
        };
        let details = (agent_type.as_str(), description.as_str());
        if let Some(mirror_id) =
            self.ensure_native_subagent_mirror(parent_session_id, &item.id, Some(details), cx)
        {
            let in_progress = matches!(status, ItemStatus::InProgress);
            let title = mirror_title(agent_type, description);
            let meta = self.resident_mut(&mirror_id).map(|mirror| {
                mirror.turn_in_flight = in_progress;
                let title_changed = mirror.meta.title == "subagent" && mirror.meta.title != title;
                if title_changed {
                    mirror.meta.title = title;
                }
                if !in_progress || title_changed {
                    mirror.meta.updated_at = now_secs();
                    Some(mirror.meta.clone())
                } else {
                    None
                }
            });
            if let Some(meta) = meta.flatten() {
                self.persist_meta(&meta, cx);
            }
            let turn_status = match status {
                ItemStatus::Failed | ItemStatus::Declined => TurnStatus::Failed,
                ItemStatus::Interrupted => TurnStatus::Interrupted,
                _ => TurnStatus::Completed,
            };
            self.sync_mirror_turn(&mirror_id, in_progress, &item.id, turn_status, cx);
        }
        false
    }

    /// Mirrors never receive provider Turn events, but the chat view derives
    /// its working indicator, live timer, and live work-log expansion from
    /// timeline turn state — so synthesize the boundaries from the parent
    /// Subagent item's lifecycle.
    fn sync_mirror_turn(
        &mut self,
        mirror_id: &str,
        running: bool,
        subagent_item_id: &str,
        status: TurnStatus,
        cx: &mut HostCx,
    ) {
        let open = self
            .resident(mirror_id)
            .is_some_and(|mirror| mirror.timeline.turn_running);
        if running && !open {
            self.record_event(
                mirror_id,
                &AgentEvent::TurnStarted {
                    turn_id: subagent_item_id.to_string(),
                },
                cx,
            );
        } else if !running && open {
            self.record_event(
                mirror_id,
                &AgentEvent::TurnCompleted {
                    turn_id: subagent_item_id.to_string(),
                    status,
                    usage: None,
                },
                cx,
            );
        }
    }

    fn ensure_native_subagent_mirror(
        &mut self,
        parent_session_id: &str,
        subagent_item_id: &str,
        details: Option<(&str, &str)>,
        cx: &mut HostCx,
    ) -> Option<String> {
        let key = (parent_session_id.to_string(), subagent_item_id.to_string());
        if let Some(id) = self.native_subagent_sessions.get(&key).cloned() {
            return Some(id);
        }

        if let Some(meta) = self
            .sessions
            .iter()
            .find(|meta| {
                meta.parent_session_id.as_deref() == Some(parent_session_id)
                    && meta.native_subagent.as_deref() == Some(subagent_item_id)
            })
            .cloned()
        {
            let id = meta.id.clone();
            if self.resident(&id).is_none() {
                self.load_background_session(meta, cx);
            }
            self.native_subagent_sessions.insert(key, id.clone());
            return Some(id);
        }

        let parent = self.find_meta(parent_session_id)?.clone();
        let mut meta = SessionMeta::new(parent.provider, parent.cwd.clone(), parent.model.clone());
        meta.project_id = parent.project_id.clone();
        meta.profile_id = parent.profile_id.clone();
        meta.acp_agent_id = parent.acp_agent_id.clone();
        meta.parent_session_id = Some(parent.id.clone());
        meta.native_subagent = Some(subagent_item_id.to_string());
        meta.title = details.map_or_else(
            || "subagent".to_string(),
            |(agent_type, description)| mirror_title(agent_type, description),
        );

        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: true,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());

        let id = meta.id.clone();
        let commands = self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut mirror = Self::build_draft_session(
            meta.project_id.clone().unwrap_or_default(),
            meta.cwd.clone(),
            meta.provider,
            meta.model.clone(),
            meta.acp_agent_id.clone(),
            commands,
        );
        mirror.meta = meta;
        mirror.draft = false;
        mirror.turn_in_flight = true;
        self.residents.parked.insert(id.clone(), mirror);
        self.native_subagent_sessions.insert(key, id.clone());
        Some(id)
    }

    pub(super) fn clear_native_subagent_work(&mut self, parent_session_id: &str, cx: &mut HostCx) {
        let mirror_ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(parent_session_id)
                    && meta.native_subagent.is_some()
            })
            .map(|meta| (meta.id.clone(), meta.native_subagent.clone().unwrap()))
            .collect();
        for (mirror_id, subagent_item_id) in mirror_ids {
            self.sync_mirror_turn(
                &mirror_id,
                false,
                &subagent_item_id,
                TurnStatus::Completed,
                cx,
            );
            let meta = self.resident_mut(&mirror_id).and_then(|mirror| {
                if !mirror.turn_in_flight {
                    return None;
                }
                mirror.turn_in_flight = false;
                mirror.meta.updated_at = now_secs();
                Some(mirror.meta.clone())
            });
            if let Some(meta) = meta {
                self.persist_meta(&meta, cx);
            }
        }
    }
}

fn lifecycle_item(event: &AgentEvent) -> Option<&ThreadItem> {
    match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => Some(item),
        _ => None,
    }
}

fn strip_parent_item_id(event: &AgentEvent) -> AgentEvent {
    let strip = |item: &ThreadItem| {
        let mut item = item.clone();
        item.parent_item_id = None;
        item
    };
    match event {
        AgentEvent::ItemStarted(item) => AgentEvent::ItemStarted(strip(item)),
        AgentEvent::ItemUpdated(item) => AgentEvent::ItemUpdated(strip(item)),
        AgentEvent::ItemCompleted(item) => AgentEvent::ItemCompleted(strip(item)),
        _ => unreachable!("only lifecycle events are rerouted"),
    }
}

fn mirror_title(agent_type: &str, description: &str) -> String {
    let first_line = description.lines().next().unwrap_or_default().trim();
    let title = format!("{agent_type}: {first_line}");
    let mut chars = title.chars();
    let truncated: String = chars.by_ref().take(60).collect();
    if chars.next().is_some() {
        format!("{}…", truncated.trim_end())
    } else {
        truncated
    }
}
