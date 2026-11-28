//! The threads a host's index leaves out: those settled, read a page at a
//! time as a Settled shelf shows them, and a thread outside the index with
//! what it started, read as a family when it is opened.

use super::*;
use std::cmp::Reverse;
use tcode_core::thread_sort::{in_index, in_roster, lead, settled_rank};
use tcode_protocol::{ProtocolError, SessionPage, SettledCursor, SettledScope};

/// How far the pages read of one scope's settled threads reach.
#[derive(Clone, Default, PartialEq)]
struct Reach {
    /// The last settled thread the pages hold.
    through: Option<SettledCursor>,
    /// No settled thread follows `through`.
    complete: bool,
}

/// What a list needs of a store to show Settled shelves: how many settled
/// threads the host lists, and which of those held here each shelf's pages
/// reach. A settled thread held from elsewhere that they do not reach has
/// threads before it that are not held.
#[derive(Clone, Default, PartialEq)]
pub struct SettledHeld {
    counts: HashMap<String, usize>,
    reach: HashMap<SettledScope, Reach>,
}

impl SettledHeld {
    pub fn count(&self, scope: &SettledScope) -> usize {
        self.counts
            .iter()
            .filter(|(project, _)| scope.holds(Some(project)))
            .map(|(_, count)| count)
            .sum()
    }

    pub fn reached(&self, scope: &SettledScope, meta: &SessionMeta) -> bool {
        self.reach
            .get(scope)
            .is_some_and(|reach| reach.reaches(meta))
    }
}

/// What is held of one scope's settled threads.
#[derive(Default)]
pub(super) struct SettledShelf {
    reach: Reach,
    /// How many threads the pages read held.
    loaded: usize,
    /// The host's index was read again since the pages were.
    stale: bool,
    /// Why the last page could not be read, until the index is read again.
    failed: bool,
    task: Option<Task<()>>,
}

impl Reach {
    fn reaches(&self, meta: &SessionMeta) -> bool {
        self.complete
            || self
                .through
                .as_ref()
                .is_some_and(|through| settled_rank(meta) <= (Reverse(through.at), &through.id))
    }
}

/// How many settled threads a title search holds.
const FOUND_SETTLED: u32 = 20;

fn cursor(meta: &SessionMeta) -> SettledCursor {
    SettledCursor {
        at: settled_rank(meta).0.0,
        id: meta.id.clone(),
    }
}

impl WorkspaceStore {
    /// Whether the host's index carries `meta`, as far as what is held of
    /// the threads it descends from tells.
    pub(super) fn in_index(&self, meta: &SessionMeta) -> bool {
        in_index(meta, |id| self.thread_meta(id))
    }

    /// A thread held here that the host's index does not carry.
    pub(super) fn outside_index(&self, session_id: &str) -> bool {
        self.thread_meta(session_id)
            .is_some_and(|meta| !self.in_index(meta))
    }

    pub fn settled_held(&self) -> SettledHeld {
        SettledHeld {
            counts: self.index_summary.settled_counts.clone(),
            reach: self
                .settled_shelves
                .iter()
                .map(|(scope, shelf)| (scope.clone(), shelf.reach.clone()))
                .collect(),
        }
    }

    /// Read `scope`'s settled threads until `rows` of them are held, or all
    /// of them. After the host's index was read again, the pages held are
    /// read again in one.
    pub fn want_settled(&mut self, scope: &SettledScope, rows: usize, cx: &mut Context<Self>) {
        if !self.baseline_topics.contains(&self.index_topic()) {
            return;
        }
        let shelf = self.settled_shelves.entry(scope.clone()).or_default();
        if shelf.task.is_some() || shelf.failed {
            return;
        }
        let fresh = shelf.stale || (shelf.reach.through.is_none() && !shelf.reach.complete);
        let (before, limit) = if fresh {
            (None, rows.max(shelf.loaded))
        } else if shelf.loaded < rows && !shelf.reach.complete {
            (shelf.reach.through.clone(), rows - shelf.loaded)
        } else {
            return;
        };
        if limit == 0 {
            return;
        }
        let host = self.host.clone();
        let scope = scope.clone();
        let query = Query::SettledSessions {
            scope: scope.clone(),
            before,
            limit: limit as u32,
        };
        shelf.task = Some(cx.spawn(async move |this, cx| {
            let result = host.query(query).await;
            let _ = this.update(cx, |store, cx| {
                store.settled_page_read(&scope, fresh, limit, result, cx)
            });
        }));
    }

    fn settled_page_read(
        &mut self,
        scope: &SettledScope,
        fresh: bool,
        limit: usize,
        result: Result<QueryResponse, ProtocolError>,
        cx: &mut Context<Self>,
    ) {
        let Some(shelf) = self.settled_shelves.get_mut(scope) else {
            return;
        };
        shelf.task = None;
        let page = match result {
            Ok(QueryResponse::SettledSessions(page)) => page,
            Ok(other) => {
                shelf.failed = true;
                log::error!("unexpected settled-sessions response: {other:?}");
                return;
            }
            Err(error) => {
                shelf.failed = true;
                cx.emit(RuntimeEvent::Error(tcode_protocol::RuntimeError::External(
                    error.message,
                )));
                cx.notify();
                return;
            }
        };
        shelf.reach.complete = page.sessions.len() < limit;
        if let Some(last) = page.sessions.last() {
            shelf.reach.through = Some(cursor(last));
        }
        if fresh {
            shelf.loaded = page.sessions.len();
            shelf.stale = false;
            if page.sessions.is_empty() {
                shelf.reach.through = None;
            }
            // A settled thread the pages reach and no longer hold is gone
            // from the host's settled threads.
            let reach = &self.settled_shelves[scope].reach;
            let gone: Vec<String> = self
                .index_replica
                .0
                .iter()
                .filter(|meta| in_roster(meta) && meta.is_settled() && reach.reaches(meta))
                .filter(|meta| scope.holds(meta.project_id.as_deref()))
                .filter(|meta| !page.sessions.iter().any(|held| held.id == meta.id))
                .map(|meta| meta.id.clone())
                .collect();
            self.forget_threads(gone);
        } else {
            shelf.loaded += page.sessions.len();
        }
        self.hold_page(page);
        self.outside_index_changed(cx);
    }

    /// Hold the settled threads whose titles match `query`, which a search
    /// over what is held would otherwise miss.
    pub fn find_settled(&mut self, query: String, cx: &mut Context<Self>) -> Task<()> {
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let found = host
                .query(Query::FindSettledSessions {
                    query,
                    limit: FOUND_SETTLED,
                })
                .await;
            let _ = this.update(cx, |store, cx| match found {
                Ok(QueryResponse::SettledSessions(page)) if !page.sessions.is_empty() => {
                    store.hold_page(page);
                    store.outside_index_changed(cx);
                }
                Ok(QueryResponse::SettledSessions(_)) => {}
                Ok(other) => log::error!("unexpected find-settled response: {other:?}"),
                // A search that could not be asked finds what is held.
                Err(error) => log::warn!("settled threads not searched: {}", error.message),
            });
        })
    }

    /// Read the family of `session_id` again: the thread nobody started
    /// that it descends from, and what descends from that.
    pub(super) fn load_family(&mut self, session_id: &str, cx: &mut App) {
        if self.family_tasks.contains_key(session_id)
            || !self.baseline_topics.contains(&Topic::Scope)
        {
            return;
        }
        let host = self.host.clone();
        let id = session_id.to_owned();
        let this = self.this.clone();
        self.family_unread.insert(session_id.to_owned());
        self.family_tasks.insert(
            session_id.to_owned(),
            cx.spawn(async move |cx| {
                let result = host
                    .query(Query::ThreadFamily {
                        session_id: id.clone(),
                    })
                    .await;
                let _ = this.update(cx, |store, cx| store.family_read(&id, result, cx));
            }),
        );
    }

    fn family_read(
        &mut self,
        session_id: &str,
        result: Result<QueryResponse, ProtocolError>,
        cx: &mut Context<Self>,
    ) {
        self.family_tasks.remove(session_id);
        match result {
            Ok(QueryResponse::ThreadFamily(page)) => {
                // What is held under the family's lead and the host no
                // longer has there is gone.
                let gone = page.sessions.first().map_or_else(Vec::new, |family| {
                    self.index_replica
                        .0
                        .iter()
                        .filter(|meta| lead(meta, |id| self.thread_meta(id)).id == family.id)
                        .filter(|meta| !page.sessions.iter().any(|held| held.id == meta.id))
                        .map(|meta| meta.id.clone())
                        .collect()
                });
                self.forget_threads(gone);
                self.hold_page(page);
            }
            // The host has no such thread unarchived: a thread held here
            // was deleted or archived, and one not held is a draft's.
            Err(error) if error.code == "thread_not_found" => {
                self.forget_threads(vec![session_id.to_owned()]);
            }
            Ok(other) => {
                log::error!("unexpected thread-family response: {other:?}");
                return;
            }
            // The link fell; the next baseline asks again.
            Err(_) => return,
        }
        self.family_unread.remove(session_id);
        self.outside_index_changed(cx);
    }

    /// Hold `page`'s threads with what the host says of them.
    fn hold_page(&mut self, page: SessionPage) {
        let SessionPage {
            sessions,
            activity,
            worktree_shared,
        } = page;
        for mut meta in sessions {
            apply::title_draft(&mut meta);
            if worktree_shared.contains(&meta.id) {
                self.index_summary.worktree_shared.insert(meta.id.clone());
            } else {
                self.index_summary.worktree_shared.remove(&meta.id);
            }
            match self.index_positions.get(&meta.id) {
                Some(position) => self.index_replica.0[*position] = meta,
                None => {
                    self.index_positions
                        .insert(meta.id.clone(), self.index_replica.0.len());
                    self.index_replica.0.push(meta);
                }
            }
        }
        self.index_summary.activity.extend(activity);
        self.index_replica
            .0
            .sort_by_key(|meta| Reverse(meta.updated_at));
        self.reindex_sessions();
    }

    /// Drop threads the host no longer has where they were held.
    fn forget_threads(&mut self, gone: Vec<String>) {
        if gone.is_empty() {
            return;
        }
        self.index_replica.0.retain(|meta| !gone.contains(&meta.id));
        self.reindex_sessions();
        for id in &gone {
            self.index_summary.activity.remove(id);
            self.index_summary.worktree_shared.remove(id);
            self.native_rewind_prefills.remove(id);
            self.conversation_ui
                .remove(&ConversationDestination::Thread(id.clone()));
            self.open_pull_requests.remove(id);
        }
    }

    fn outside_index_changed(&mut self, cx: &mut Context<Self>) {
        self.thread_list_revision += 1;
        self.close_unshown_pull_requests();
        self.reconcile_destination(cx);
        cx.emit(StoreChange {
            topic: TopicKind::Index,
        });
        cx.notify();
    }

    /// Before a new index replaces the one held: add to `sessions` the
    /// threads held from outside the index, which the new one leaves out as
    /// the old one did, and every open thread it leaves out. The ids of
    /// those open threads are returned: the host has not vouched for them,
    /// so each is asked for by family.
    pub(super) fn keep_outside_index(&self, sessions: &mut Vec<SessionMeta>) -> Vec<String> {
        let listed: HashSet<&str> = sessions.iter().map(|meta| meta.id.as_str()).collect();
        let mut unvouched = Vec::new();
        let kept: Vec<SessionMeta> = self
            .index_replica
            .0
            .iter()
            .filter(|meta| !listed.contains(meta.id.as_str()))
            .filter(|meta| {
                let open = self.threads.get(&meta.id).is_some_and(|thread| thread.open);
                if open {
                    unvouched.push(meta.id.clone());
                }
                open || !self.in_index(meta)
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            sessions.extend(kept);
            sessions.sort_by_key(|meta| Reverse(meta.updated_at));
        }
        unvouched
    }

    /// The host's index was read again: what the shelves hold is read again
    /// when next wanted.
    pub(super) fn settled_shelves_stale(&mut self) {
        for shelf in self.settled_shelves.values_mut() {
            shelf.stale = true;
            shelf.failed = false;
            shelf.task = None;
        }
    }
}
