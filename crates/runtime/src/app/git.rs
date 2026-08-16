use super::*;

impl AppState {
    // -- git quick actions (Group: Git) -------------------------------------

    pub(super) fn next_operation_id(&mut self) -> RuntimeOperationId {
        let id = RuntimeOperationId(self.next_operation_id);
        self.next_operation_id += 1;
        id
    }

    /// Kick off a background refresh of the active session's git status (on
    /// session open, after each turn, and after each git action). A stale result
    /// (session switched, or a newer refresh superseded it) is discarded.
    pub(crate) fn refresh_git_status(&mut self, cx: &mut HostCx) {
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            self.git_status = None;
            self.emit_git_status(cx);
            cx.notify();
            return;
        };
        let session_id = self.active_session_id().map(str::to_string);
        self.git_status_generation += 1;
        let generation = self.git_status_generation;
        self.emit_git_status(cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let status = host_cx.unblock(move || read_status(&cwd)).await;
            host_cx.enqueue(move |state, cx| {
                if state.git_status_generation == generation
                    && state.active_session_id().map(str::to_string) == session_id
                {
                    state.git_status = Some(status);
                    state.emit_git_status(cx);
                    cx.notify();
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
            host_cx.enqueue(move |state, cx| {
                if let Some(session) = state.resident_mut(&session_id) {
                    session.git_branch = branch;
                    state.emit_session_status(&session_id, cx);
                    cx.notify();
                }
            });
        });
    }

    /// The active session's current branch (for the commit dialog header).
    pub(crate) fn git_branch_name(&self) -> Option<String> {
        self.git_status.as_ref().and_then(|s| s.branch.clone())
    }

    /// Generate a commit message with the current provider (Claude, headless
    /// `claude -p`) for the active session, scoped to `included` paths. Returns
    /// a task the caller (commit dialog) awaits to fill the message field.
    pub fn generate_commit_message(
        &self,
        included: Option<Vec<String>>,
        cx: &HostCx,
    ) -> HostTask<Result<String, String>> {
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
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
        action: GitAction,
        message: Option<String>,
        included: Option<Vec<String>>,
        feature_branch: Option<String>,
        cx: &mut HostCx,
    ) {
        if self.git_busy {
            emit_runtime(cx, RuntimeEvent::Toast(RuntimeToast::GitBusy));
            return;
        }
        let Some(cwd) = self.active.as_ref().map(|a| a.meta.cwd.clone()) else {
            return;
        };
        let current_branch = self.git_branch_name();
        self.git_busy = true;
        self.emit_git_status(cx);
        let operation = self.next_operation_id();
        let retry = GitActionRequest {
            action,
            message: message.clone(),
            included: included.clone(),
            feature_branch: feature_branch.clone(),
        };
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::GitStarted { operation, action }),
        );

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
                state.git_busy = false;
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
                if let Some(active) = state.active.as_mut() {
                    active.git_branch = git_branch;
                }
                state.emit_active_session_status(cx);
                state.emit_git_status(cx);
                state.refresh_git_status(cx);
                cx.notify();
            });
        });
    }
}
