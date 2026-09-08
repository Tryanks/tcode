use super::*;

/// Sessions viewed by clients and sessions retained for in-flight work or re-adoption.
#[derive(Default)]
pub struct ResidentSessions {
    pub live: HashMap<String, ActiveSession>,
    pub(super) parked: HashMap<String, ActiveSession>,
}
impl ResidentSessions {
    pub(crate) fn resident(&self, id: &str) -> Option<&ActiveSession> {
        self.live.get(id).or_else(|| self.parked.get(id))
    }
    pub(super) fn resident_mut(&mut self, id: &str) -> Option<&mut ActiveSession> {
        self.live.get_mut(id).or_else(|| self.parked.get_mut(id))
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
        self.live
            .keys()
            .chain(self.parked.keys())
            .map(String::as_str)
    }
}

impl AppState {
    pub(crate) fn subscribe(
        &mut self,
        subscription: &tcode_protocol::Subscription,
        cx: &mut HostCx,
    ) {
        if self.subscriptions.insert(subscription.topic.clone()) {
            match &subscription.topic {
                Topic::SessionStatus { session_id }
                | Topic::SessionEvents { session_id }
                | Topic::GitStatus { session_id } => self.select_session(session_id, cx),
                // No projection ran while nobody was attached, so the first
                // subscriber rebuilds the frame. A later one reads that same
                // retained frame and continues from the shared delta sequence.
                Topic::Terminal { terminal_id } => {
                    self.refresh_terminal_projection(*terminal_id);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn unsubscribe(
        &mut self,
        subscription: &tcode_protocol::Subscription,
        cx: &mut HostCx,
    ) {
        self.subscriptions.remove(&subscription.topic);
        let session_id = match &subscription.topic {
            Topic::SessionStatus { session_id }
            | Topic::SessionEvents { session_id }
            | Topic::GitStatus { session_id } => session_id,
            _ => return,
        };
        if !self.subscriptions.iter().any(|topic| match topic {
            Topic::SessionStatus { session_id: id }
            | Topic::SessionEvents { session_id: id }
            | Topic::GitStatus { session_id: id } => id == session_id,
            _ => false,
        }) {
            self.park_active(session_id, cx);
        }
    }

    /// Assemble the provider-bound message at the runtime boundary. Unreadable
    /// attachment files are skipped, matching the composer's previous behavior.
    pub(super) fn assemble_user_message(
        &self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
    ) -> (String, Vec<Attachment>) {
        let terminal_contexts = self
            .resident(target_id)
            .map(|active| active.terminal_workspace.contexts.as_slice())
            .unwrap_or_default();
        let text = append_terminal_contexts_to_prompt(&text, terminal_contexts);
        let text = append_review_comments_to_prompt(&text, self.review_comments(target_id));
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

    pub(super) fn clear_consumed_draft_context(&mut self, target_id: &str, cx: &mut HostCx) {
        self.clear_terminal_contexts(target_id);
        self.clear_review_comments(target_id, cx);
    }

    /// Cycle the sidebar PROJECTS ordering and persist it.
    pub fn cycle_project_sort(&mut self, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        settings.project_sort = settings.project_sort.next();
        self.update_settings(settings, cx);
    }

    /// Create a project rooted at `root`, or return the existing id when one
    /// already covers it.
    ///
    /// The root is validated here, against this host's filesystem and path
    /// rules: a client cannot decide whether `C:\src` or `/srv/src` is absolute,
    /// and only the host can see whether the directory exists.
    pub fn create_project(
        &mut self,
        root: PathBuf,
        cx: &mut HostCx,
    ) -> Result<String, ProtocolError> {
        if let Some(existing) = self.projects.iter().find(|p| p.root == root) {
            return Ok(existing.id.clone());
        }
        if !root.is_absolute() {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!("{} is not an absolute path on this host", root.display()),
            });
        }
        if !root.is_dir() {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!("{} is not a directory on this host", root.display()),
            });
        }
        let project = Project::from_root(root);
        let id = project.id.clone();
        self.enqueue_store_write(StoreWrite::UpsertProject(project.clone()), cx);
        self.projects.push(project);
        Ok(id)
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

    /// Import selected external threads in the background, publishing progress
    /// as replicated host state on [`Topic::ExternalImport`]. Returns `false`
    /// for an unknown project; a second concurrent run is rejected outright.
    ///
    /// Completion is runtime-owned: the importer's last update finalizes the
    /// index here regardless of whether any client is still subscribed.
    pub fn start_external_import(
        &mut self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut HostCx,
    ) -> Result<bool, ProtocolError> {
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
        else {
            return Ok(false);
        };
        if let Some(status) = self.external_imports.get(project_id)
            && matches!(status.state, ExternalImportState::Progress { .. })
        {
            return Err(ProtocolError {
                code: "import_in_progress".into(),
                message: format!("an import is already running for project {project_id}"),
            });
        }
        let run_id = self.next_import_run_id;
        self.next_import_run_id += 1;
        let total = threads.len();
        let tool = threads
            .first()
            .map(|thread| thread.source.display_name().to_string())
            .unwrap_or_default();
        self.replace_external_import_status(
            project_id,
            Some(ExternalImportStatus {
                run_id,
                state: ExternalImportState::Progress {
                    done: 0,
                    total,
                    tool,
                },
            }),
            cx,
        );

        let store = self.store.clone();
        let metas = self.sessions.clone();
        let id = project_id.to_string();
        let updates = cx.clone();
        cx.unblock(move || {
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
                let (id, done) = (id.clone(), index + 1);
                updates.enqueue(move |state, cx| {
                    state.advance_external_import(
                        &id,
                        run_id,
                        ExternalImportState::Progress { done, total, tool },
                        cx,
                    );
                });
            }
            updates.enqueue(move |state, cx| {
                state.complete_external_import(&id, run_id, imported, skipped, cx);
            });
        })
        .detach();
        Ok(true)
    }

    /// Publish a status the importer produced, ignoring updates from a run that
    /// a newer one has already superseded.
    fn advance_external_import(
        &mut self,
        project_id: &str,
        run_id: u64,
        state: ExternalImportState,
        cx: &mut HostCx,
    ) {
        if self
            .external_imports
            .get(project_id)
            .map(|status| status.run_id)
            != Some(run_id)
        {
            return;
        }
        self.replace_external_import_status(
            project_id,
            Some(ExternalImportStatus { run_id, state }),
            cx,
        );
    }

    /// Finalize a finished run. The index is reloaded in this mailbox turn, so
    /// its replacement reaches clients before the `Finished` status published
    /// by the follow-up turn — a subscriber never sees `Finished` with a stale
    /// session list.
    fn complete_external_import(
        &mut self,
        project_id: &str,
        run_id: u64,
        imported: usize,
        skipped: usize,
        cx: &mut HostCx,
    ) {
        if self
            .external_imports
            .get(project_id)
            .map(|status| status.run_id)
            != Some(run_id)
        {
            return;
        }
        self.finish_external_import(project_id, cx);
        let project_id = project_id.to_string();
        cx.enqueue(move |state, cx| {
            state.advance_external_import(
                &project_id,
                run_id,
                ExternalImportState::Finished { imported, skipped },
                cx,
            );
        });
    }

    pub(crate) fn replace_external_import_status(
        &mut self,
        project_id: &str,
        status: Option<ExternalImportStatus>,
        cx: &mut HostCx,
    ) {
        match &status {
            Some(status) => {
                self.external_imports
                    .insert(project_id.to_string(), status.clone());
            }
            None => {
                self.external_imports.remove(project_id);
            }
        }
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
            event: ServerEvent::ExternalImportStatusReplaced {
                project_id: project_id.to_string(),
                status,
            },
        }));
    }

    /// Search this host's own stored sessions in index order. Both the file
    /// reads and the cache lock stay on the blocking executor.
    pub fn search_session_content(
        &self,
        query: String,
        limit: u32,
        executor: &HostCx,
    ) -> HostTask<Vec<SessionSearchHit>> {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX).min(50);
        let sessions = self.sessions.clone();
        let search = self.session_search.clone();
        executor.unblock(move || {
            search
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .search(&sessions, &query, limit)
        })
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
    fn finish_external_import(&mut self, project_id: &str, cx: &mut HostCx) {
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

    /// Flush pending appends, then render a thread into transferable bytes on
    /// the host's blocking-I/O executor. Nothing is written: the requesting
    /// client owns the destination, which may not be on this machine at all.
    pub fn render_thread_export(
        &mut self,
        session_id: &str,
        format: ThreadExportFormat,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        let Some(meta) = self.find_meta(session_id) else {
            let session_id = session_id.to_owned();
            return cx.spawn_background(async move {
                Err(ProtocolError {
                    code: "unknown_session".into(),
                    message: format!("unknown session {session_id}"),
                })
            });
        };
        let barrier = self.store_write_barrier(cx);
        let store = self.store.clone();
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            barrier.recv().await.map_err(|error| ProtocolError {
                code: "store_barrier_closed".into(),
                message: format!("session-store flush failed: {error}"),
            })?;
            let suggested_name = export::export_file_name(&meta.title, format);
            let bytes = host_cx
                .unblock(move || export::render_thread(&store, &meta, format))
                .await
                .map_err(|error| ProtocolError {
                    code: "export_failed".into(),
                    message: error.to_string(),
                })?;
            if bytes.len() > tcode_protocol::MAX_THREAD_EXPORT_BYTES {
                return Err(ProtocolError {
                    code: "export_too_large".into(),
                    message: format!(
                        "the rendered export is {} bytes, over the {} byte transfer limit",
                        bytes.len(),
                        tcode_protocol::MAX_THREAD_EXPORT_BYTES
                    ),
                });
            }
            Ok(QueryResponse::ThreadExport {
                bytes,
                suggested_name,
                mime: export::export_mime(format).to_owned(),
            })
        })
    }

    /// Merge a clean dedicated-worktree branch into its clean original checkout
    /// without blocking the host owner thread.
    pub fn merge_worktree(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(meta) = self.find_meta(session_id) else {
            return;
        };
        let Some(worktree) = meta.worktree else {
            return;
        };
        let destination = worktree.root_project_path;
        let worktree_path = meta.cwd;
        let branch = worktree.branch;
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || merge_back(&destination, &worktree_path, &branch))
                .await;
            host_cx.enqueue(move |_state, cx| {
                let notice = match result {
                    Ok(MergeBackOutcome::FastForward) => RuntimeNotice::WorktreeMergedFastForward,
                    Ok(MergeBackOutcome::MergeCommit) => RuntimeNotice::WorktreeMergedCommit,
                    Err(error) => {
                        let (reason, detail) = match error {
                            MergeBackError::WorktreeMissing => {
                                (MergeWorktreeFailure::Missing, None)
                            }
                            MergeBackError::DirtyWorktree => {
                                (MergeWorktreeFailure::DirtyWorktree, None)
                            }
                            MergeBackError::DestinationDetached => {
                                (MergeWorktreeFailure::DestinationDetached, None)
                            }
                            MergeBackError::DirtyDestination => {
                                (MergeWorktreeFailure::DirtyDestination, None)
                            }
                            MergeBackError::DivergedConflict => {
                                (MergeWorktreeFailure::DivergedConflict, None)
                            }
                            MergeBackError::Git(detail) => {
                                (MergeWorktreeFailure::Git, Some(detail))
                            }
                        };
                        RuntimeNotice::WorktreeMergeFailed { reason, detail }
                    }
                };
                emit_runtime(cx, RuntimeEvent::Notice(notice));
            });
        });
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

    pub(crate) fn resident(&self, id: &str) -> Option<&ActiveSession> {
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
        let changed: HashSet<_> = self
            .providers
            .provider_usage
            .keys()
            .chain(self.providers.usage_checking.iter())
            .filter(|id| self.settings.resolved_profile(id) != settings.resolved_profile(id))
            .cloned()
            .collect();
        for id in changed {
            self.providers.invalidate_usage(&id);
        }
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
        settings.apply(patch);
        self.update_settings(settings, cx);
    }

    /// Persist a restart-continuity marker naming the Settings page to reopen and
    /// the session that is active now. Written before a Screen Recording request
    /// or an explicit relaunch, so an externally-initiated quit reopens cleanly.
    pub fn write_relaunch_marker(&self, target_id: &str, reopen_settings: &str) {
        let marker = tcode_services::relaunch::RelaunchMarker {
            reopen_settings: reopen_settings.to_string(),
            active_session: self
                .resident(target_id)
                .map(|session| session.meta.id.as_str())
                .map(str::to_string),
        };
        if let Err(err) = tcode_services::relaunch::write(self.store.root(), &marker) {
            log::warn!("failed to write relaunch marker: {err}");
        }
    }

    pub fn clear_relaunch_marker(&self) {
        if let Err(err) = tcode_services::relaunch::clear(self.store.root()) {
            log::warn!("failed to clear relaunch marker: {err}");
        }
    }

    /// Apply a marker taken at launch: reopen the recorded session and open
    /// Settings on the recorded page. The page reruns a permission recheck as it
    /// mounts, so the user immediately sees the post-restart status. No-op when
    /// there is no marker (the normal launch path).
    pub fn apply_pending_relaunch(&mut self) -> (Option<String>, Option<String>) {
        let Some(marker) = self.pending_relaunch.take() else {
            return (None, None);
        };
        let session_id = marker
            .active_session
            .filter(|id| self.sessions.iter().any(|meta| meta.id == *id));
        (Some(marker.reopen_settings), session_id)
    }

    pub(super) fn settle_family_busy(&self, session_id: &str) -> bool {
        descendant_session_ids(&self.sessions, session_id)
            .iter()
            .any(|id| {
                self.resident(id).is_some_and(|session| {
                    session.has_work()
                        || session.timeline.turn_running
                        || !session.timeline.pending_approvals.is_empty()
                        || session.timeline.pending_user_input.is_some()
                })
            })
    }

    /// Settle a whole descendant group without shutting down its resources.
    pub fn settle_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.settle_family_busy(session_id) {
            return;
        }
        let timestamp = now_secs();
        for id in descendant_session_ids(&self.sessions, session_id) {
            if let Some(mut meta) = self.find_meta(&id)
                && meta.settled_at.is_none()
            {
                meta.settled_at = Some(timestamp);
                self.persist_settled_meta(meta, cx);
            }
        }
    }

    /// Restore the matching settle cascade, then expose all of its ancestors.
    pub fn make_session_active(&mut self, session_id: &str, cx: &mut HostCx) {
        if let Some(timestamp) = self.find_meta(session_id).and_then(|meta| meta.settled_at) {
            for id in descendant_session_ids(&self.sessions, session_id) {
                if let Some(mut meta) = self.find_meta(&id)
                    && meta.settled_at == Some(timestamp)
                {
                    meta.settled_at = None;
                    self.persist_settled_meta(meta, cx);
                }
            }
        }
        self.reactivate_session(session_id, cx);
    }

    /// Accepted input exposes this thread and its ancestors, leaving siblings settled.
    pub(super) fn reactivate_session(&mut self, session_id: &str, cx: &mut HostCx) {
        let mut next = Some(session_id.to_string());
        let mut visited = HashSet::new();
        while let Some(id) = next.take() {
            if !visited.insert(id.clone()) {
                break;
            }
            let Some(mut meta) = self.find_meta(&id) else {
                break;
            };
            next = meta.parent_session_id.clone();
            if meta.settled_at.take().is_some() {
                self.persist_settled_meta(meta, cx);
            }
        }
    }

    fn persist_settled_meta(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        if let Some(session) = self.resident_mut(&meta.id) {
            session.meta.settled_at = meta.settled_at;
        }
        self.persist_meta(&meta, cx);
    }

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
            active: self.residents.live.keys().cloned().collect(),
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

        for id in ids.iter().copied() {
            self.shutdown_active(id, cx);
            // An archived conversation must not leave an off-screen PTY running.
            self.terminal_workspaces
                .remove(&ConversationDestination::Thread(id.to_string()));
            self.drop_background(id, cx);
            self.revoke_preview_registration(id);
            self.revoke_orchestrate_child_registration(id);
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
    pub fn fork_thread(&mut self, id: &str, cx: &mut HostCx) -> Option<String> {
        let source = self
            .resident(id)
            .map(|session| (session.meta.clone(), session.turn_in_flight))
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|meta| meta.id == id)
                    .cloned()
                    .map(|meta| (meta, false))
            });
        let (source, turn_in_flight) = source?;
        if !source.provider.caps().supports_fork {
            self.report_error(
                RuntimeError::External("This provider does not support conversation forks.".into()),
                cx,
            );
            return None;
        }
        if source.resume_cursor.is_none() {
            self.report_error(
                RuntimeError::External("This conversation is empty and cannot be forked.".into()),
                cx,
            );
            return None;
        }
        if turn_in_flight {
            self.report_error(
                RuntimeError::External(
                    "Wait for the running turn to finish before forking this conversation.".into(),
                ),
                cx,
            );
            return None;
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

        let fork_id = fork.id.clone();
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
                    if let Some(snapshot) =
                        state.subscription_snapshot(&tcode_protocol::Subscription {
                            topic: Topic::SessionEvents {
                                session_id: fork.id.clone(),
                            },
                            after: None,
                        })
                    {
                        cx.emit(HostEvent::Domain(snapshot));
                    }
                }
                Err(error) => {
                    state.report_error(RuntimeError::PersistEvent { error }, cx);
                }
            });
        });
        Some(fork_id)
    }

    /// Permanently delete a thread: stop the provider, close its terminal,
    /// delete meta + JSONL, and (when `remove_worktree`) remove the git worktree
    /// it was the last user of.
    pub fn delete_session(&mut self, session_id: &str, remove_worktree: bool, cx: &mut HostCx) {
        self.clear_approvals(session_id);
        let meta = self.sessions.iter().find(|m| m.id == session_id).cloned();
        if self.residents.live.contains_key(session_id) {
            // shutdown_active drops the ActiveSession (and its terminal PTY).
            self.shutdown_active(session_id, cx);
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
                        && !state.residents.live.contains_key(&deleted_id)
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
        let drafts: Vec<_> = self
            .residents
            .live
            .values()
            .filter(|s| s.draft && s.meta.project_id.as_deref() == Some(project_id))
            .map(|s| s.meta.id.clone())
            .collect();
        for id in drafts {
            self.shutdown_active(&id, cx);
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
        self.replace_external_import_status(project_id, None, cx);
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
        self.residents
            .live
            .values()
            .chain(self.residents.parked.values())
            .filter(|s| s.has_work())
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
        if self.residents.live.contains_key(session_id) {
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

    /// Remove app-owned worktrees whose session is absent from the loaded store.
    pub(crate) fn recover_orphaned_worktrees(&self, cx: &mut HostCx) {
        let known_ids = self
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let summary = host_cx.unblock(move || cleanup_orphans(&known_ids)).await;
            if !summary.removed.is_empty() || !summary.skipped.is_empty() {
                log::info!(
                    "worktree orphan recovery removed {}, left {}",
                    summary.removed.len(),
                    summary.skipped.len()
                );
            }
        });
    }

    /// Choose the draft's workspace mode (checkout-row picker). No-op unless the
    /// active thread is an unstarted draft.
    pub fn set_draft_workspace(&mut self, target_id: &str, mode: WorkspaceMode, _cx: &mut HostCx) {
        if let Some(active) = self.resident_mut(target_id).filter(|a| a.draft) {
            active.draft_workspace = mode;
        }
    }

    /// Kick off background worktree creation for a draft's first send, then send
    /// the queued text once it is ready. Sets the "Preparing worktree…" state.
    pub(super) fn begin_worktree_prep(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        _base: String,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        active.preparing_worktree = true;
        let session_id = active.meta.id.clone();
        let session_id_for_task = session_id.clone();
        let root = active.meta.cwd.clone();

        let root_for_task = root.clone();
        let target_id = target_id.to_string();
        let delivery_key = cx.delivery_key.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || provision(&root_for_task, &session_id_for_task))
                .await;
            host_cx.enqueue(move |state, cx| {
                let Some(active) = state
                    .resident_mut(&target_id)
                    .filter(|a| a.meta.id == session_id && a.draft)
                else {
                    return;
                };
                active.preparing_worktree = false;
                match result {
                    Ok(created) => {
                        active.meta.cwd = created.path.clone();
                        active.meta.worktree = Some(WorktreeInfo {
                            root_project_path: root,
                            base: created.base,
                            branch: created.branch.clone(),
                        });
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        active.git_branch = Some(created.branch);
                        if created.seed_summary.manifest_found {
                            emit_runtime(
                                cx,
                                RuntimeEvent::Notice(RuntimeNotice::WorktreeSeeded {
                                    copied_files: created.seed_summary.copied_files,
                                    skipped: created.seed_summary.skipped,
                                    limit_reached: created.seed_summary.limit_reached,
                                }),
                            );
                        }
                        // Now that the worktree exists, run the deferred send.
                        cx.delivery_key = delivery_key;
                        state.send_turn_assembled(&target_id, text, attachments, cx);
                        cx.delivery_key = None;
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

    /// Build a draft for `cwd` under `project_id` without persisting it or
    /// starting a provider (see `commit_draft`).
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

    /// The reasoning effort last used with exactly this (provider, model),
    /// from the most recently updated session that ran it (archived included:
    /// memory outlives the thread). Model switches restore this instead of
    /// resetting to the model's default. Keyed per model, so the remembered
    /// value is always one the model accepts.
    pub(super) fn remembered_effort(
        &self,
        provider: ProviderKind,
        model: Option<&str>,
    ) -> Option<OptionSelection> {
        self.sessions
            .iter()
            .filter(|meta| meta.provider == provider && meta.model.as_deref() == model)
            .max_by_key(|meta| meta.updated_at)
            .and_then(|meta| {
                meta.option_selections
                    .iter()
                    .find(|selection| selection.id == "reasoningEffort")
                    .cloned()
            })
    }

    /// Switch the main area into a draft for `project_id` (rooted at `cwd`): an
    /// empty timeline with a focused, functional composer. The session is
    /// created lazily on the first send (see `send_turn`/`commit_draft`).
    ///
    /// A New thread surface keeps at most one unsent draft: reopening the same
    /// project at the same root returns the draft already standing, because the
    /// composer's attachments and the draft's terminal follow that session id.
    /// A second root (another client viewing the same project elsewhere) is a
    /// different surface and gets its own draft.
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut HostCx) -> String {
        let standing = self
            .residents
            .live
            .values()
            .chain(self.residents.parked.values())
            .find(|active| {
                active.draft
                    && active.meta.project_id.as_deref() == Some(project_id.as_str())
                    && active.meta.cwd == cwd
            })
            .map(|active| active.meta.id.clone());
        if let Some(session_id) = standing {
            if let Some(mut parked) = self.residents.adopt(&session_id) {
                parked.idle_since = None;
                self.restore_terminal_workspace(&mut parked);
                self.residents.live.insert(session_id.clone(), parked);
            }
            self.refresh_git_status(&session_id, cx);
            return session_id;
        }
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
        let session_id = draft.meta.id.clone();
        self.residents.live.insert(session_id.clone(), draft);
        if let Some(active) = self.resident(&session_id) {
            self.refresh_session_git_branch(active.meta.id.clone(), active.meta.cwd.clone(), cx);
        }
        if !restored_terminal {
            self.reopen_persisted_terminals(&session_id, terminal_preferences, cx);
        }
        self.refresh_git_status(&session_id, cx);
        session_id
    }

    /// Persist the active draft as a real session.
    /// The session id is preserved, so its already-recorded events line up.
    pub(super) fn commit_draft(&mut self, target_id: &str, cx: &mut HostCx) {
        let preference_migration = self.resident(target_id).and_then(|active| {
            active.draft.then(|| {
                (
                    conversation_destination(active).preference_key(),
                    active.meta.id.clone(),
                )
            })
        });
        if let Some(active) = self.resident_mut(target_id)
            && active.draft
        {
            active.draft = false;
            let meta = active.meta.clone();
            self.emit_domain(
                Topic::SessionEvents {
                    session_id: meta.id.clone(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    records: Vec::new(),
                },
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
            TimelineLoadTarget::Active { .. } => self.resident(&session_id),
            TimelineLoadTarget::Background => self.residents.parked.get(&session_id),
        };
        let Some(cwd) = intended.map(|session| session.meta.cwd.clone()) else {
            return;
        };
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let read_id = session_id.clone();
            let (timeline, git_branch) = {
                let stored = store.read_events(&read_id);
                let mut timeline = Timeline::fold_events(stored.iter().cloned());
                let (mark_idle, load_branch) = match target {
                    TimelineLoadTarget::Active { mark_idle } => (mark_idle, true),
                    TimelineLoadTarget::Background => (true, false),
                };
                if mark_idle {
                    timeline.mark_idle();
                }
                let git_branch = load_branch.then(|| read_git_branch(&cwd));
                (timeline, git_branch)
            };
            host_cx.enqueue(move |state, cx| {
                let generation_matches = state
                    .timeline_load_generations
                    .get(&session_id)
                    .copied()
                    == Some(generation);
                let target_matches = match target {
                    TimelineLoadTarget::Active { .. } => {
                        state.residents.live.contains_key(&session_id)
                    }
                    TimelineLoadTarget::Background => {
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
                if let Some(session) = state.resident_mut(&session_id) {
                    session.timeline = timeline;
                    if let Some(git_branch) = git_branch {
                        session.git_branch = git_branch;
                    }
                }
            });
        });
    }

    /// Adopt a subscribed session, including an uncommitted parked draft. Stored
    /// sessions replay their JSONL; providers start lazily on the next send.
    pub fn select_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.residents.live.contains_key(session_id) {
            return;
        }
        let Some(meta) = self.find_meta(session_id) else {
            return;
        };
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
            self.residents.live.insert(session_id.to_string(), parked);
            self.schedule_timeline_load(
                session_id.to_string(),
                TimelineLoadTarget::Active { mark_idle: false },
                cx,
            );
            if !restored_terminal {
                self.reopen_persisted_terminals(session_id, terminal_preferences, cx);
            }
            // Anything still queued that can go now, goes now.
            if self.dispatch_next_queued(session_id, cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if needs_restart {
                // Parked with a dead provider (its start failed while parked):
                // the queue survived, so try again now that someone is looking.
                self.ensure_started(session_id, cx);
            }
            self.refresh_git_status(session_id, cx);
            self.preview_draft_or_persist_active(session_id, cx);
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
        self.residents.live.insert(session_id.clone(), active);
        self.schedule_timeline_load(
            session_id.clone(),
            TimelineLoadTarget::Active { mark_idle: true },
            cx,
        );
        if !restored_terminal {
            self.reopen_persisted_terminals(&session_id, terminal_preferences, cx);
        }
        self.refresh_git_status(&session_id, cx);
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
