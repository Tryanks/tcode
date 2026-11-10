use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    ExternalHistory, ExternalRoots, ExternalThread, RecentDir, SourceTool, collect_files,
    file_modified_ms, timestamp_ms,
};

#[derive(Debug)]
struct DesktopMeta {
    title: Option<String>,
    last_active_ms: Option<u64>,
}

/// The external threads under `roots`, grouped by directory, and every path
/// the scan could not read.
pub fn scan_recent_dirs(roots: &ExternalRoots, exclude_roots: &[PathBuf]) -> ExternalHistory {
    let mut unreadable = Vec::new();
    let desktop = roots
        .claude_desktop_meta
        .as_deref()
        .map(|root| desktop_metadata(root, &mut unreadable))
        .unwrap_or_default();
    let mut found = Vec::<(PathBuf, ExternalThread)>::new();

    let claude_files = roots
        .claude_projects
        .as_deref()
        .map(|root| claude_session_files(root, &mut unreadable))
        .unwrap_or_default();
    for path in claude_files {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                unreadable.push(unreadable_path(&path, &error));
                continue;
            }
        };
        let mut head = None;
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    unreadable.push(unreadable_path(&path, &error));
                    break;
                }
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Subagent/workflow transcripts share the session's cwd but are not
            // conversations of their own.
            if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                head = None;
                break;
            }
            let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
                continue;
            };
            let id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .or_else(|| path.file_stem().and_then(|name| name.to_str()))
                .unwrap_or_default()
                .to_string();
            let entrypoint = value
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(str::to_string);
            head = Some((
                PathBuf::from(cwd),
                id,
                entrypoint,
                value.get("timestamp").and_then(timestamp_ms),
            ));
            break;
        }
        let Some((cwd, id, entrypoint, head_ts)) = head else {
            continue;
        };
        // `entrypoint` is the CLAUDE_CODE_ENTRYPOINT the session ran under:
        // T3 Code sets "t3-code"; the Agent SDKs set "sdk-ts", "sdk-py" and
        // "sdk-cli" for any application built on them.
        let joined = desktop.get(&id);
        let source = if joined.is_some() {
            SourceTool::ClaudeDesktop
        } else {
            match entrypoint.as_deref() {
                Some("t3-code") => SourceTool::T3Code,
                Some("claude-desktop") => SourceTool::ClaudeDesktop,
                Some("sdk-ts" | "sdk-py" | "sdk-cli") => SourceTool::ClaudeAgentSdk,
                _ => SourceTool::ClaudeCode,
            }
        };
        found.push((
            cwd,
            ExternalThread {
                source,
                file: path.clone(),
                external_id: format!("claude:{id}"),
                title_hint: joined.and_then(|meta| meta.title.clone()),
                last_active_ms: joined
                    .and_then(|meta| meta.last_active_ms)
                    .unwrap_or_else(|| file_modified_ms(&path).max(head_ts.unwrap_or(0))),
            },
        ));
    }

    for root in &roots.codex_session_roots {
        let files = collect_files(
            root,
            &|path| path.extension().is_some_and(|ext| ext == "jsonl"),
            &mut unreadable,
        );
        for path in files {
            let line = match File::open(&path)
                .and_then(|file| BufReader::new(file).lines().next().transpose())
            {
                Ok(Some(line)) => line,
                Ok(None) => continue,
                Err(error) => {
                    unreadable.push(unreadable_path(&path, &error));
                    continue;
                }
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("session_meta") {
                continue;
            }
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let Some(id) = payload.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
                continue;
            };
            let originator = payload.get("originator").and_then(Value::as_str);
            if originator == Some("tcode") {
                continue;
            }
            found.push((
                PathBuf::from(cwd),
                ExternalThread {
                    source: if originator == Some("Codex Desktop") {
                        SourceTool::CodexDesktop
                    } else {
                        SourceTool::CodexCli
                    },
                    file: path.clone(),
                    external_id: format!("codex:{id}"),
                    title_hint: None,
                    last_active_ms: file_modified_ms(&path)
                        .max(value.get("timestamp").and_then(timestamp_ms).unwrap_or(0)),
                },
            ));
        }
    }

    let mut groups = HashMap::<PathBuf, Vec<ExternalThread>>::new();
    for (cwd, thread) in found {
        groups.entry(cwd).or_default().push(thread);
    }
    // Filesystem checks run once per directory, not per thread: each one can
    // block on a slow or unreachable mount.
    let excluded: Vec<_> = exclude_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect();
    let mut dirs: Vec<_> = groups
        .into_iter()
        .filter(|(cwd, _)| {
            cwd.is_dir() && {
                let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
                !excluded.contains(&cwd)
            }
        })
        .map(|(path, mut threads)| {
            threads.sort_by_key(|thread| std::cmp::Reverse(thread.last_active_ms));
            RecentDir {
                last_active_ms: threads.first().map_or(0, |thread| thread.last_active_ms),
                path,
                threads,
            }
        })
        .collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.last_active_ms));
    ExternalHistory { dirs, unreadable }
}

pub(super) fn unreadable_path(path: &Path, error: &std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

/// Session transcripts live directly under `projects/<slug>/` — one `.jsonl`
/// per session. Deeper trees (`<session-id>/subagents/**`, `workflows/**`) hold
/// sidechain material, so recursing would surface non-threads.
fn claude_session_files(projects_root: &Path, unreadable: &mut Vec<String>) -> Vec<PathBuf> {
    let slugs = match std::fs::read_dir(projects_root) {
        Ok(slugs) => slugs,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            unreadable.push(unreadable_path(projects_root, &error));
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for slug in slugs.flatten() {
        let dir = slug.path();
        if !dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                unreadable.push(unreadable_path(&dir, &error));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

fn desktop_metadata(root: &Path, unreadable: &mut Vec<String>) -> HashMap<String, DesktopMeta> {
    let mut metadata = HashMap::new();
    let files = collect_files(
        root,
        &|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
        },
        unreadable,
    );
    for path in files {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                unreadable.push(unreadable_path(&path, &error));
                continue;
            }
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(id) = value.get("cliSessionId").and_then(Value::as_str) else {
            continue;
        };
        metadata.insert(
            id.to_string(),
            DesktopMeta {
                title: value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                last_active_ms: value.get("lastActivityAt").and_then(Value::as_u64),
            },
        );
    }
    metadata
}
