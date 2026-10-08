use super::*;
use tcode_core::pull_request::{
    self, PullRequestKey, PullRequestSnapshot, PullRequestSource, PullRequestStackState,
    PullRequestState, PullRequestSyncError,
};
use tcode_protocol::{CommandResponse, ProtocolError};
use tcode_services::github::{
    GitHubApi, GitHubError,
    pull_requests::{PullRequests, Summary},
    repository::{self, BranchHead, Repository},
};

pub(super) const LINKING_INSTRUCTIONS: &str = "<pull_request_linking>\nWhen the tcode_pull_requests MCP server exposes link_pull_request, use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, link every layer, not just the current branch or top PR. This applies to gh, gh stack, other CLIs and host APIs: they do not register PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link anything missing. Do not link unrelated PRs mentioned only as background. If linking fails, report that failure instead of claiming the PR is linked.\nFor dependent changes, GitHub native stacks preserve the full bottom-to-top topology and merge scope; see https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/working-with-stacked-pull-requests .\n</pull_request_linking>\n\n";

pub(super) struct PullRequestRuntime {
    service: Arc<PullRequests>,
    last_synced: HashMap<PullRequestKey, u64>,
    requested: HashMap<PullRequestKey, u64>,
    generation: u64,
    paused: HashMap<(Option<String>, String), SystemTime>,
    syncing: bool,
    discovering: bool,
    discover_again: HashSet<String>,
    merge_commands: HashSet<String>,
    discovered: HashMap<String, (BranchHead, PullRequestKey)>,
    workers: Vec<HostTask<()>>,
}
impl PullRequestRuntime {
    pub(super) fn new(api: Arc<GitHubApi>) -> Self {
        Self {
            service: PullRequests::new(api),
            last_synced: HashMap::new(),
            requested: HashMap::new(),
            generation: 0,
            paused: HashMap::new(),
            syncing: false,
            discovering: false,
            discover_again: HashSet::new(),
            merge_commands: HashSet::new(),
            discovered: HashMap::new(),
            workers: Vec::new(),
        }
    }
}
struct SyncGroup {
    key: PullRequestKey,
    projects: Vec<Option<String>>,
    threads: Vec<String>,
    observations: Vec<(Option<PullRequestSnapshot>, PullRequestStackState)>,
    request: Option<u64>,
}

fn failure(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "pull_request_failed".into(),
        message: message.into(),
    }
}
fn resolve_reference(
    cwd: &Path,
    reference: &str,
) -> Result<(PullRequestKey, String), ProtocolError> {
    if let Some(target) = repository::pull_request_url(reference) {
        return Ok(target);
    }
    let invalid = || ProtocolError {
        code: "pull_request_invalid_reference".into(),
        message: "Enter a pull request URL or positive number.".into(),
    };
    let number: u64 = reference
        .trim()
        .trim_start_matches('#')
        .parse()
        .map_err(|_| invalid())?;
    if number == 0 {
        return Err(invalid());
    }
    let repository = repository::resolve(cwd).ok_or_else(|| ProtocolError {
        code: "pull_request_no_repository".into(),
        message: "This project has no GitHub repository. Use a full PR URL.".into(),
    })?;
    Ok((repository.key(number), repository.url(number)))
}
impl AppState {
    pub(crate) fn pump_pull_request_requests(
        &mut self,
        server: Option<pull_request_mcp::PullRequestMcpServer>,
        cx: &mut HostCx,
    ) {
        let Some(server) = server else { return };
        self.mcp.pull_request_url = Some(server.url);
        self.mcp.pull_request_tokens = Some(server.tokens);
        let host = cx.clone();
        cx.spawn_detached(async move {
            while let Ok(request) = server.requests.recv().await {
                host.enqueue(move |state, cx| state.handle_pull_request_request(request, cx));
            }
        });
    }
    pub(super) fn pull_request_registration_for(
        &mut self,
        meta: &SessionMeta,
    ) -> Option<agent::McpRegistration> {
        if !meta.provider.caps().mcp_servers {
            return None;
        }
        if let Some(registration) = self.mcp.pull_request_registrations.get(&meta.id) {
            return Some(registration.clone());
        }
        let url = self.mcp.pull_request_url.clone()?;
        let bearer_token = self.mcp.pull_request_tokens.as_ref()?.register(&meta.id);
        let registration = agent::McpRegistration {
            name: "tcode_pull_requests".into(),
            url,
            bearer_token,
        };
        self.mcp
            .pull_request_registrations
            .insert(meta.id.clone(), registration.clone());
        Some(registration)
    }
    pub(super) fn revoke_pull_request_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.pull_request_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.pull_request_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }
    fn handle_pull_request_request(
        &mut self,
        request: pull_request_mcp::BrokerRequest,
        cx: &mut HostCx,
    ) {
        use pull_request_mcp::Operation;
        let Some(meta) = self.find_meta(&request.session_id) else {
            let _ = request.reply.try_send(Err("Unknown thread.".into()));
            return;
        };
        if !self.mcp.pull_request_registrations.contains_key(&meta.id) {
            let _ = request.reply.try_send(Err(
                "Pull request tools are unavailable for this thread.".into(),
            ));
            return;
        }
        if matches!(request.operation, Operation::List) {
            let groups = pull_request::groups(&meta.pull_requests);
            let mut positions = HashMap::new();
            let chains:Vec<_>=groups.iter().map(|group| {
                let kind=match group.kind {pull_request::PullRequestGroupKind::Native=>"native",pull_request::PullRequestGroupKind::Derived=>"derived",pull_request::PullRequestGroupKind::Single=>"single"};
                if group.links.len()>1 {for (position,link) in group.links.iter().enumerate() {positions.insert(link.key.clone(),serde_json::json!({"kind":kind,"position":position+1,"size":group.links.len()}));}}
                serde_json::json!({"kind":kind,"numbers":group.links.iter().map(|link|link.key.number).collect::<Vec<_>>()})
            }).collect();
            let rows: Vec<_> = meta.pull_requests.iter().filter(|link|link.visible()).map(|link|serde_json::json!({
                "host":link.key.host,"repository":link.key.repository,"number":link.key.number,"url":link.url,"source":link.source,"watching":link.watch.is_some(),
                "state":link.snapshot.as_ref().map(|snapshot|snapshot.state),"title":link.snapshot.as_ref().map(|snapshot|&snapshot.title),
                "headBranch":link.snapshot.as_ref().map(|snapshot|&snapshot.head_branch),"baseBranch":link.snapshot.as_ref().map(|snapshot|&snapshot.base_branch),"isDraft":link.snapshot.as_ref().map(|snapshot|snapshot.is_draft),"stack":positions.get(&link.key)
            })).collect();
            let _ = request
                .reply
                .try_send(Ok(serde_json::json!({"pullRequests":rows,"chains":chains})));
            return;
        }
        let (target, linking) = match request.operation {
            Operation::Link(target) => (target, true),
            Operation::Unlink(target) => (target, false),
            Operation::List => unreachable!(),
        };
        let cwd = self.pull_request_project_cwd(&meta);
        let host = cx.clone();
        cx.spawn_detached(async move {
            let target = host.unblock(move || {
                if let Some(url) = target.url { return repository::pull_request_url(&url).ok_or_else(|| "Invalid pull request URL.".to_owned()) }
                let number = target.number.filter(|n| *n > 0).ok_or_else(|| "Pass url or repository plus number.".to_owned())?;
                let name = target.repository.ok_or_else(|| "Pass url or repository plus number.".to_owned())?;
                let host = target.host.or_else(|| repository::resolve(&cwd).map(|r| r.host)).ok_or_else(|| "Pass host or a full PR URL.".to_owned())?;
                let repository = repository::selector(&name, &host).ok_or_else(|| "Invalid GitHub repository.".to_owned())?;
                Ok((repository.key(number),repository.url(number)))
            }).await;
            host.enqueue(move |state,cx| {
                let result = target.and_then(|(key,url)| {
                    let linked = state.find_meta(&request.session_id).ok_or_else(|| "Thread disappeared.".to_owned())?.pull_requests.iter().any(|link| link.visible() && link.key == key);
                    if linking { state.apply_pull_request_link(&request.session_id,key.clone(),url.clone(),PullRequestSource::Agent,true,cx)?; } else { state.unlink_pull_request(&request.session_id,&key,cx); }
                    Ok(serde_json::json!({"host":key.host,"repository":key.repository,"number":key.number,"url":url,"alreadyLinked": linking && linked,"wasLinked": !linking && linked}))
                });
                let _ = request.reply.try_send(result);
            });
        });
    }
    fn pull_request_project_cwd(&self, meta: &SessionMeta) -> PathBuf {
        self.projects
            .iter()
            .find(|project| Some(&project.id) == meta.project_id.as_ref())
            .map(|project| project.root.clone())
            .unwrap_or_else(|| meta.cwd.clone())
    }
    pub fn link_pull_request(
        &mut self,
        session_id: &str,
        reference: String,
        source: PullRequestSource,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let meta = self.find_meta(session_id);
        let cwd = meta
            .as_ref()
            .map(|meta| self.pull_request_project_cwd(meta));
        let id = session_id.to_owned();
        let host = cx.clone();
        cx.spawn_background(async move {
            let cwd = cwd.ok_or_else(|| failure("Unknown thread."))?;
            let target = host
                .unblock(move || resolve_reference(&cwd, &reference))
                .await?;
            let key = target.0.clone();
            let already_linked = host
                .enqueue_and_wait(move |state, cx| {
                    let already_linked = state.find_meta(&id).is_some_and(|meta| {
                        meta.pull_requests
                            .iter()
                            .any(|link| link.visible() && link.key == target.0)
                    });
                    state.apply_pull_request_link(&id, target.0, target.1, source, true, cx)?;
                    Ok::<_, String>(already_linked)
                })
                .await
                .map_err(|_| failure("Host closed."))?
                .map_err(failure)?;
            Ok(CommandResponse::PullRequestLinked {
                key,
                already_linked,
            })
        })
    }
    fn save_pull_request_meta(&mut self, meta: SessionMeta, cx: &mut HostCx) {
        if let Some(resident) = self.meta_mut(&meta.id) {
            resident.pull_requests.clone_from(&meta.pull_requests);
        }
        self.persist_meta(&meta, cx);
    }
    fn apply_pull_request_link(
        &mut self,
        id: &str,
        key: PullRequestKey,
        url: String,
        source: PullRequestSource,
        explicit: bool,
        cx: &mut HostCx,
    ) -> Result<(), String> {
        let mut meta = self
            .find_meta(id)
            .ok_or_else(|| "Unknown thread.".to_owned())?;
        if meta.native_subagent.is_some() {
            return Err("This thread is read-only.".into());
        }
        if pull_request::link_pull_request(
            &mut meta.pull_requests,
            key.clone(),
            url,
            source,
            now_secs(),
            explicit,
        ) {
            self.save_pull_request_meta(meta, cx);
            self.request_pull_request_sync(key, cx);
        }
        Ok(())
    }
    pub fn unlink_pull_request(&mut self, id: &str, key: &PullRequestKey, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id) else {
            return;
        };
        if meta.native_subagent.is_some() {
            return;
        }
        if pull_request::unlink_pull_request(&mut meta.pull_requests, key) {
            self.save_pull_request_meta(meta, cx);
        }
    }
    pub fn refresh_thread_pull_requests(&mut self, id: &str, cx: &mut HostCx) {
        if let Some(meta) = self.find_meta(id) {
            for link in meta.pull_requests.into_iter().filter(|link| link.visible()) {
                self.request_pull_request_sync(link.key, cx);
            }
        }
    }
    fn request_pull_request_sync(&mut self, key: PullRequestKey, cx: &mut HostCx) {
        self.pull_requests.generation += 1;
        self.pull_requests
            .requested
            .insert(key, self.pull_requests.generation);
        if self.pull_requests.syncing {
            return;
        }
        let host = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(Duration::from_millis(10)).await;
            host.enqueue(move |state, cx| {
                state.sweep_pull_requests(true, cx).detach();
            });
        });
    }
    pub(super) fn stop_pull_request_workers(&mut self) {
        self.pull_requests.workers.clear();
    }
    pub(crate) fn start_pull_request_workers(&mut self, cx: &mut HostCx) {
        for discovery in [false, true] {
            let host = cx.clone();
            let task = cx.spawn_background(async move {
                loop {
                    let task = host
                        .enqueue_and_wait(move |state, cx| {
                            if discovery {
                                state.discover_pull_requests(None, false, cx)
                            } else {
                                state.sweep_pull_requests(false, cx)
                            }
                        })
                        .await;
                    let Ok(task) = task else { break };
                    task.await;
                    smol::Timer::after(Duration::from_secs(60)).await;
                }
            });
            self.pull_requests.workers.push(task);
        }
    }
    pub(super) fn sweep_pull_requests(
        &mut self,
        requested_only: bool,
        cx: &mut HostCx,
    ) -> HostTask<()> {
        if self.pull_requests.syncing {
            return cx.spawn_background(async {});
        }
        self.pull_requests.syncing = true;
        let sweep_generation = self.pull_requests.generation;
        let now = now_secs();
        let mut groups: HashMap<PullRequestKey, SyncGroup> = HashMap::new();
        let metas: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|meta| self.find_meta(&meta.id))
            .filter(|meta| meta.archived_at.is_none())
            .collect();
        for meta in metas {
            for link in meta.pull_requests.iter().filter(|link| link.visible()) {
                let group = groups.entry(link.key.clone()).or_insert_with(|| SyncGroup {
                    key: link.key.clone(),
                    projects: Vec::new(),
                    threads: Vec::new(),
                    observations: Vec::new(),
                    request: self.pull_requests.requested.get(&link.key).copied(),
                });
                group.threads.push(meta.id.clone());
                if !group.projects.contains(&meta.project_id) {
                    group.projects.push(meta.project_id.clone());
                }
                // Settled threads share updates without independently scheduling reads.
                if meta.settled_at.is_none() {
                    group
                        .observations
                        .push((link.snapshot.clone(), link.stack.clone()));
                }
            }
        }
        self.pull_requests
            .last_synced
            .retain(|key, _| groups.contains_key(key));
        self.pull_requests
            .requested
            .retain(|key, _| groups.contains_key(key));
        let due: Vec<_> = groups
            .into_values()
            .filter(|group| {
                if requested_only && group.request.is_none() {
                    return false;
                }
                if group.projects.iter().all(|project| {
                    self.pull_requests
                        .paused
                        .get(&(project.clone(), group.key.host.clone()))
                        .is_some_and(|until| *until > SystemTime::now())
                }) {
                    return false;
                }
                group.request.is_some()
                    || group
                        .observations
                        .iter()
                        .any(|(snapshot, _)| snapshot.is_none())
                    || group.observations.iter().any(|(snapshot, _)| {
                        snapshot
                            .as_ref()
                            .is_some_and(|s| s.state == PullRequestState::Open)
                    })
                    || (group.observations.iter().any(|(snapshot, _)| {
                        snapshot
                            .as_ref()
                            .is_some_and(|s| s.state == PullRequestState::Closed)
                    }) && self
                        .pull_requests
                        .last_synced
                        .get(&group.key)
                        .is_none_or(|last| now.saturating_sub(*last) >= 900))
            })
            .collect();
        let service = self.pull_requests.service.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            let mut failures = HashMap::<String, usize>::new();
            for chunk in due.chunks(25) {
                let mut tasks = Vec::new();
                for group in chunk {
                    let key = group.key.clone();
                    let observations = group.observations.clone();
                    let forced = group.request.is_some();
                    let service = service.clone();
                    let paused = host
                        .enqueue_and_wait({
                            let projects = group.projects.clone();
                            let host_name = key.host.clone();
                            move |state, _| {
                                projects.iter().all(|project| {
                                    state
                                        .pull_requests
                                        .paused
                                        .get(&(project.clone(), host_name.clone()))
                                        .is_some_and(|until| *until > SystemTime::now())
                                })
                            }
                        })
                        .await
                        .unwrap_or(true);
                    if paused {
                        continue;
                    }
                    let result = host.unblock(move || {
                        let summary = service.summary(&key, false)?;
                        let changed = observations.iter().any(|(snapshot, stack)| {
                            snapshot
                                .as_ref()
                                .is_none_or(|s| !s.same_observation(&summary.snapshot))
                                || match summary.stack_number {
                                    Some(number) => {
                                        number
                                            != match stack {
                                                PullRequestStackState::Native(stack) => {
                                                    Some(stack.number)
                                                }
                                                _ => None,
                                            }
                                    }
                                    None => false,
                                }
                        });
                        let stack = if key.host != "github.com" {
                            Some(PullRequestStackState::Unknown)
                        } else if summary.stack_number == Some(None) {
                            Some(PullRequestStackState::None)
                        } else if forced || changed {
                            Some(service.stack(&key, false)?)
                        } else {
                            None
                        };
                        Ok::<_, GitHubError>((summary, stack))
                    });
                    tasks.push((
                        group.key.clone(),
                        group.projects.clone(),
                        group.threads.clone(),
                        group.request,
                        result,
                    ));
                }
                for (key, project, threads, generation, task) in tasks {
                    let result = task.await;
                    if let Err(error) = &result {
                        *failures.entry(error.to_string()).or_default() += 1;
                    }
                    let _ = host
                        .enqueue_and_wait(move |state, cx| {
                            state.finish_pull_request_sync(
                                key, project, threads, generation, result, cx,
                            )
                        })
                        .await;
                }
            }
            for (reason, count) in failures {
                log::warn!("pull request sweep skipped {count} reads: {reason}");
            }
            let _ = host
                .enqueue_and_wait(move |state, cx| {
                    state.pull_requests.syncing = false;
                    if state
                        .pull_requests
                        .requested
                        .values()
                        .any(|generation| *generation > sweep_generation)
                    {
                        state.sweep_pull_requests(true, cx).detach();
                    }
                })
                .await;
        })
    }
    fn finish_pull_request_sync(
        &mut self,
        key: PullRequestKey,
        projects: Vec<Option<String>>,
        threads: Vec<String>,
        generation: Option<u64>,
        result: Result<(Summary, Option<PullRequestStackState>), GitHubError>,
        cx: &mut HostCx,
    ) {
        if let Err(error) = &result {
            let reason = match error {
                GitHubError::Credential(tcode_services::github::CredentialError::Disabled) => {
                    PullRequestSyncError::HostDisabled
                }
                GitHubError::Credential(_) | GitHubError::Unauthorized => {
                    PullRequestSyncError::NoCredential
                }
                GitHubError::RateLimited { retry_at, .. } | GitHubError::Paused { retry_at } => {
                    PullRequestSyncError::RateLimited {
                        retry_at: retry_at
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    }
                }
                GitHubError::NotFound => PullRequestSyncError::NotFound,
                _ => PullRequestSyncError::Failed,
            };
            for id in &threads {
                if let Some(mut meta) = self.find_meta(id)
                    && let Some(link) = meta
                        .pull_requests
                        .iter_mut()
                        .find(|link| link.key == key && link.visible() && link.snapshot.is_none())
                    && link.sync_error.as_ref() != Some(&reason)
                {
                    link.sync_error = Some(reason.clone());
                    self.save_pull_request_meta(meta, cx);
                }
            }
        }
        let (summary, stack) = match result {
            Ok(result) => result,
            Err(GitHubError::Paused { retry_at } | GitHubError::RateLimited { retry_at, .. }) => {
                for project in projects {
                    let until = self
                        .pull_requests
                        .paused
                        .entry((project, key.host.clone()))
                        .or_insert(retry_at);
                    *until = (*until).max(retry_at);
                }
                return;
            }
            Err(error) => {
                log::debug!("pull request summary skipped: {error}");
                return;
            }
        };
        self.pull_requests
            .last_synced
            .insert(key.clone(), now_secs());
        if self.pull_requests.requested.get(&key).copied() == generation {
            self.pull_requests.requested.remove(&key);
        }
        for id in threads {
            let Some(mut meta) = self
                .find_meta(&id)
                .filter(|meta| meta.archived_at.is_none())
            else {
                continue;
            };
            let Some(link) = meta
                .pull_requests
                .iter()
                .find(|link| link.key == key && link.visible())
            else {
                continue;
            };
            let stack = stack.clone().unwrap_or_else(|| link.stack.clone());
            let observation_changed = link
                .snapshot
                .as_ref()
                .is_none_or(|s| !s.same_observation(&summary.snapshot))
                || link.stack != stack;
            let mut changed = observation_changed;
            if meta.settled_at.is_none()
                && let PullRequestStackState::Native(topology) = &stack
            {
                for layer in &topology.layers {
                    let sibling = PullRequestKey::new(&key.host, &key.repository, layer.number);
                    let Some(repository) = Repository::from_key(&sibling) else {
                        continue;
                    };
                    if pull_request::link_pull_request(
                        &mut meta.pull_requests,
                        sibling.clone(),
                        repository.url(layer.number),
                        PullRequestSource::Stack,
                        now_secs(),
                        false,
                    ) {
                        changed = true;
                        if let Some(link) = meta
                            .pull_requests
                            .iter_mut()
                            .find(|link| link.key == sibling)
                        {
                            link.stack = stack.clone();
                        }
                        self.pull_requests.generation += 1;
                        self.pull_requests
                            .requested
                            .insert(sibling, self.pull_requests.generation);
                    }
                }
            }
            if !changed {
                continue;
            }
            if observation_changed
                && let Some(link) = meta
                    .pull_requests
                    .iter_mut()
                    .find(|link| link.key == key && link.visible())
            {
                link.sync_error = None;
                link.snapshot = Some(summary.snapshot.clone());
                link.stack = stack;
            }
            self.save_pull_request_meta(meta, cx);
        }
    }
    pub(super) fn discover_pull_requests(
        &mut self,
        thread: Option<String>,
        refresh: bool,
        cx: &mut HostCx,
    ) -> HostTask<()> {
        if self.pull_requests.discovering {
            if let Some(thread) = thread {
                self.pull_requests.discover_again.insert(thread);
            }
            return cx.spawn_background(async {});
        }
        self.pull_requests.discovering = true;
        let metas: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| thread.as_ref().is_none_or(|id| &meta.id == id))
            .filter_map(|meta| self.find_meta(&meta.id))
            .filter(|meta| meta.archived_at.is_none() && meta.settled_at.is_none())
            .map(|meta| {
                let root = self.pull_request_project_cwd(&meta);
                (meta, root)
            })
            .collect();
        let mut grouped = HashMap::<(PathBuf, PathBuf), Vec<SessionMeta>>::new();
        for (meta, root) in metas {
            let cwd = if meta.worktree.is_some() {
                meta.cwd.clone()
            } else {
                root.clone()
            };
            grouped.entry((cwd, root)).or_default().push(meta);
        }
        let groups: Vec<_> = grouped.into_iter().collect();
        let service = self.pull_requests.service.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            for chunk in groups.chunks(32) {
                let mut tasks = Vec::new();
                for ((cwd, root), metas) in chunk {
                    let cwd = cwd.clone();
                    let root = root.clone();
                    let service = service.clone();
                    tasks.push((
                        metas.clone(),
                        root.clone(),
                        host.unblock(move || {
                            let cwd = if cwd.exists() { cwd } else { root.clone() };
                            let head = repository::branch_head(&cwd)?;
                            let project_repository = repository::resolve(&root)?;
                            if head.repository != project_repository {
                                return None;
                            }
                            let result = match service.branch(&head, refresh) {
                                Ok(Some(result)) => result,
                                _ => return None,
                            };
                            if result.key.host != project_repository.host
                                || result.key.repository
                                    != project_repository.key(result.key.number).repository
                            {
                                return None;
                            }
                            if repository::branch_head(&cwd).as_ref() != Some(&head)
                                || repository::resolve(&root).as_ref() != Some(&project_repository)
                            {
                                return None;
                            }
                            Some((head, result))
                        }),
                    ));
                }
                for (metas, root, task) in tasks {
                    let Some((head, result)) = task.await else {
                        continue;
                    };
                    for meta in metas {
                        let head = head.clone();
                        let result = result.clone();
                        let root = root.clone();
                        let _ = host
                            .enqueue_and_wait(move |state, cx| {
                                let Some(current) = state.find_meta(&meta.id) else {
                                    return;
                                };
                                if current.cwd != meta.cwd
                                    || current.worktree != meta.worktree
                                    || current.project_id != meta.project_id
                                    || current.archived_at.is_some()
                                    || current.settled_at.is_some()
                                    || state.pull_request_project_cwd(&current) != root
                                {
                                    return;
                                }
                                if state
                                    .resident(&meta.id)
                                    .and_then(|r| r.git_branch.as_ref())
                                    .is_some_and(|branch| branch != &head.branch)
                                {
                                    return;
                                }
                                if state.pull_requests.discovered.get(&meta.id).is_some_and(
                                    |(previous, key)| previous == &head && key == &result.key,
                                ) {
                                    return;
                                }
                                state
                                    .pull_requests
                                    .discovered
                                    .insert(meta.id.clone(), (head, result.key.clone()));
                                let _ = state.apply_pull_request_link(
                                    &meta.id,
                                    result.key,
                                    result.url,
                                    PullRequestSource::Created,
                                    false,
                                    cx,
                                );
                            })
                            .await;
                    }
                }
            }
            let _ = host
                .enqueue_and_wait(|state, cx| {
                    state.pull_requests.discovering = false;
                    let pending = std::mem::take(&mut state.pull_requests.discover_again);
                    if !pending.is_empty() {
                        state.discover_pull_requests(None, true, cx).detach();
                    }
                })
                .await;
        })
    }
    pub(super) fn observe_pull_request_event(
        &mut self,
        id: &str,
        event: &AgentEvent,
        cx: &mut HostCx,
    ) {
        if let AgentEvent::ItemStarted(item) | AgentEvent::ItemCompleted(item) = event
            && let ItemContent::CommandExecution { command, .. } = &item.content
        {
            let words: Vec<_> = command.split_whitespace().collect();
            if words.windows(3).any(|words| {
                words[0] == "gh" && words[1] == "pr" && matches!(words[2], "merge" | "close")
            }) {
                self.pull_requests.merge_commands.insert(id.to_owned());
            }
        }
        if matches!(
            event,
            AgentEvent::TurnCompleted { .. } | AgentEvent::TurnCheckpoint { .. }
        ) {
            if self.pull_requests.merge_commands.remove(id) {
                self.refresh_thread_pull_requests(id, cx);
            }
            self.discover_pull_requests(Some(id.to_owned()), true, cx)
                .detach();
        }
    }
}

#[cfg(test)]
#[path = "pull_requests_tests.rs"]
mod tests;
