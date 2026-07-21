//! Projects and session-index domain data.

use std::path::{Path, PathBuf};

use agent::{ApprovalMode, InteractionMode, OptionSelection, ProviderKind, ResumeCursor};
use serde::{Deserialize, Serialize};

use crate::settings::ProjectSort;

/// A project groups sessions (threads) that share a working-directory root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub created_at: u64,
}

impl Project {
    /// Create a project rooted at `root`, deriving a display name from its
    /// last path component (falling back to the full path).
    pub fn from_root(root: PathBuf) -> Self {
        let name = project_name_from_root(&root);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            root,
            created_at: now_secs(),
        }
    }
}

/// Derive a project display name from a directory path.
pub fn project_name_from_root(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

/// Where a session's worktree lives, when it runs in dedicated-worktree mode
/// (Group C). The session's `cwd` is the worktree path; this records what it was
/// derived from so the worktree can be cleaned up on deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// The main project checkout the worktree was created from (its git root).
    pub root_project_path: PathBuf,
    /// The base branch/ref the worktree was branched from.
    pub base: String,
    /// The branch created for this worktree (`tcode/<session-id>`).
    pub branch: String,
}

/// Index entry describing one persisted session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub provider: ProviderKind,
    /// Which provider *profile* this session runs against. `None` (and absent in
    /// legacy index files) means the built-in profile for `provider`; `Some(id)`
    /// selects a user-created profile (e.g. a third-party Anthropic endpoint).
    /// `provider` stays the protocol discriminant — the profile only changes the
    /// env / binary / home the same protocol is spawned with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Set when the thread is archived (unix secs). Archived threads vanish from
    /// the sidebar and are reversible from Settings → Archived Threads. Absent in
    /// legacy files (defaults to "not archived"). (Group A)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
    /// Dedicated-worktree mode metadata, when the session runs in its own git
    /// worktree instead of the project checkout. Absent = local checkout. (Group C)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    /// The user-facing permission model for this session. Older index files
    /// predate the field; a missing value defaults to `ApprovalMode::default()`
    /// (now `FullAccess`, matching T3).
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    #[serde(default)]
    pub resume_cursor: Option<ResumeCursor>,
    /// The tcode thread whose transcript and provider context this thread
    /// forked from. A fork is a sibling, not an orchestrator child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Whether the next provider start must fork `resume_cursor` rather than
    /// resume it in place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_fork: bool,
    /// Set when this thread was imported from another tool's local history
    /// ("claude:<id>" / "codex:<id>"). Used to keep re-imports idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    /// Chosen values for the selected model's option descriptors (reasoning
    /// effort, context window, service tier, fast mode, thinking, …). Absent in
    /// index files written before this slice; defaults to no selections (each
    /// descriptor then resolves to its own default).
    #[serde(default)]
    pub option_selections: Vec<OptionSelection>,
    /// Build (default) vs Plan interaction mode. Absent in legacy files;
    /// defaults to `Build`.
    #[serde(default)]
    pub interaction_mode: InteractionMode,
    /// Which ACP agent this session runs (its registry id), when
    /// `provider == ProviderKind::Acp`. `None` for the native providers, and
    /// absent in every index file written before ACP existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_agent_id: Option<String>,
    /// Parent orchestrator thread for native child sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Whether this session receives the tcode_orchestrate MCP registration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orchestrate_enabled: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl SessionMeta {
    pub fn new(provider: ProviderKind, cwd: PathBuf, model: Option<String>) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: format!("New {} session", provider.display_name()),
            provider,
            profile_id: None,
            cwd,
            project_id: None,
            model,
            archived_at: None,
            worktree: None,
            approval_mode: ApprovalMode::default(),
            resume_cursor: None,
            forked_from: None,
            pending_fork: false,
            imported_from: None,
            option_selections: Vec::new(),
            interaction_mode: InteractionMode::default(),
            acp_agent_id: None,
            parent_session_id: None,
            orchestrate_enabled: false,
            created_at: now,
            updated_at: now,
        }
    }
}

/// A project and its sessions, ready for the sidebar (newest activity first).
#[derive(Debug, Clone)]
pub struct ProjectGroup {
    pub project: Project,
    pub sessions: Vec<SessionMeta>,
}

/// Group `sessions` under their `projects`, ordering sessions newest-activity
/// first within each group and groups per `sort`.
pub fn group_sessions(
    projects: &[Project],
    sessions: &[SessionMeta],
    sort: ProjectSort,
) -> Vec<ProjectGroup> {
    let mut groups: Vec<ProjectGroup> = projects
        .iter()
        .map(|project| {
            let mut sessions: Vec<SessionMeta> = sessions
                .iter()
                .filter(|s| s.project_id.as_deref() == Some(project.id.as_str()))
                .cloned()
                .collect();
            sessions = order_sessions_with_children(sessions);
            ProjectGroup {
                project: project.clone(),
                sessions,
            }
        })
        .collect();

    match sort {
        // Groups ordered by newest activity (falling back to project creation).
        ProjectSort::RecentActivity => groups.sort_by(|a, b| {
            let activity = |g: &ProjectGroup| {
                g.sessions
                    .iter()
                    .map(|s| s.updated_at)
                    .max()
                    .unwrap_or(g.project.created_at)
            };
            activity(b).cmp(&activity(a))
        }),
        // Groups ordered by project name, case-insensitive A-Z.
        ProjectSort::NameAsc => {
            groups.sort_by(|a, b| {
                a.project
                    .name
                    .to_lowercase()
                    .cmp(&b.project.name.to_lowercase())
            });
        }
    }
    groups
}

/// Stable parent-first ordering for one project. Orphans are roots; each
/// parent's newest children follow it immediately.
fn order_sessions_with_children(sessions: Vec<SessionMeta>) -> Vec<SessionMeta> {
    let ids: std::collections::HashSet<&str> =
        sessions.iter().map(|session| session.id.as_str()).collect();
    let mut roots: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|session| {
            session
                .parent_session_id
                .as_deref()
                .is_none_or(|parent| !ids.contains(parent))
        })
        .collect();
    roots.sort_by_key(|session| std::cmp::Reverse(session.updated_at));

    fn append(
        parent: &SessionMeta,
        sessions: &[SessionMeta],
        output: &mut Vec<SessionMeta>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        if !visited.insert(parent.id.clone()) {
            return;
        }
        output.push(parent.clone());
        let mut children: Vec<&SessionMeta> = sessions
            .iter()
            .filter(|session| session.parent_session_id.as_deref() == Some(parent.id.as_str()))
            .collect();
        children.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        for child in children {
            append(child, sessions, output, visited);
        }
    }

    let mut output = Vec::with_capacity(sessions.len());
    let mut visited = std::collections::HashSet::new();
    for root in roots {
        append(root, &sessions, &mut output, &mut visited);
    }
    // Defensive cycle handling: malformed cyclic metadata stays visible.
    let mut remainder: Vec<&SessionMeta> = sessions
        .iter()
        .filter(|session| !visited.contains(&session.id))
        .collect();
    remainder.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    for session in remainder {
        append(session, &sessions, &mut output, &mut visited);
    }
    output
}

/// On-disk shape of `sessions.json` (current schema). Old files were a bare
/// `Vec<SessionMeta>`; the compatibility persistence layer tolerates both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexFile {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub sessions: Vec<SessionMeta>,
}

/// Ensure every session belongs to a project, deriving implicit projects from
/// each orphan session's cwd (deduped by root). Idempotent.
pub fn migrate_index(mut file: IndexFile) -> IndexFile {
    // Map existing project roots to their ids so derived projects dedupe.
    let mut root_to_id: std::collections::HashMap<PathBuf, String> = file
        .projects
        .iter()
        .map(|p| (p.root.clone(), p.id.clone()))
        .collect();

    for session in &mut file.sessions {
        if session
            .project_id
            .as_ref()
            .is_some_and(|id| file.projects.iter().any(|p| &p.id == id))
        {
            continue;
        }
        let root = session.cwd.clone();
        let project_id = if let Some(id) = root_to_id.get(&root) {
            id.clone()
        } else {
            let project = Project::from_root(root.clone());
            let id = project.id.clone();
            root_to_id.insert(root, id.clone());
            file.projects.push(project);
            id
        };
        session.project_id = Some(project_id);
    }
    file
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_in(project_id: &str, updated_at: u64) -> SessionMeta {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/x"), None);
        meta.project_id = Some(project_id.to_string());
        meta.updated_at = updated_at;
        meta
    }

    #[test]
    fn group_sessions_orders_by_activity() {
        let projects = vec![
            Project {
                id: "p-old".into(),
                name: "Old".into(),
                root: PathBuf::from("/old"),
                created_at: 1,
            },
            Project {
                id: "p-new".into(),
                name: "New".into(),
                root: PathBuf::from("/new"),
                created_at: 2,
            },
            Project {
                id: "p-empty".into(),
                name: "Empty".into(),
                root: PathBuf::from("/empty"),
                created_at: 15,
            },
        ];
        let sessions = vec![
            session_in("p-old", 10),
            session_in("p-new", 100),
            session_in("p-new", 50),
            session_in("p-old", 20),
        ];

        let groups = group_sessions(&projects, &sessions, ProjectSort::RecentActivity);
        // p-new (activity 100), p-old (activity 20), p-empty (created_at 15, no sessions).
        assert_eq!(groups[0].project.id, "p-new");
        assert_eq!(groups[1].project.id, "p-old");
        assert_eq!(groups[2].project.id, "p-empty");
        // Within a group, newest session first.
        assert_eq!(groups[0].sessions[0].updated_at, 100);
        assert_eq!(groups[0].sessions[1].updated_at, 50);
        assert!(groups[2].sessions.is_empty());

        // Name A-Z ordering ignores activity: Empty, New, Old (case-insensitive).
        let by_name = group_sessions(&projects, &sessions, ProjectSort::NameAsc);
        assert_eq!(by_name[0].project.name, "Empty");
        assert_eq!(by_name[1].project.name, "New");
        assert_eq!(by_name[2].project.name, "Old");
    }

    #[test]
    fn group_sessions_places_children_after_their_parent() {
        let projects = vec![Project {
            id: "p".into(),
            name: "Project".into(),
            root: PathBuf::from("/p"),
            created_at: 1,
        }];
        let make = |id: &str, updated_at: u64, parent: Option<&str>| {
            let mut meta = session_in("p", updated_at);
            meta.id = id.into();
            meta.parent_session_id = parent.map(str::to_string);
            meta
        };
        let sessions = vec![
            make("child-old", 10, Some("parent-new")),
            make("parent-old", 90, None),
            make("orphan", 95, Some("deleted-parent")),
            make("child-new", 500, Some("parent-new")),
            make("parent-new", 100, None),
        ];

        let groups = group_sessions(&projects, &sessions, ProjectSort::RecentActivity);
        let ids: Vec<_> = groups[0]
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "parent-new",
                "child-new",
                "child-old",
                "orphan",
                "parent-old"
            ]
        );
    }

    #[test]
    fn fork_metadata_is_legacy_safe_and_roundtrips() {
        let legacy = serde_json::json!({
            "id": "legacy", "title": "Legacy", "provider": "codex",
            "cwd": "/work", "created_at": 1, "updated_at": 2
        });
        let meta: SessionMeta = serde_json::from_value(legacy).unwrap();
        assert_eq!(meta.forked_from, None);
        assert!(!meta.pending_fork);

        let mut meta = meta;
        meta.forked_from = Some("source".into());
        meta.pending_fork = true;
        let json = serde_json::to_string(&meta).unwrap();
        let roundtrip: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.forked_from.as_deref(), Some("source"));
        assert!(roundtrip.pending_fork);
    }
}
