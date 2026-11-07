//! What Tcode's data directory and each provider account's directories take
//! on disk.
//!
//! A directory is measured as it is: every entry under it, links not
//! followed. On Unix a file counts the blocks the disk allocates for it, once
//! however many hard links name it; APFS clones and sparse files count as the
//! blocks each file reports. On Windows a file counts its length, under each
//! of its names. A directory that is one of the other measured directories
//! counts only there, so an extra account's directory inside the data
//! directory counts under its provider and nothing counts twice.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use agent::{LaunchEnv, ProviderKind};
use tcode_protocol::{
    AccountDiskUsage, DiskUsage, DiskUsagePart, ProviderDiskUsage, TcodeDiskUsage,
};

/// How many of an account's largest entries are named.
const PARTS: usize = 5;

/// One provider account: its profile, and the directories its CLI keeps its
/// state in.
#[derive(Debug, Clone)]
pub struct Account {
    pub provider: ProviderKind,
    pub profile_id: String,
    pub dirs: Vec<PathBuf>,
}

/// The directories `provider`'s CLI launched with `launch_env` keeps its
/// state in; none for an ACP agent, whose CLI Tcode knows no directory of.
pub fn state_dirs(provider: ProviderKind, launch_env: &LaunchEnv) -> Vec<PathBuf> {
    match provider {
        ProviderKind::ClaudeCode => agent::claude::config_dir(launch_env).into_iter().collect(),
        ProviderKind::Codex => agent::codex::home_dir(launch_env).into_iter().collect(),
        ProviderKind::Pi => agent::pi::agent_dir(launch_env).into_iter().collect(),
        ProviderKind::OpenCode => agent::opencode::data_dir(launch_env).into_iter().collect(),
        ProviderKind::Cursor => agent::cursor::state_dirs(launch_env),
        ProviderKind::Grok => agent::grok::home_dir(launch_env).into_iter().collect(),
        ProviderKind::Acp => Vec::new(),
    }
}

/// Measure `data_dir` and `accounts` on a thread of its own at background
/// priority, so a large tree never competes with the user's work. A
/// directory already claimed by the data directory or an earlier account is
/// counted once, under the first. Blocking.
pub fn measure(data_dir: &Path, accounts: &[Account]) -> DiskUsage {
    let run = || {
        lower_priority();
        measure_here(data_dir, accounts)
    };
    std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .name("disk-usage".into())
            .spawn_scoped(scope, run)
        {
            Ok(worker) => worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            Err(error) => {
                log::warn!("could not start a thread to measure disk usage: {error}");
                measure_here(data_dir, accounts)
            }
        }
    })
}

fn measure_here(data_dir: &Path, accounts: &[Account]) -> DiskUsage {
    let started = std::time::Instant::now();
    let home = dirs::home_dir();
    let data_root = fs::canonicalize(data_dir).ok();
    let mut roots: HashSet<PathBuf> = data_root.iter().cloned().collect();
    let accounts: Vec<(&Account, Vec<(PathBuf, &PathBuf)>)> = accounts
        .iter()
        .map(|account| {
            let dirs = account
                .dirs
                .iter()
                .filter_map(|dir| Some((fs::canonicalize(dir).ok()?, dir)))
                .filter(|(root, _)| roots.insert(root.clone()))
                .collect();
            (account, dirs)
        })
        .collect();
    let mut walk = Walk {
        roots: &roots,
        #[cfg(unix)]
        linked: HashSet::new(),
    };

    let mut tcode = TcodeDiskUsage {
        dir: shown(data_dir, home.as_deref()),
        conversations: 0,
        images: 0,
        worktrees: 0,
        other: 0,
    };
    if let Some(root) = &data_root {
        let images = [
            crate::user_files::attachments_root(root),
            crate::image_previews::previews_dir(root),
        ];
        let worktrees = crate::worktree::worktrees_root(root);
        let own = walk.root(root, |name, path, bytes| {
            let part = if crate::store::holds_conversations(name) {
                &mut tcode.conversations
            } else if images.iter().any(|dir| dir == path) {
                &mut tcode.images
            } else if path == worktrees {
                &mut tcode.worktrees
            } else {
                &mut tcode.other
            };
            *part += bytes;
        });
        tcode.other += own;
    }

    let mut providers: Vec<ProviderDiskUsage> = Vec::new();
    for (account, dirs) in accounts {
        let mut entries = 0;
        let mut own = 0;
        let mut parts = Vec::new();
        for (root, _) in &dirs {
            own += walk.root(root, |name, _, bytes| {
                entries += bytes;
                parts.push(DiskUsagePart {
                    name: name.to_string_lossy().into_owned(),
                    bytes,
                });
            });
        }
        // An empty or missing directory is nothing on disk.
        if entries == 0 {
            continue;
        }
        parts.sort_by_key(|part| std::cmp::Reverse(part.bytes));
        parts.retain(|part| part.bytes > 0);
        parts.truncate(PARTS);
        let usage = AccountDiskUsage {
            profile_id: account.profile_id.clone(),
            dirs: dirs
                .iter()
                .map(|(_, dir)| shown(dir, home.as_deref()))
                .collect(),
            bytes: own + entries,
            parts,
        };
        match providers
            .iter_mut()
            .find(|provider| provider.provider == account.provider)
        {
            Some(provider) => {
                provider.bytes += usage.bytes;
                provider.accounts.push(usage);
            }
            None => providers.push(ProviderDiskUsage {
                provider: account.provider,
                bytes: usage.bytes,
                accounts: vec![usage],
            }),
        }
    }
    for provider in &mut providers {
        provider
            .accounts
            .sort_by_key(|account| std::cmp::Reverse(account.bytes));
    }
    providers.sort_by_key(|provider| std::cmp::Reverse(provider.bytes));
    log::info!("measured disk usage in {:?}", started.elapsed());
    DiskUsage {
        measured_at: tcode_core::project::now_secs(),
        tcode,
        providers,
    }
}

struct Walk<'a> {
    /// Every measured directory; one met inside another counts only as
    /// itself.
    roots: &'a HashSet<PathBuf>,
    /// The files with several names already counted.
    #[cfg(unix)]
    linked: HashSet<(u64, u64)>,
}

impl Walk<'_> {
    /// Measure each entry directly inside `root` and hand it to `entry` with
    /// its name and path; returns what `root` itself takes.
    fn root(&mut self, root: &Path, mut entry: impl FnMut(&std::ffi::OsStr, &Path, u64)) -> u64 {
        let Ok(metadata) = fs::symlink_metadata(root) else {
            return 0;
        };
        let own = self.allocated(&metadata);
        if let Ok(entries) = fs::read_dir(root) {
            for child in entries.flatten() {
                let path = child.path();
                if let Some(bytes) = self.tree(&path, child.metadata()) {
                    entry(&child.file_name(), &path, bytes);
                }
            }
        }
        own
    }

    /// What `path` and everything under it take; `None` when it is another
    /// measured directory.
    fn tree(&mut self, path: &Path, metadata: std::io::Result<Metadata>) -> Option<u64> {
        let Ok(metadata) = metadata else {
            return Some(0);
        };
        if metadata.is_dir() && self.roots.contains(path) {
            return None;
        }
        let mut bytes = self.allocated(&metadata);
        if !metadata.is_dir() {
            return Some(bytes);
        }
        let mut pending = vec![path.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for child in entries.flatten() {
                // A directory entry's metadata never follows a link.
                let Ok(metadata) = child.metadata() else {
                    continue;
                };
                if metadata.is_dir() {
                    let path = child.path();
                    if self.roots.contains(&path) {
                        continue;
                    }
                    pending.push(path);
                }
                bytes += self.allocated(&metadata);
            }
        }
        Some(bytes)
    }

    #[cfg(unix)]
    fn allocated(&mut self, metadata: &Metadata) -> u64 {
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_dir()
            && metadata.nlink() > 1
            && !self.linked.insert((metadata.dev(), metadata.ino()))
        {
            return 0;
        }
        // `st_blocks` counts 512-byte units on every Unix.
        metadata.blocks() * 512
    }

    // Stable Rust exposes neither the allocation nor the file id here.
    #[cfg(not(unix))]
    fn allocated(&mut self, metadata: &Metadata) -> u64 {
        metadata.len()
    }
}

/// `path` as the user reads it, `~` standing for `home`.
fn shown(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.to_string_lossy()),
        None => path.to_string_lossy().into_owned(),
    }
}

/// Put the calling thread at background priority for the rest of its life.
fn lower_priority() {
    // SAFETY: each call below only changes the calling thread's own
    // scheduling.
    #[cfg(target_os = "macos")]
    unsafe {
        // Darwin's background band also throttles the thread's disk reads.
        libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG);
    }
    // Linux keeps a nice value per thread, so this leaves the process alone.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unit the fixture's files are sized in, a whole number of blocks
    /// on every disk the tests run on.
    const UNIT: u64 = 64 * 1024;
    /// What the fixture's directories themselves may add to a total.
    const SLACK: u64 = UNIT / 2;

    fn file(path: &Path, units: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![1u8; (units * UNIT) as usize]).unwrap();
    }

    #[track_caller]
    fn about(actual: u64, units: u64) {
        let expected = units * UNIT;
        assert!(
            (expected..=expected + SLACK).contains(&actual),
            "{actual} bytes is not about {units} units"
        );
    }

    fn account(provider: ProviderKind, profile_id: &str, dir: PathBuf) -> Account {
        Account {
            provider,
            profile_id: profile_id.into(),
            dirs: vec![dir],
        }
    }

    /// Each row counts what its directories hold, once: a database
    /// hard-linked under a second name counts once, an extra account's
    /// directory inside the data directory counts under its provider and not
    /// in Tcode's other, two profiles sharing a directory count it once, and
    /// a link out of a worktree counts as a link, not as the tree it points
    /// at. A provider with nothing on disk is left out.
    #[test]
    fn every_row_counts_its_own_directories_once_without_following_links() {
        let temp = std::env::temp_dir().join(format!("disk-usage-{}", uuid::Uuid::new_v4()));
        let data = temp.join("data");
        file(&data.join("tcode.db"), 16);
        file(&data.join("tcode.db-wal"), 1);
        file(&data.join("blobs/sha256/ab/cdef"), 4);
        file(&data.join("attachments/thread/photo.png"), 8);
        file(&data.join("image-previews-v1/preview"), 2);
        file(&data.join("worktrees/thread/src/main.rs"), 3);
        file(&data.join("settings.json"), 1);
        // A directory no profile names any more stays Tcode's.
        file(&data.join("profile-homes/left/history.jsonl"), 5);
        file(&data.join("profile-homes/work/projects/session.jsonl"), 6);
        file(&temp.join("claude/projects/session.jsonl"), 7);
        file(&temp.join("codex/sessions/rollout.jsonl"), 9);
        file(&temp.join("codex/generated_images/image.png"), 2);
        fs::create_dir_all(temp.join("pi")).unwrap();
        #[cfg(unix)]
        {
            fs::hard_link(data.join("tcode.db"), data.join("tcode.db.v1")).unwrap();
            file(&temp.join("cache/build.bin"), 32);
            std::os::unix::fs::symlink(temp.join("cache"), data.join("worktrees/thread/target"))
                .unwrap();
        }
        let accounts = [
            account(ProviderKind::ClaudeCode, "claude", temp.join("claude")),
            account(
                ProviderKind::ClaudeCode,
                "work",
                data.join("profile-homes/work"),
            ),
            account(ProviderKind::Codex, "codex", temp.join("codex")),
            account(ProviderKind::Codex, "shadow", temp.join("codex")),
            account(ProviderKind::Pi, "pi", temp.join("pi")),
            account(ProviderKind::Cursor, "native:cursor", temp.join("cursor")),
        ];

        let usage = measure(&data, &accounts);
        let _ = fs::remove_dir_all(&temp);

        let tcode = &usage.tcode;
        about(tcode.conversations, 16 + 1 + 4);
        about(tcode.images, 8 + 2);
        about(tcode.worktrees, 3);
        about(tcode.other, 1 + 5);
        let rows: Vec<_> = usage
            .providers
            .iter()
            .map(|provider| {
                let accounts: Vec<_> = provider
                    .accounts
                    .iter()
                    .map(|account| account.profile_id.as_str())
                    .collect();
                (provider.provider, accounts)
            })
            .collect();
        assert_eq!(
            rows,
            [
                (ProviderKind::ClaudeCode, vec!["claude", "work"]),
                (ProviderKind::Codex, vec!["codex"]),
            ]
        );
        let claude = &usage.providers[0];
        about(claude.bytes, 7 + 6);
        about(claude.accounts[1].bytes, 6);
        let codex = &usage.providers[1].accounts[0];
        about(codex.bytes, 9 + 2);
        let parts: Vec<_> = codex.parts.iter().map(|part| part.name.as_str()).collect();
        assert_eq!(parts, ["sessions", "generated_images"]);
        about(codex.parts[0].bytes, 9);
    }
}
