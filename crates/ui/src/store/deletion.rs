use super::*;

impl WorkspaceStore {
    /// How many threads deleting `session_id` removes (the host deletes every
    /// thread under it too), while the archived threads are held: archived
    /// descendants are not in the index.
    pub fn held_deletion_count(&self, session_id: &str) -> Option<usize> {
        let archived = self.archived_replica.as_ref()?;
        Some(self.deletion_count(session_id, &archived.sessions))
    }

    /// [`Self::held_deletion_count`] over the archived threads, fetched once;
    /// why they could not be read, when they could not.
    pub fn fetch_deletion_count(
        &self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<usize, String>> {
        let host = self.host.clone();
        let session_id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            let archived = match host.query(Query::ArchivedSessions).await {
                Ok(QueryResponse::ArchivedSessions(archived)) => archived.sessions,
                Ok(other) => {
                    return Err(format!("unexpected archived-sessions response: {other:?}"));
                }
                Err(error) => return Err(error.message),
            };
            this.read_with(cx, |store, _| store.deletion_count(&session_id, &archived))
                .map_err(|error| error.to_string())
        })
    }

    /// What deleting every listed archived thread removes, while the archived
    /// threads are held. Only the archived threads with no archived ancestor
    /// are sent: the host deletes each one's tree, so a thread under one is
    /// neither sent nor counted twice.
    pub fn archived_deletion(&self) -> Option<ArchivedDeletion> {
        let held = self.archived_replica.as_ref()?;
        let listed: HashSet<String> = self
            .archived_groups()
            .into_iter()
            .flat_map(|group| group.sessions.into_iter().map(|meta| meta.id))
            .collect();
        let all = || self.index_replica.0.iter().chain(&held.sessions);
        let parents: HashMap<&str, &str> = all()
            .filter_map(|meta| Some((meta.id.as_str(), meta.parent_session_id.as_deref()?)))
            .collect();
        let under_archived = |id: &str| {
            let mut seen = HashSet::from([id]);
            let mut current = id;
            while let Some(&parent) = parents.get(current) {
                if !seen.insert(parent) {
                    return false;
                }
                if listed.contains(parent) {
                    return true;
                }
                current = parent;
            }
            false
        };
        let roots: Vec<String> = held
            .sessions
            .iter()
            .filter(|meta| listed.contains(&meta.id) && !under_archived(&meta.id))
            .map(|meta| meta.id.clone())
            .collect();
        let deleted: HashSet<String> = roots
            .iter()
            .flat_map(|root| descendant_session_ids(all(), root))
            .collect();
        Some(ArchivedDeletion {
            archived: listed.len(),
            unarchived: deleted.iter().filter(|id| !listed.contains(*id)).count(),
            roots,
        })
    }

    pub(super) fn deletion_count(&self, session_id: &str, archived: &[SessionMeta]) -> usize {
        descendant_session_ids(self.index_replica.0.iter().chain(archived), session_id)
            .len()
            .max(1)
    }

    pub fn worktree_orphaned_by_delete(&self, session_id: &str) -> Option<WorktreeInfo> {
        let (meta, shared) = if let Some(meta) = self
            .index_replica
            .0
            .iter()
            .find(|meta| meta.id == session_id)
        {
            (meta, &self.index_summary.worktree_shared)
        } else {
            let archived = self.archived_replica.as_ref()?;
            (
                archived
                    .sessions
                    .iter()
                    .find(|meta| meta.id == session_id)?,
                &archived.worktree_shared,
            )
        };
        // Worktrees it owns elsewhere are checked for other users when it is deleted.
        (!shared.contains(session_id))
            .then(|| meta.worktree.clone())
            .flatten()
            .or_else(|| meta.other_worktrees.first().map(|owned| owned.info.clone()))
    }
}
