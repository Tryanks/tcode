use super::*;

/// Live sessions owned by the runtime, whether currently selected or parked.
#[derive(Default)]
pub struct ResidentSessions {
    pub active: Option<ActiveSession>,
    pub(super) parked: HashMap<String, ActiveSession>,
}

impl ResidentSessions {
    pub(super) fn resident(&self, id: &str) -> Option<&ActiveSession> {
        self.active
            .as_ref()
            .filter(|session| session.meta.id == id)
            .or_else(|| self.parked.get(id))
    }

    pub(super) fn resident_mut(&mut self, id: &str) -> Option<&mut ActiveSession> {
        if self
            .active
            .as_ref()
            .is_some_and(|session| session.meta.id == id)
        {
            return self.active.as_mut();
        }
        self.parked.get_mut(id)
    }

    pub(super) fn park(&mut self, session: ActiveSession) {
        self.parked.insert(session.meta.id.clone(), session);
    }

    pub(super) fn adopt(&mut self, id: &str) -> Option<ActiveSession> {
        self.parked.remove(id)
    }

    pub(super) fn evict(&mut self, id: &str) -> Option<ActiveSession> {
        self.parked.remove(id)
    }

    pub(super) fn ids(&self) -> impl Iterator<Item = &str> {
        self.active
            .iter()
            .map(|session| session.meta.id.as_str())
            .chain(self.parked.keys().map(String::as_str))
    }
}

impl AppState {
    /// Assemble the provider-bound message at the runtime boundary. Unreadable
    /// attachment files are skipped, matching the composer's previous behavior.
    pub(super) fn assemble_user_message(
        &self,
        text: String,
        attachment_paths: Vec<PathBuf>,
    ) -> (String, Vec<Attachment>) {
        let terminal_contexts = self
            .residents
            .active
            .as_ref()
            .map(|active| active.terminal_workspace.contexts.as_slice())
            .unwrap_or_default();
        let text = append_terminal_contexts_to_prompt(&text, terminal_contexts);
        let text = append_review_comments_to_prompt(&text, self.review_comments());
        let attachments = attachment_paths
            .into_iter()
            .filter_map(|path| {
                let bytes = fs::read(&path).ok()?;
                Some(Attachment {
                    media_type: mime_from_path(&path),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    source_path: Some(path.to_string_lossy().into_owned()),
                })
            })
            .collect();
        (text, attachments)
    }

    pub(super) fn clear_consumed_draft_context(&mut self, cx: &mut HostCx) {
        self.clear_terminal_contexts();
        self.clear_review_comments(cx);
    }

    /// Cycle the sidebar PROJECTS ordering and persist it.
    pub fn cycle_project_sort(&mut self, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        settings.project_sort = settings.project_sort.next();
        self.update_settings(settings, cx);
    }

    /// Create a project rooted at `root` (native picker feeds this).
    /// Returns the new project's id, or an existing one if `root` matches.
    pub fn create_project(&mut self, root: PathBuf, cx: &mut HostCx) -> Option<String> {
        if let Some(existing) = self.projects.iter().find(|p| p.root == root) {
            return Some(existing.id.clone());
        }
        let project = Project::from_root(root);
        let id = project.id.clone();
        self.enqueue_store_write(StoreWrite::UpsertProject(project.clone()), cx);
        self.projects.push(project);
        Some(id)
    }

    /// Scan supported external-agent histories without exposing the import
    /// service or application stores to callers.
    pub fn scan_external_history(&self, executor: &HostCx) -> HostTask<Vec<RecentDir>> {
        let exclude: Vec<_> = self
            .projects
            .iter()
            .map(|project| project.root.clone())
            .collect();
        let sessions = self.sessions.clone();
        executor.unblock(move || {
            let known = existing_external_ids(&sessions);
            let mut recent = scan_recent_dirs(&ExternalRoots::detect(), &exclude);
            for dir in &mut recent {
                dir.threads
                    .retain(|thread| !known.contains(&thread.external_id));
            }
            recent.retain(|dir| !dir.threads.is_empty());
            recent
        })
    }

    /// Import selected external threads in the background and stream runtime-
    /// owned progress updates. Returns `None` for an unknown project.
    pub fn start_external_import(
        &self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        executor: &HostCx,
    ) -> Option<smol::channel::Receiver<ExternalImportUpdate>> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)?
            .clone();
        let store = self.store.clone();
        let metas = self.sessions.clone();
        let (sender, receiver) = smol::channel::unbounded();
        executor
            .unblock(move || {
                let total = threads.len();
                let mut imported = 0;
                let mut skipped = 0;
                let mut existing = existing_external_ids(&metas);
                for (index, thread) in threads.into_iter().enumerate() {
                    let tool = thread.source.display_name().to_string();
                    match import_thread(&store, &project, &thread, &mut existing) {
                        ImportOutcome::Imported => imported += 1,
                        ImportOutcome::SkippedDuplicate
                        | ImportOutcome::SkippedEmpty
                        | ImportOutcome::Failed(_) => skipped += 1,
                    }
                    let _ = sender.try_send(ExternalImportUpdate::Progress {
                        done: index + 1,
                        total,
                        tool,
                    });
                }
                let _ = sender.try_send(ExternalImportUpdate::Finished { imported, skipped });
            })
            .detach();
        Some(receiver)
    }

    /// List one replicated session cwd on the background executor.
    pub fn list_workspace_at(
        &self,
        cwd: Option<PathBuf>,
        executor: &HostCx,
    ) -> HostTask<Vec<PathEntry>> {
        executor.unblock(move || cwd.map(|cwd| list_workspace(&cwd)).unwrap_or_default())
    }

    /// Reload sessions written by the external-history importer and expand its
    /// project group.
    pub fn finish_external_import(&mut self, project_id: &str, cx: &mut HostCx) {
        self.sessions = self.store.load_index();
        if self
            .settings
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
        {
            let mut settings = self.settings.clone();
            settings.collapsed_projects.retain(|id| id != project_id);
            self.update_settings(settings, cx);
        }
    }

    /// Toggle a project's collapsed state (persisted in settings).
    pub fn toggle_project_collapsed(&mut self, project_id: &str, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings
            .collapsed_projects
            .iter()
            .position(|id| id == project_id)
        {
            settings.collapsed_projects.remove(pos);
        } else {
            settings.collapsed_projects.push(project_id.to_string());
        }
        self.update_settings(settings, cx);
    }

    pub fn active_session_id(&self) -> Option<&str> {
        self.residents.active.as_ref().map(|a| a.meta.id.as_str())
    }

    pub(crate) fn active_session(&self) -> Option<&ActiveSession> {
        self.residents.active.as_ref()
    }

    pub(super) fn resident(&self, id: &str) -> Option<&ActiveSession> {
        self.residents.resident(id)
    }

    pub(super) fn resident_mut(&mut self, id: &str) -> Option<&mut ActiveSession> {
        self.residents.resident_mut(id)
    }

    pub(super) fn find_meta(&self, id: &str) -> Option<SessionMeta> {
        self.sessions
            .iter()
            .find(|meta| meta.id == id)
            .cloned()
            .or_else(|| self.resident(id).map(|session| session.meta.clone()))
    }

    /// Directory where one session's image attachments are persisted.
    pub(crate) fn attachments_dir_for(&self, session_id: &str) -> PathBuf {
        user_files::attachment_dir(self.store.root(), session_id)
    }

    /// Persist attachment bytes to a previously captured active-session target.
    /// Callers run this blocking helper on the background executor.
    pub fn save_attachment_to_dir(dir: &Path, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    pub fn update_settings(&mut self, settings: Settings, cx: &mut HostCx) {
        self.enqueue_settings(&settings, cx);
        let language = settings.language.clone();
        self.settings = settings;
        self.providers.provider_secret_names =
            provider_secret_names(&self.settings, &self.settings_store);
        // Keep the live computer-use MCP config in step with the persisted
        // settings on every change (the server outlives any one snapshot).
        computer_use_mcp::config::set(self.settings.computer_use.clone());
        emit_runtime(
            cx,
            RuntimeEvent::Effect(RuntimeEffect::ApplyLocale { language }),
        );
    }

    pub fn patch_settings(&mut self, patch: tcode_protocol::SettingsPatch, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        match patch {
            tcode_protocol::SettingsPatch::Language(value) => settings.language = value,
            tcode_protocol::SettingsPatch::ThemeMode(value) => settings.theme_mode = value,
            tcode_protocol::SettingsPatch::WordWrapDiffs(value) => settings.word_wrap_diffs = value,
            tcode_protocol::SettingsPatch::SkipDeleteConfirmation(value) => {
                settings.skip_delete_confirmation = value;
            }
            tcode_protocol::SettingsPatch::AutoOpenTaskPanel(value) => {
                settings.auto_open_task_panel = value;
            }
            tcode_protocol::SettingsPatch::ProviderUpdateChecksDisabled(value) => {
                settings.provider_update_checks_disabled = value;
            }
            tcode_protocol::SettingsPatch::AutoArchiveDisabled(value) => {
                settings.auto_archive_disabled = value;
            }
            tcode_protocol::SettingsPatch::AutoArchiveMaxIdleDays(value) => {
                settings.auto_archive_max_idle_days = value;
            }
            tcode_protocol::SettingsPatch::AutoArchiveKeepCount(value) => {
                settings.auto_archive_keep_count = value;
            }
            tcode_protocol::SettingsPatch::AutoArchiveNoticeShown(value) => {
                settings.auto_archive_notice_shown = value;
            }
            tcode_protocol::SettingsPatch::Orchestrate(value) => settings.orchestrate = value,
            tcode_protocol::SettingsPatch::ComputerUse(value) => settings.computer_use = value,
            tcode_protocol::SettingsPatch::Browser(value) => settings.browser = value,
            tcode_protocol::SettingsPatch::TitleGeneration(value) => {
                settings.title_generation = value;
            }
            tcode_protocol::SettingsPatch::SidebarLayout(value) => settings.sidebar_layout = value,
        }
        self.update_settings(settings, cx);
    }

    /// Persist a restart-continuity marker naming the Settings page to reopen and
    /// the session that is active now. Written *before* a permission grant or an
    /// explicit relaunch, so an externally-initiated quit reopens cleanly.
    pub fn write_relaunch_marker(&self, reopen_settings: &str) {
        let marker = tcode_services::relaunch::RelaunchMarker {
            reopen_settings: reopen_settings.to_string(),
            active_session: self.active_session_id().map(str::to_string),
        };
        if let Err(err) = tcode_services::relaunch::write(self.store.root(), &marker) {
            log::warn!("failed to write relaunch marker: {err}");
        }
    }

    /// Apply a marker taken at launch: reopen the recorded session and open
    /// Settings on the recorded page. The page reruns a permission recheck as it
    /// mounts, so the user immediately sees the post-restart status. No-op when
    /// there is no marker (the normal launch path).
    pub fn apply_pending_relaunch(&mut self, cx: &mut HostCx) -> Option<String> {
        let marker = self.pending_relaunch.take()?;
        if let Some(id) = marker.active_session.as_deref()
            && self.sessions.iter().any(|meta| meta.id == id)
        {
            self.select_session(id, cx);
        }
        Some(marker.reopen_settings)
    }

    // -- archive / delete / rename / unread (Group A) -----------------------

    /// Archive a thread (reversible; it vanishes from the sidebar). Blocked while
    /// its turn is running (returns without changing anything so the caller's
    /// tooltip stands). The active thread is closed back to the empty state.
    pub fn archive_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.turn_running_for(session_id)
            || self
                .sessions
                .iter()
                .find(|meta| meta.id == session_id)
                .is_none_or(|meta| meta.archived_at.is_some())
        {
            return;
        }
        let ids = descendant_session_ids(&self.sessions, session_id);
        self.archive_session_ids(&ids, now_secs(), cx);
    }

    /// Restore an archived thread (Settings → Archived Threads → Unarchive).
    pub fn unarchive_session(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(archived_at) = self
            .sessions
            .iter()
            .find(|meta| meta.id == session_id)
            .and_then(|meta| meta.archived_at)
        else {
            return;
        };
        let ids = descendant_session_ids(&self.sessions, session_id);
        for id in ids {
            let Some(meta) = self
                .sessions
                .iter_mut()
                .find(|meta| meta.id == id && meta.archived_at == Some(archived_at))
            else {
                continue;
            };
            meta.archived_at = None;
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    /// Sweep one project's visible sessions using the configured idle and
    /// sibling keep windows. Returns the number of threads archived.
    pub fn auto_archive_sweep(&mut self, project_id: &str, cx: &mut HostCx) -> usize {
        if self.settings.auto_archive_disabled {
            return 0;
        }
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(project_id) && meta.archived_at.is_none()
            })
            .cloned()
            .collect();
        let exemptions = AutoArchiveExemptions {
            working: sessions
                .iter()
                .filter(|meta| self.turn_running_for(&meta.id))
                .map(|meta| meta.id.clone())
                .collect(),
            unread: sessions
                .iter()
                .filter(|meta| self.session_unread(&meta.id))
                .map(|meta| meta.id.clone())
                .collect(),
            active: self
                .active_session_id()
                .map(str::to_string)
                .into_iter()
                .collect(),
        };
        let config = AutoArchiveConfig {
            max_idle_secs: u64::from(self.settings.auto_archive_max_idle_days.max(1)) * 86_400,
            keep_count: self.settings.auto_archive_keep_count.max(1),
        };
        let ids = auto_archive_candidates(&sessions, now_secs(), &config, &exemptions);
        let count = ids.len();
        if count > 0 {
            self.archive_session_ids(&ids, now_secs(), cx);
        }
        count
    }

    pub(super) fn archive_session_ids(
        &mut self,
        ids: &[String],
        archived_at: u64,
        cx: &mut HostCx,
    ) {
        let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
        if self
            .active_session_id()
            .is_some_and(|active| ids.contains(active))
        {
            self.shutdown_active(cx);
        }
        for id in ids.iter().copied() {
            // An archived conversation must not leave an off-screen PTY running.
            self.terminal_workspaces
                .remove(&ConversationDestination::Thread(id.to_string()));
            self.drop_background(id, cx);
            self.revoke_preview_registration(id);
        }
        let orchestrators: Vec<_> = ids.iter().map(|id| (*id).to_string()).collect();
        for id in orchestrators {
            self.close_orchestrator_children(&id, cx);
        }
        let mut changed = Vec::new();
        for meta in &mut self.sessions {
            if ids.contains(meta.id.as_str()) {
                meta.archived_at = Some(archived_at);
                changed.push(meta.clone());
            }
        }
        for meta in changed {
            self.persist_meta(&meta, cx);
        }
    }

    /// Rename a thread (context-menu inline edit). Empty titles are rejected.
    pub fn rename_session(&mut self, session_id: &str, title: &str, cx: &mut HostCx) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if let Some(session) = self.resident_mut(session_id) {
            session.meta.title = title.to_string();
        }
        if let Some(meta) = self.sessions.iter_mut().find(|m| m.id == session_id) {
            meta.title = title.to_string();
            meta.updated_at = now_secs();
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    /// Duplicate a stored transcript and arrange for its next provider start to
    /// fork the source's native session. The fork stays idle until its first
    /// user turn, exactly like a cold-opened stored thread.
    pub fn fork_thread(&mut self, id: &str, cx: &mut HostCx) {
        let source = self
            .residents
            .active
            .as_ref()
            .filter(|session| session.meta.id == id)
            .map(|session| (session.meta.clone(), session.turn_in_flight))
            .or_else(|| {
                self.residents
                    .parked
                    .get(id)
                    .map(|session| (session.meta.clone(), session.turn_in_flight))
            })
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|meta| meta.id == id)
                    .cloned()
                    .map(|meta| (meta, false))
            });
        let Some((source, turn_in_flight)) = source else {
            return;
        };
        if !source.provider.supports_fork() {
            self.report_error(
                RuntimeError::External("This provider does not support conversation forks.".into()),
                cx,
            );
            return;
        }
        if source.resume_cursor.is_none() {
            self.report_error(
                RuntimeError::External("This conversation is empty and cannot be forked.".into()),
                cx,
            );
            return;
        }
        if turn_in_flight {
            self.report_error(
                RuntimeError::External(
                    "Wait for the running turn to finish before forking this conversation.".into(),
                ),
                cx,
            );
            return;
        }

        let mut fork = SessionMeta::new(source.provider, source.cwd.clone(), source.model.clone());
        fork.title = format!("{} (fork)", source.title);
        fork.option_selections = source.option_selections.clone();
        fork.approval_mode = source.approval_mode;
        fork.interaction_mode = source.interaction_mode;
        fork.project_id = source.project_id.clone();
        fork.acp_agent_id = source.acp_agent_id.clone();
        fork.profile_id = source.profile_id.clone();
        fork.resume_cursor = source.resume_cursor.clone();
        fork.pending_fork = true;
        // `worktree` deliberately stays absent: it is an ownership/cleanup
        // marker. The cwd may be shared, but the fork must not own the source's
        // generated worktree or offer to delete it.

        let (completion, completed) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::CloneEvents {
                src: source.id,
                dst: fork.id.clone(),
                completion,
            },
            cx,
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = completed
                .recv()
                .await
                .unwrap_or_else(|_| Err("session store writer stopped".into()));
            host_cx.enqueue(move |state, cx| match result {
                Ok(()) => {
                    state.enqueue_store_write(
                        StoreWrite::UpsertMeta {
                            meta: Box::new(fork.clone()),
                            initial: true,
                        },
                        cx,
                    );
                    state.upsert_session_in_memory(fork.clone());
                    state.select_session(&fork.id, cx);
                }
                Err(error) => {
                    state.report_error(RuntimeError::PersistEvent { error }, cx);
                }
            });
        });
    }

    /// Permanently delete a thread: stop the provider, close its terminal,
    /// delete meta + JSONL, and (when `remove_worktree`) remove the git worktree
    /// it was the last user of.
    pub fn delete_session(&mut self, session_id: &str, remove_worktree: bool, cx: &mut HostCx) {
        self.clear_approvals(session_id);
        let meta = self.sessions.iter().find(|m| m.id == session_id).cloned();
        if self.active_session_id() == Some(session_id) {
            // shutdown_active drops the ActiveSession (and its terminal PTY).
            self.shutdown_active(cx);
        }
        // Deleting a thread that is working in the background kills it for real.
        self.drop_background(session_id, cx);
        self.terminal_workspaces
            .remove(&ConversationDestination::Thread(session_id.to_string()));
        if self.terminal_preferences.remove(session_id).is_some() {
            self.write_terminal_preferences(cx);
        }
        self.close_orchestrator_children(session_id, cx);
        let worktree_remove = meta.as_ref().and_then(|meta| {
            (remove_worktree && meta.worktree.is_some()).then(|| {
                let worktree = meta.worktree.as_ref().unwrap();
                (worktree.root_project_path.clone(), meta.cwd.clone())
            })
        });
        self.settings.last_visited.remove(session_id);
        self.enqueue_store_write(StoreWrite::RemoveSession(session_id.to_string()), cx);
        // Persist the pruned last-visited map (ignore save errors — cosmetic).
        self.persist_settings(cx);
        self.sessions.retain(|meta| meta.id != session_id);
        if let Some((root, cwd)) = worktree_remove {
            let deleted_id = session_id.to_string();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let result = host_cx
                    .unblock(move || remove_git_worktree(&root, &cwd))
                    .await;
                host_cx.enqueue(move |state, cx| {
                    if let Err(err) = result
                        && !state.sessions.iter().any(|meta| meta.id == deleted_id)
                        && state.active_session_id() != Some(&deleted_id)
                        && !state.residents.parked.contains_key(&deleted_id)
                    {
                        state.report_error(
                            RuntimeError::WorktreeRemove {
                                error: err.to_string(),
                            },
                            cx,
                        );
                    }
                });
            });
        }
    }

    /// Permanently remove a project and all of its threads from tcode. Project
    /// files and worktrees on disk are left in place.
    pub fn delete_project(&mut self, project_id: &str, cx: &mut HostCx) {
        let session_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .map(|meta| meta.id.clone())
            .collect();
        if self
            .residents
            .active
            .as_ref()
            .is_some_and(|active| active.meta.project_id.as_deref() == Some(project_id))
        {
            self.shutdown_active(cx);
        }
        let draft_destination = ConversationDestination::ProjectDraft(project_id.to_string());
        self.terminal_workspaces.remove(&draft_destination);
        if self
            .terminal_preferences
            .remove(&draft_destination.preference_key())
            .is_some()
        {
            self.write_terminal_preferences(cx);
        }
        for session_id in session_ids {
            self.delete_session(&session_id, false, cx);
        }
        self.enqueue_store_write(StoreWrite::RemoveProject(project_id.to_string()), cx);
        self.settings
            .collapsed_projects
            .retain(|id| id != project_id);
        self.persist_settings(cx);
        self.projects.retain(|project| project.id != project_id);
    }

    /// Whether `session_id` owns live or queued work.
    pub(crate) fn turn_running_for(&self, session_id: &str) -> bool {
        self.resident(session_id)
            .is_some_and(ActiveSession::has_work)
    }

    /// Number of active or parked sessions that still own live work: a turn in
    /// flight, an unacknowledged delivery, queued messages, or provider
    /// background tasks. Quitting stops all of it, so the quit guard must gate
    /// on this rather than on turns alone.
    #[cfg(test)]
    pub(super) fn working_sessions_count(&self) -> usize {
        usize::from(
            self.residents
                .active
                .as_ref()
                .is_some_and(ActiveSession::has_work),
        ) + self
            .residents
            .parked
            .values()
            .filter(|session| session.has_work())
            .count()
    }

    /// Record that a thread has been visited now (clears its unread dot).
    pub(super) fn mark_visited(&mut self, session_id: &str, cx: &mut HostCx) {
        self.settings
            .last_visited
            .insert(session_id.to_string(), now_secs());
        self.persist_settings(cx);
    }

    /// Mark a thread unread (context menu): set its last-visited just below its
    /// update time so the dot reappears.
    pub fn mark_session_unread(&mut self, session_id: &str, cx: &mut HostCx) {
        let updated = self
            .sessions
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.updated_at)
            .unwrap_or(0);
        self.settings
            .last_visited
            .insert(session_id.to_string(), updated.saturating_sub(1));
        self.persist_settings(cx);
    }

    /// Whether a thread shows an unread dot: it has been visited before, its
    /// update time is newer than that visit, and it is not the active thread.
    pub(crate) fn session_unread(&self, session_id: &str) -> bool {
        if self.active_session_id() == Some(session_id) {
            return false;
        }
        let Some(meta) = self.sessions.iter().find(|m| m.id == session_id) else {
            return false;
        };
        self.settings
            .last_visited
            .get(session_id)
            .is_some_and(|&visited| meta.updated_at > visited)
    }

    // -- worktree mode (Group C) --------------------------------------------

    /// Choose the draft's workspace mode (checkout-row picker). No-op unless the
    /// active thread is an unstarted draft.
    pub fn set_draft_workspace(&mut self, mode: WorkspaceMode, _cx: &mut HostCx) {
        if let Some(active) = self.residents.active.as_mut().filter(|a| a.draft) {
            active.draft_workspace = mode;
        }
    }

    /// Kick off background worktree creation for a draft's first send, then send
    /// the queued text once it is ready. Sets the "Preparing worktree…" state.
    pub(super) fn begin_worktree_prep(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        base: String,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.residents.active.as_mut() else {
            return;
        };
        active.preparing_worktree = true;
        let session_id = active.meta.id.clone();
        let root = active.meta.cwd.clone();

        let path = worktree_path_for(&session_id);
        let branch = format!("tcode/{session_id}");
        let base_for_task = base.clone();
        let root_for_task = root.clone();
        let path_for_task = path.clone();
        let branch_for_task = branch.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || {
                    create_git_worktree(
                        &root_for_task,
                        &path_for_task,
                        &branch_for_task,
                        &base_for_task,
                    )
                    .map(|worktree_path| {
                        let git_branch = read_git_branch(&worktree_path);
                        (worktree_path, git_branch)
                    })
                })
                .await;
            host_cx.enqueue(move |state, cx| {
                let Some(active) = state
                    .residents
                    .active
                    .as_mut()
                    .filter(|a| a.meta.id == session_id && a.draft)
                else {
                    return;
                };
                active.preparing_worktree = false;
                match result {
                    Ok((worktree_path, git_branch)) => {
                        active.meta.cwd = worktree_path.clone();
                        active.meta.worktree = Some(WorktreeInfo {
                            root_project_path: root,
                            base,
                            branch,
                        });
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        active.git_branch = git_branch;
                        // Now that the worktree exists, run the deferred send.
                        state.send_turn_assembled(text, attachments, cx);
                    }
                    Err(err) => {
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        state.report_error(
                            RuntimeError::WorktreeAdd {
                                error: err.to_string(),
                            },
                            cx,
                        );
                    }
                }
            });
        });
    }

    // -- draft threads ------------------------------------------------------

    /// Build a draft `ActiveSession` for `cwd` under `project_id`: set up but
    /// not persisted or started (see `commit_draft`). Pure (no store/cx) so the
    /// draft flow is unit-testable.
    pub(super) fn build_draft_session(
        project_id: String,
        cwd: PathBuf,
        provider: ProviderKind,
        model: Option<String>,
        acp_agent_id: Option<String>,
        provider_commands: Vec<ProviderCommand>,
    ) -> ActiveSession {
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.project_id = Some(project_id);
        meta.acp_agent_id = acp_agent_id;
        ActiveSession::new(meta, true, provider_commands)
    }

    /// The provider + model a new draft should start with: the most recently
    /// updated, non-archived session in this project. Only reasoning effort is
    /// inherited from its model options. Projects without active history fall
    /// back to the most recently updated non-archived global session (or the
    /// Claude default), without inheriting model options.
    pub(super) fn draft_defaults(
        &self,
        project_id: &str,
    ) -> (
        ProviderKind,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<OptionSelection>,
    ) {
        if let Some(meta) = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none() && meta.project_id.as_deref() == Some(project_id)
            })
            .max_by_key(|meta| meta.updated_at)
        {
            let reasoning_effort = meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .cloned();
            return (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                // Inherit the profile too, so "new thread" keeps talking to the
                // same third-party endpoint instead of falling back to the
                // built-in provider (which would reject the profile's model).
                meta.profile_id.clone(),
                reasoning_effort,
            );
        }

        match self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .max_by_key(|meta| meta.updated_at)
        {
            Some(meta) => (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                meta.profile_id.clone(),
                None,
            ),
            None => (ProviderKind::ClaudeCode, None, None, None, None),
        }
    }

    /// Switch the main area into a draft for `project_id` (rooted at `cwd`): an
    /// empty timeline with a focused, functional composer. The session is
    /// created lazily on the first send (see `send_turn`/`commit_draft`).
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut HostCx) {
        self.park_active(cx);
        let (provider, model, acp_agent_id, profile_id, reasoning_effort) =
            self.draft_defaults(&project_id);
        let provider_commands = self.cached_provider_commands(provider, acp_agent_id.as_deref());
        let mut draft = Self::build_draft_session(
            project_id,
            cwd,
            provider,
            model,
            acp_agent_id,
            provider_commands,
        );
        draft.meta.profile_id = profile_id;
        draft.meta.option_selections = reasoning_effort.into_iter().collect();
        let terminal_preferences = self.terminal_preferences_for(&draft);
        let restored_terminal = self.restore_terminal_workspace(&mut draft);
        self.residents.active = Some(draft);
        if let Some(active) = self.residents.active.as_ref() {
            self.refresh_session_git_branch(active.meta.id.clone(), active.meta.cwd.clone(), cx);
        }
        if !restored_terminal {
            self.reopen_persisted_terminals(terminal_preferences, cx);
        }
        self.refresh_git_status(cx);
    }

    /// Whether the active thread is an unsent draft.
    pub(crate) fn active_is_draft(&self) -> bool {
        self.residents.active.as_ref().is_some_and(|a| a.draft)
    }

    /// Persist the active draft as a real session.
    /// The session id is preserved, so its already-recorded events line up.
    pub(super) fn commit_draft(&mut self, cx: &mut HostCx) -> std::io::Result<()> {
        let preference_migration = self.residents.active.as_ref().and_then(|active| {
            active.draft.then(|| {
                (
                    conversation_destination(active).preference_key(),
                    active.meta.id.clone(),
                )
            })
        });
        if let Some(active) = self.residents.active.as_mut()
            && active.draft
        {
            active.draft = false;
            let meta = active.meta.clone();
            self.emit_domain(
                Topic::SessionEvents {
                    session_id: meta.id.clone(),
                },
                ServerEvent::SessionSnapshot(Vec::new()),
                cx,
            );
            self.enqueue_store_write(
                StoreWrite::UpsertMeta {
                    meta: Box::new(meta.clone()),
                    initial: true,
                },
                cx,
            );
            self.upsert_session_in_memory(meta);
        }
        if let Some((draft_key, session_key)) = preference_migration
            && let Some(preferences) = self.terminal_preferences.remove(&draft_key)
        {
            self.terminal_preferences.insert(session_key, preferences);
            self.write_terminal_preferences(cx);
        }
        Ok(())
    }

    pub(super) fn schedule_timeline_load(
        &mut self,
        session_id: String,
        target: TimelineLoadTarget,
        cx: &mut HostCx,
    ) {
        let generation = {
            let generation = self
                .timeline_load_generations
                .entry(session_id.clone())
                .or_default();
            *generation += 1;
            *generation
        };
        self.spawn_timeline_load_attempt(
            session_id,
            target,
            generation,
            self.store_append_generation,
            1,
            cx,
        );
    }

    pub(super) fn spawn_timeline_load_attempt(
        &mut self,
        session_id: String,
        target: TimelineLoadTarget,
        generation: u64,
        watermark: u64,
        attempt: u8,
        cx: &mut HostCx,
    ) {
        let intended = match target {
            TimelineLoadTarget::Active { .. } => self
                .residents
                .active
                .as_ref()
                .filter(|session| session.meta.id == session_id),
            TimelineLoadTarget::Background { .. } => self.residents.parked.get(&session_id),
        };
        let Some(cwd) = intended.map(|session| session.meta.cwd.clone()) else {
            return;
        };
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let read_id = session_id.clone();
            let (timeline, records, git_branch) = {
                let stored = store.read_events(&read_id);
                let mut timeline = Timeline::fold_events(stored.iter().cloned());
                let records = stored
                    .into_iter()
                    .map(|stored| SessionEventRecord {
                        ts: stored.ts,
                        event: stored.event,
                    })
                    .collect();
                let (mark_idle, load_branch) = match target {
                    TimelineLoadTarget::Active {
                        mark_idle,
                        read_git_branch,
                    } => (mark_idle, read_git_branch),
                    TimelineLoadTarget::Background { mark_idle } => (mark_idle, false),
                };
                if mark_idle {
                    timeline.mark_idle();
                }
                let git_branch = load_branch.then(|| read_git_branch(&cwd));
                (timeline, records, git_branch)
            };
            host_cx.enqueue(move |state, cx| {
                let generation_matches = state
                    .timeline_load_generations
                    .get(&session_id)
                    .copied()
                    == Some(generation);
                let target_matches = match target {
                    TimelineLoadTarget::Active { .. } => {
                        state.active_session_id() == Some(session_id.as_str())
                    }
                    TimelineLoadTarget::Background { .. } => {
                        state.residents.parked.contains_key(&session_id)
                    }
                };
                if !generation_matches || !target_matches {
                    return;
                }
                if state.store_append_generation != watermark && attempt < 4 {
                    state.spawn_timeline_load_attempt(
                        session_id,
                        target,
                        generation,
                        state.store_append_generation,
                        attempt + 1,
                        cx,
                    );
                    return;
                }
                if state.store_append_generation != watermark {
                    log::warn!(
                        "timeline load for {session_id} remained racy after {attempt} attempts; applying the last fold"
                    );
                }
                match target {
                    TimelineLoadTarget::Active { .. } => {
                        if let Some(active) = state.residents.active.as_mut() {
                            active.timeline = timeline;
                            if let Some(git_branch) = git_branch {
                                active.git_branch = git_branch;
                            }
                        }
                        state.emit_domain(
                            Topic::SessionEvents {
                                session_id: session_id.clone(),
                            },
                            ServerEvent::SessionSnapshot(records),
                            cx,
                        );
                    }
                    TimelineLoadTarget::Background { .. } => {
                        if let Some(background) = state.residents.parked.get_mut(&session_id) {
                            background.timeline = timeline;
                        }
                    }
                }
            });
        });
    }

    /// Make a stored session active: replay its JSONL log only. The provider
    /// process starts lazily on the next send (with the stored resume cursor).
    pub fn select_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.active_session_id() == Some(session_id) {
            return;
        }
        let Some(meta) = self.sessions.iter().find(|m| m.id == session_id).cloned() else {
            return;
        };
        self.park_active(cx);
        self.mark_visited(session_id, cx);

        // A parked session is re-adopted, not replayed cold: its process, pump
        // and queue come back as they were, and the timeline is rebuilt from the
        // JSONL — which stayed current while parked, because `record_event`
        // routes by session id.
        if let Some(mut parked) = self.residents.adopt(session_id) {
            log::info!(
                "re-adopting parked session {} (turn in flight: {}, queued: {})",
                session_id,
                parked.turn_in_flight,
                parked.queue.len()
            );
            parked.idle_since = None;
            let terminal_preferences = self.terminal_preferences_for(&parked);
            let restored_terminal = self.restore_terminal_workspace(&mut parked);
            let needs_restart = matches!(parked.runtime, Runtime::Idle) && !parked.queue.is_empty();
            self.residents.active = Some(parked);
            self.schedule_timeline_load(
                session_id.to_string(),
                TimelineLoadTarget::Active {
                    mark_idle: false,
                    read_git_branch: true,
                },
                cx,
            );
            if !restored_terminal {
                self.reopen_persisted_terminals(terminal_preferences, cx);
            }
            // Anything still queued that can go now, goes now.
            if self.dispatch_next_queued(cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if needs_restart {
                // Parked with a dead provider (its start failed while parked):
                // the queue survived, so try again now that someone is looking.
                self.ensure_started(cx);
            }
            self.refresh_git_status(cx);
            self.preview_draft_or_persist_active(cx);
            self.reschedule_scheduled_wake(cx);
            return;
        }

        log::info!(
            "opening session {} (resume cursor: {})",
            meta.id,
            meta.resume_cursor.is_some()
        );
        let session_id = meta.id.clone();
        let provider_commands =
            self.cached_provider_commands(meta.provider, meta.acp_agent_id.as_deref());
        let mut active = ActiveSession::new(meta, false, provider_commands);
        let terminal_preferences = self.terminal_preferences_for(&active);
        let restored_terminal = self.restore_terminal_workspace(&mut active);
        self.residents.active = Some(active);
        self.schedule_timeline_load(
            session_id,
            TimelineLoadTarget::Active {
                mark_idle: true,
                read_git_branch: true,
            },
            cx,
        );
        if !restored_terminal {
            self.reopen_persisted_terminals(terminal_preferences, cx);
        }
        self.refresh_git_status(cx);
    }

    /// Open the most recently updated stored session (replay only). Used by the
    /// hidden `--open-latest` launch flag. No-op when there are no sessions.
    pub fn open_latest_session(&mut self, cx: &mut HostCx) {
        // `sessions` is kept sorted newest-first by `load_index`.
        if let Some(id) = self.sessions.first().map(|m| m.id.clone()) {
            self.select_session(&id, cx);
        }
    }
}

pub(super) fn descendant_session_ids(sessions: &[SessionMeta], root_id: &str) -> Vec<String> {
    fn append(
        sessions: &[SessionMeta],
        session_id: &str,
        visited: &mut HashSet<String>,
        output: &mut Vec<String>,
    ) {
        if !visited.insert(session_id.to_string()) {
            return;
        }
        output.push(session_id.to_string());
        let children: Vec<_> = sessions
            .iter()
            .filter(|meta| meta.parent_session_id.as_deref() == Some(session_id))
            .map(|meta| meta.id.clone())
            .collect();
        for child in children {
            append(sessions, &child, visited, output);
        }
    }

    if !sessions.iter().any(|meta| meta.id == root_id) {
        return Vec::new();
    }
    let mut output = Vec::new();
    append(sessions, root_id, &mut HashSet::new(), &mut output);
    output
}
