use super::*;

impl AppState {
    pub(super) fn next_operation_id(&mut self) -> RuntimeOperationId {
        let id = RuntimeOperationId(self.next_operation_id);
        self.next_operation_id += 1;
        id
    }

    pub(crate) fn refresh_git_status(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(cwd) = self.resident(target_id).map(|a| a.meta.cwd.clone()) else {
            self.git_status.remove(target_id);
            return;
        };
        let session_id = target_id.to_string();
        let generation = self
            .git_status_generation
            .entry(session_id.clone())
            .or_default();
        *generation += 1;
        let generation = *generation;
        if let Some(pending) = self.git_status_pending.get_mut(&cwd) {
            pending.insert(session_id, generation);
        } else {
            self.start_git_status_read(cwd, HashMap::from([(session_id, generation)]), cx);
        }
    }

    fn start_git_status_read(
        &mut self,
        cwd: PathBuf,
        requests: HashMap<String, u64>,
        cx: &mut HostCx,
    ) {
        self.git_status_pending.insert(cwd.clone(), HashMap::new());
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let read_cwd = cwd.clone();
            let status = host_cx.unblock(move || read_status(&read_cwd)).await;
            host_cx.enqueue(move |state, cx| {
                for (session_id, generation) in requests {
                    if state.git_status_generation.get(&session_id) == Some(&generation)
                        && state
                            .resident(&session_id)
                            .is_some_and(|session| session.meta.cwd == cwd)
                    {
                        state.git_status.insert(session_id, status.clone());
                    }
                }
                // A turn or Git action may have changed the checkout after
                // this read began. Later requests need a later snapshot.
                let mut pending = state.git_status_pending.remove(&cwd).unwrap_or_default();
                pending.retain(|session_id, generation| {
                    state.git_status_generation.get(session_id) == Some(generation)
                        && state
                            .resident(session_id)
                            .is_some_and(|session| session.meta.cwd == cwd)
                });
                if !pending.is_empty() {
                    state.start_git_status_read(cwd, pending, cx);
                }
            });
        });
    }

    pub(super) fn refresh_session_git_branch(
        &mut self,
        session_id: String,
        cwd: PathBuf,
        cx: &mut HostCx,
    ) {
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let branch = host_cx.unblock(move || read_git_branch(&cwd)).await;
            host_cx.enqueue(move |state, _cx| {
                if let Some(session) = state.resident_mut(&session_id) {
                    session.git_branch = branch;
                }
            });
        });
    }

    /// The active session's current branch (for the commit dialog header).
    pub(crate) fn git_branch_name(&self, target_id: &str) -> Option<String> {
        self.git_status
            .get(target_id)
            .and_then(|s| s.branch.clone())
    }

    /// Generate a commit message with the current provider (Claude, headless
    /// `claude -p`) for the active session, scoped to `included` paths. Returns
    /// a task the caller (commit dialog) awaits to fill the message field.
    pub fn generate_commit_message(
        &self,
        target_id: &str,
        included: Option<Vec<String>>,
        cx: &HostCx,
    ) -> HostTask<Result<String, String>> {
        let Some(cwd) = self.resident(target_id).map(|a| a.meta.cwd.clone()) else {
            return HostCx::spawn_background(cx, async { Err("no active session".to_string()) });
        };
        let binary = self.settings.provider(ProviderKind::ClaudeCode).binary_path;
        HostCx::spawn_background(cx, async move {
            let (stat, patch) = commit_diff_context(&cwd, included.as_deref());
            let prompt = build_commit_prompt(&stat, &patch);
            let raw = run_claude_headless(binary.as_deref(), &cwd, &prompt)?;
            let message = sanitize_commit_message(&raw);
            if message.is_empty() {
                Err("model returned an empty commit message".to_string())
            } else {
                Ok(message)
            }
        })
    }

    /// Run a resolved git quick-action in the background, tracking progress in a
    /// single toast (running → success/error with the command output as the
    /// error detail). Refreshes the git status + branch label on completion.
    ///
    /// `message` is the commit message (commit actions); `included` the checked
    /// file subset (`None` = all); `feature_branch` the safeguard's new branch.
    pub fn run_git_action(
        &mut self,
        target_id: &str,
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
        cx: &mut HostCx,
    ) {
        if self.git_busy.contains(target_id) {
            emit_runtime(cx, RuntimeEvent::Toast(RuntimeToast::GitBusy));
            return;
        }
        let Some(cwd) = self.resident(target_id).map(|a| a.meta.cwd.clone()) else {
            return;
        };
        let current_branch = self.git_branch_name(target_id);
        self.git_busy.insert(target_id.to_string());
        let operation = self.next_operation_id();
        let retry = GitActionRequest {
            session_id: target_id.to_string(),
            action,
            message: message.clone(),
            included: included.clone(),
            feature_branch: feature_branch.clone(),
        };
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::GitStarted { operation, action }),
        );

        let target_id = target_id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let (result, git_branch) = host_cx
                .unblock(move || {
                    let result = perform_action(
                        &cwd,
                        action,
                        message.as_deref(),
                        included.as_deref(),
                        feature_branch.as_deref(),
                        current_branch.as_deref(),
                    );
                    let git_branch = read_git_branch(&cwd);
                    (result, git_branch)
                })
                .await;
            host_cx.enqueue(move |state, cx| {
                state.git_busy.remove(&target_id);
                match &result {
                    Ok(_) => emit_runtime(
                        cx,
                        RuntimeEvent::Toast(RuntimeToast::GitSucceeded { operation, action }),
                    ),
                    Err(detail) => emit_runtime(
                        cx,
                        RuntimeEvent::Toast(RuntimeToast::GitFailed {
                            operation,
                            detail: detail.clone(),
                            retry,
                        }),
                    ),
                }
                if let Some(active) = state.resident_mut(&target_id) {
                    active.git_branch = git_branch;
                }
                state.refresh_git_status(&target_id, cx);
            });
        });
    }
}
