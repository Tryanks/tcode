use super::*;
use control_mcp::{
    GetGitParams, GetPullRequestParams, GetSettingsParams, GetThreadParams, ListProjectsParams,
    ListProvidersParams, ListThreadsParams, ReadTerminalParams, ReviewCandidatesParams,
    SearchParams,
};

impl AppState {
    pub(super) fn op_list_projects(
        &mut self,
        caller: &Caller,
        _params: ListProjectsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let projects: Vec<serde_json::Value> = self
            .projects
            .iter()
            .filter(|project| {
                caller
                    .grant
                    .scope
                    .reaches(caller.project_id.as_deref(), Some(&project.id))
            })
            .map(|project| {
                let (mut active, mut settled, mut archived) = (0, 0, 0);
                for meta in self
                    .sessions
                    .iter()
                    .filter(|meta| meta.project_id.as_deref() == Some(project.id.as_str()))
                {
                    if meta.archived_at.is_some() {
                        archived += 1;
                    } else if meta.is_settled() {
                        settled += 1;
                    } else {
                        active += 1;
                    }
                }
                serde_json::json!({
                    "id": project.id,
                    "name": project.name,
                    "root": project.root,
                    "quick_chat": project.quick_chat,
                    "threads": { "active": active, "settled": settled, "archived": archived },
                })
            })
            .collect();
        Ok(serde_json::json!({ "projects": projects }))
    }

    pub(super) fn op_list_threads(
        &mut self,
        _caller: &Caller,
        _params: ListThreadsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_get_thread(
        &mut self,
        _caller: &Caller,
        _params: GetThreadParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_search(
        &mut self,
        _caller: &Caller,
        _params: SearchParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_get_git(
        &mut self,
        _caller: &Caller,
        _params: GetGitParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_get_pull_request(
        &mut self,
        _caller: &Caller,
        _params: GetPullRequestParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_list_providers(
        &mut self,
        _caller: &Caller,
        _params: ListProvidersParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    /// Settings hold no secret values (those live in the keychain-backed secret store), so the
    /// whole document is readable; `keys` narrows it to top-level fields.
    pub(super) fn op_get_settings(
        &mut self,
        _caller: &Caller,
        params: GetSettingsParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let mut settings = serde_json::to_value(&self.settings).map_err(|e| e.to_string())?;
        if let Some(keys) = params.keys
            && let serde_json::Value::Object(fields) = &mut settings
        {
            fields.retain(|key, _| keys.iter().any(|wanted| wanted == key));
            let missing: Vec<&str> = keys
                .iter()
                .map(String::as_str)
                .filter(|key| !fields.contains_key(*key))
                .collect();
            if !missing.is_empty() {
                return Err(format!("unknown settings keys: {}", missing.join(", ")));
            }
        }
        Ok(serde_json::json!({ "settings": settings }))
    }

    pub(super) fn op_read_terminal(
        &mut self,
        _caller: &Caller,
        _params: ReadTerminalParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }

    pub(super) fn op_review_candidates(
        &mut self,
        _caller: &Caller,
        _params: ReviewCandidatesParams,
        _cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        // Phase 2
        Err("not implemented yet".into())
    }
}
