//! A project and its threads moving to another machine of the group, on disk.
//!
//! The machine a project leaves builds its parts in `<data>/moves/out/<move>`
//! ([`prepare`]): a manifest, the code (one Git bundle, or the directory as a
//! tar), the files `.worktreeinclude` selects when they go along, and the
//! threads (their metas, event logs in canonical stored form with their
//! images inline, attachments and write journals) as a tar. The machine it
//! goes to stages them in `<data>/moves/in/<move>` ([`stage_begin`],
//! [`stage_chunk`]) and makes the project and its threads from them in one
//! step ([`arrive`]).
pub mod git;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tcode_core::project::{Project, SessionMeta, rebase_path};
use tcode_protocol::MovePart;

use crate::store::{Mutation, SessionStore};
use crate::user_files::{attachment_dir, write_journal_dir};

pub const MANIFEST: &str = "manifest.json";
pub const CODE_BUNDLE: &str = "code.bundle";
pub const CODE_TAR: &str = "code.tar";
pub const IGNORED_TAR: &str = "ignored.tar";
pub const THREADS_TAR: &str = "threads.tar";
/// What one `MoveChunk` carries at most: as base64 in one line, well inside
/// [`tcode_protocol::MAX_LINE_BYTES`].
pub const CHUNK_BYTES: usize = 1 << 20;

/// Which parts belong to the code, for the phase a move reports while it
/// sends them; the rest are the threads.
pub fn is_code_part(name: &str) -> bool {
    name != THREADS_TAR
}

/// What a move says of the project, read by the machine it goes to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub project_name: String,
    /// The project's root on the machine it leaves.
    pub source_root: PathBuf,
    pub git: Option<GitManifest>,
    pub threads: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitManifest {
    /// Where the repository's remotes are and which branch each local one
    /// follows ([`git::carried_config`]).
    pub config: Vec<(String, String)>,
    /// The root checkout first.
    pub checkouts: Vec<Checkout>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkout {
    /// Its name inside the move: its snapshot refs' and its ignored files'.
    pub label: String,
    pub kind: CheckoutKind,
    pub source_path: PathBuf,
    pub snapshot: git::CheckoutSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckoutKind {
    Root,
    /// A Tcode worktree of `thread_id`'s, called `name` where it is.
    Worktree {
        thread_id: String,
        name: String,
    },
}

/// A name another machine gives for a file or directory here: one plain path
/// component, never one that walks out of where it is put.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name != "."
        && name != ".."
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn moves_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("moves")
}

/// Where the machine a project leaves keeps the parts of move `move_id`.
pub fn outgoing_dir(data_dir: &Path, move_id: &str) -> PathBuf {
    moves_dir(data_dir).join("out").join(move_id)
}

/// Where the machine a project goes to stages move `move_id`.
pub fn staging_dir(data_dir: &Path, move_id: &str) -> PathBuf {
    moves_dir(data_dir).join("in").join(move_id)
}

const PARTS_FILE: &str = "parts.json";
const BEGIN_FILE: &str = "begin.json";

fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let hash = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((size, hash))
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(value).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// What the machine a project leaves sends of it.
pub struct Outgoing<'a> {
    pub move_id: &'a str,
    pub project_name: &'a str,
    pub root: &'a Path,
    pub include_ignored: bool,
    /// Every thread that goes, settled and archived ones included.
    pub threads: &'a [SessionMeta],
}

/// The checkouts a move snapshots: the root, then each Tcode worktree of a
/// thread that is a checkout of the root's repository, once however many
/// threads work in it.
pub fn checkouts(root: &Path, threads: &[SessionMeta]) -> Vec<(CheckoutKind, PathBuf)> {
    let mut checkouts = vec![(CheckoutKind::Root, root.to_path_buf())];
    let common = |dir: &Path| {
        crate::git::run_git(dir, &["rev-parse", "--git-common-dir"])
            .ok()
            .and_then(|common| dir.join(common.trim()).canonicalize().ok())
    };
    let Some(repository) = common(root) else {
        return checkouts;
    };
    let mut seen = HashSet::new();
    for meta in threads {
        let owned = meta
            .worktree
            .as_ref()
            .map(|_| meta.cwd.clone())
            .into_iter()
            .chain(meta.other_worktrees.iter().map(|owned| owned.path.clone()));
        for path in owned {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !seen.insert(path.clone())
                || !git::is_checkout_root(&path)
                || common(&path).as_ref() != Some(&repository)
            {
                continue;
            }
            checkouts.push((
                CheckoutKind::Worktree {
                    thread_id: meta.id.clone(),
                    name,
                },
                path,
            ));
        }
    }
    checkouts
}

/// Whether `root` goes as a Git repository: it is the top level of one.
pub fn is_git_project(root: &Path) -> bool {
    git::is_checkout_root(root)
}

/// Whether `root` lies inside a Git work tree without being its top level:
/// its history and its threads' worktrees belong to a repository larger than
/// the project, which a move does not take.
pub fn inside_larger_repository(root: &Path) -> Result<bool, String> {
    Ok(crate::git::is_git_repo(root)? && !git::is_checkout_root(root))
}

/// The files `.worktreeinclude` selects in each checkout, by checkout.
pub fn ignored_files(
    checkouts: &[(CheckoutKind, PathBuf)],
) -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    checkouts
        .iter()
        .map(|(_, path)| {
            crate::worktree::worktree_include_files(path)
                .map(|files| (path.clone(), files))
                .map_err(|error| error.to_string())
        })
        .collect()
}

/// The parts of a move already built in `out`, when every one of them is
/// still what was recorded.
pub fn built_parts(out: &Path) -> Option<Vec<MovePart>> {
    let parts: Vec<MovePart> = read_json(&out.join(PARTS_FILE)).ok()?;
    parts
        .iter()
        .all(|part| {
            sha256_file(&out.join(&part.name))
                .is_ok_and(|(size, sha)| size == part.size && sha == part.sha256)
        })
        .then_some(parts)
}

/// Build every part of a move in `out`, off the host. The snapshot refs it
/// leaves in the repository are removed by [`end_outgoing`].
pub fn prepare(
    store: &SessionStore,
    out: &Path,
    outgoing: &Outgoing<'_>,
) -> Result<Vec<MovePart>, String> {
    let result = prepare_parts(store, out, outgoing);
    if result.is_err() && is_git_project(outgoing.root) {
        let _ = git::remove_refs(outgoing.root, &refs_prefix(outgoing.move_id));
    }
    result
}

fn refs_prefix(move_id: &str) -> String {
    format!("{}/{move_id}", git::REFS_ROOT)
}

fn prepare_parts(
    store: &SessionStore,
    out: &Path,
    outgoing: &Outgoing<'_>,
) -> Result<Vec<MovePart>, String> {
    let error = |error: io::Error| error.to_string();
    remove_dir(out).map_err(error)?;
    let scratch = out.join("scratch");
    fs::create_dir_all(&scratch).map_err(error)?;
    let mut names = vec![MANIFEST.to_owned()];
    let git = if is_git_project(outgoing.root) {
        let found = checkouts(outgoing.root, outgoing.threads);
        let mut checkouts = Vec::new();
        for (index, (kind, path)) in found.iter().enumerate() {
            let label = match kind {
                CheckoutKind::Root => "root".to_owned(),
                CheckoutKind::Worktree { .. } => format!("worktree-{index}"),
            };
            let snapshot = git::snapshot(
                path,
                &format!("{}/{label}", refs_prefix(outgoing.move_id)),
                &scratch,
            )?;
            checkouts.push(Checkout {
                label,
                kind: kind.clone(),
                source_path: path.clone(),
                snapshot,
            });
        }
        let config = git::carried_config(outgoing.root)?;
        git::bundle(outgoing.root, &out.join(CODE_BUNDLE))?;
        names.push(CODE_BUNDLE.to_owned());
        if outgoing.include_ignored {
            let ignored = ignored_files(&found)?;
            if ignored.iter().any(|(_, files)| !files.is_empty()) {
                let mut tar = tar::Builder::new(BufWriter::new(
                    File::create(out.join(IGNORED_TAR)).map_err(error)?,
                ));
                tar.follow_symlinks(false);
                for ((path, files), checkout) in ignored.iter().zip(&checkouts) {
                    for file in files {
                        let source = path.join(file);
                        // A link is not copied, as `.worktreeinclude` seeding does not.
                        if fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_file()) {
                            tar.append_path_with_name(
                                &source,
                                format!("{}/{file}", checkout.label),
                            )
                            .map_err(error)?;
                        }
                    }
                }
                tar.into_inner().map_err(error)?.flush().map_err(error)?;
                names.push(IGNORED_TAR.to_owned());
            }
        }
        Some(GitManifest { config, checkouts })
    } else {
        let mut tar = tar::Builder::new(BufWriter::new(
            File::create(out.join(CODE_TAR)).map_err(error)?,
        ));
        tar.follow_symlinks(false);
        tar.append_dir_all(".", outgoing.root).map_err(error)?;
        tar.into_inner().map_err(error)?.flush().map_err(error)?;
        names.push(CODE_TAR.to_owned());
        None
    };
    write_threads(store, &out.join(THREADS_TAR), &scratch, outgoing.threads).map_err(error)?;
    names.push(THREADS_TAR.to_owned());
    write_json(
        &out.join(MANIFEST),
        &Manifest {
            project_name: outgoing.project_name.to_owned(),
            source_root: outgoing.root.to_path_buf(),
            git,
            threads: outgoing
                .threads
                .iter()
                .map(|meta| meta.id.clone())
                .collect(),
        },
    )
    .map_err(error)?;
    remove_dir(&scratch).map_err(error)?;
    let parts = names
        .into_iter()
        .map(|name| {
            let (size, sha256) = sha256_file(&out.join(&name))?;
            Ok(MovePart { name, size, sha256 })
        })
        .collect::<io::Result<Vec<_>>>()
        .map_err(error)?;
    write_json(&out.join(PARTS_FILE), &parts).map_err(error)?;
    Ok(parts)
}

/// Every thread as `<id>/meta.json`, `<id>/events.jsonl`, and its
/// `attachments/` and `write-journal/` where it has them.
fn write_threads(
    store: &SessionStore,
    out: &Path,
    scratch: &Path,
    threads: &[SessionMeta],
) -> io::Result<()> {
    let mut tar = tar::Builder::new(BufWriter::new(File::create(out)?));
    tar.follow_symlinks(false);
    for meta in threads {
        let id = &meta.id;
        if !safe_name(id) {
            return Err(invalid(format!("thread id {id:?} cannot name a file")));
        }
        let meta_path = scratch.join("meta.json");
        fs::write(
            &meta_path,
            serde_json::to_vec(meta).map_err(io::Error::other)?,
        )?;
        tar.append_path_with_name(&meta_path, format!("{id}/meta.json"))?;
        let events = scratch.join("events.jsonl");
        {
            let mut file = BufWriter::new(File::create(&events)?);
            store.export_event_log(id, &mut file)?;
            file.flush()?;
        }
        tar.append_path_with_name(&events, format!("{id}/events.jsonl"))?;
        for (dir, name) in [
            (attachment_dir(store.root(), id), "attachments"),
            (write_journal_dir(store.root(), id), "write-journal"),
        ] {
            if dir.is_dir() {
                tar.append_dir_all(format!("{id}/{name}"), &dir)?;
            }
        }
    }
    tar.into_inner()?.flush()
}

/// Remove what a move left on the machine it leaves, whichever way it ended:
/// its parts and the snapshot refs in the project's repository.
pub fn end_outgoing(data_dir: &Path, move_id: &str, root: &Path) -> Result<(), String> {
    let refs = if is_git_project(root) {
        git::remove_refs(root, &refs_prefix(move_id))
    } else {
        Ok(())
    };
    remove_dir(&outgoing_dir(data_dir, move_id)).map_err(|error| error.to_string())?;
    refs
}

/// What `MoveBegin` asked the machine a project goes to to stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub project_name: String,
    pub root: String,
    pub git: bool,
    pub thread_ids: Vec<String>,
    pub parts: Vec<MovePart>,
}

/// Stage move `move_id` as `staged` says, keeping each part already held
/// whose name, size and SHA-256 are unchanged and starting the others over;
/// answers the bytes held of each.
pub fn stage_begin(
    data_dir: &Path,
    move_id: &str,
    staged: &Staged,
) -> io::Result<Vec<(String, u64)>> {
    if !safe_name(move_id) || staged.parts.iter().any(|part| !safe_name(&part.name)) {
        return Err(invalid("a move or part name cannot name a file"));
    }
    let dir = staging_dir(data_dir, move_id);
    fs::create_dir_all(&dir)?;
    let before: Option<Staged> = read_json(&dir.join(BEGIN_FILE)).ok();
    let mut held = Vec::new();
    for part in &staged.parts {
        let path = dir.join(&part.name);
        let same = before
            .as_ref()
            .is_some_and(|before| before.parts.contains(part));
        let length = fs::metadata(&path).map_or(0, |meta| meta.len());
        if !same || length > part.size {
            File::create(&path)?;
            held.push((part.name.clone(), 0));
        } else {
            held.push((part.name.clone(), length));
        }
    }
    if let Some(before) = &before {
        for part in &before.parts {
            if !staged.parts.iter().any(|kept| kept.name == part.name) {
                let _ = fs::remove_file(dir.join(&part.name));
            }
        }
    }
    write_json(&dir.join(BEGIN_FILE), staged)?;
    Ok(held)
}

/// What move `move_id` staged, if it is staging here; what is staged and
/// cannot be read is an error.
pub fn staged(data_dir: &Path, move_id: &str) -> io::Result<Option<Staged>> {
    if !safe_name(move_id) {
        return Ok(None);
    }
    match read_json(&staging_dir(data_dir, move_id).join(BEGIN_FILE)) {
        Ok(staged) => Ok(Some(staged)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Append `bytes` to the staged `part` when they start at the bytes it holds,
/// answering what it holds afterwards; bytes from anywhere else are not taken,
/// and what it holds is the answer.
pub fn stage_chunk(
    data_dir: &Path,
    move_id: &str,
    part: &str,
    offset: u64,
    bytes: &[u8],
) -> io::Result<u64> {
    let staged = staged(data_dir, move_id)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("move {move_id} is not staging here"),
        )
    })?;
    let expected = staged
        .parts
        .iter()
        .find(|staged| staged.name == part)
        .ok_or_else(|| invalid(format!("move {move_id} has no part {part}")))?;
    let path = staging_dir(data_dir, move_id).join(part);
    let length = fs::metadata(&path)?.len();
    if length != offset {
        return Ok(length);
    }
    if offset + bytes.len() as u64 > expected.size {
        return Err(invalid(format!("part {part} is {} bytes", expected.size)));
    }
    crate::user_files::append_upload(&path, offset, bytes)
}

/// Forget move `move_id`'s staging and everything it held.
pub fn discard_staging(data_dir: &Path, move_id: &str) -> io::Result<()> {
    if !safe_name(move_id) {
        return Err(invalid("a move name cannot name a file"));
    }
    remove_dir(&staging_dir(data_dir, move_id))
}

/// What the project and its threads are once they arrive.
pub struct Arrival<'a> {
    pub move_id: &'a str,
    /// The project's root here: absent or an empty directory.
    pub root: &'a Path,
    pub project: Project,
    pub from_machine: &'a str,
    pub from_machine_name: &'a str,
    pub to_machine: &'a str,
    /// The profile a thread arriving with `meta` takes here.
    pub profile: &'a (dyn Fn(&SessionMeta) -> Option<String> + Sync),
    pub now_ms: u64,
}

/// Paths of the machine a project leaves and where they are here, the most
/// specific first.
struct PathMap(Vec<(PathBuf, PathBuf)>);

impl PathMap {
    fn map(&self, path: &Path) -> Option<PathBuf> {
        self.0
            .iter()
            .find_map(|(from, to)| rebase_path(path, from, to))
    }
}

/// What was made on disk for an arrival, taken back if it does not commit.
#[derive(Default)]
struct Made {
    root: Option<(PathBuf, bool)>,
    dirs: Vec<PathBuf>,
}

impl Made {
    /// Take back what was made; what could not be, each as a sentence.
    fn undo(self) -> Vec<String> {
        let mut left = Vec::new();
        let mut remove = |result: io::Result<()>, path: &Path| {
            if let Err(error) = result
                && error.kind() != io::ErrorKind::NotFound
            {
                left.push(format!("{}: {error}", path.display()));
            }
        };
        for dir in &self.dirs {
            remove(fs::remove_dir_all(dir), dir);
        }
        if let Some((root, created)) = &self.root {
            if *created {
                remove(fs::remove_dir_all(root), root);
            } else {
                match fs::read_dir(root) {
                    Ok(entries) => {
                        for entry in entries {
                            let result = entry.and_then(|entry| {
                                let path = entry.path();
                                if entry.file_type()?.is_dir() {
                                    fs::remove_dir_all(path)
                                } else {
                                    fs::remove_file(path)
                                }
                            });
                            remove(result, root);
                        }
                    }
                    Err(error) => remove(Err(error), root),
                }
            }
        }
        left
    }
}

/// Make the project staged by `arrival.move_id` and all its threads at once:
/// each part checked against its size and SHA-256, the code put at the
/// root, each thread worktree made where this machine keeps that thread's,
/// then one store transaction creating the project, every thread with its
/// events and the move's record. Until that commits nothing is visible, and a
/// failure takes back what was made on disk and keeps the staging. Answers
/// the threads as they are here.
pub fn arrive(store: &SessionStore, arrival: &Arrival<'_>) -> Result<Vec<SessionMeta>, String> {
    let data_dir = store.root().clone();
    let dir = staging_dir(&data_dir, arrival.move_id);
    let staged = staged(&data_dir, arrival.move_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("move {} is not staging here", arrival.move_id))?;
    for part in &staged.parts {
        let (size, sha) = sha256_file(&dir.join(&part.name)).map_err(|error| error.to_string())?;
        if size != part.size || sha != part.sha256 {
            return Err(format!(
                "part {} holds {size} of {} bytes or differs from what was sent",
                part.name, part.size
            ));
        }
    }
    let manifest: Manifest = read_json(&dir.join(MANIFEST)).map_err(|error| error.to_string())?;
    let mut made = Made::default();
    match place(store, arrival, &staged, &manifest, &dir, &mut made) {
        Ok(metas) => {
            if let Err(error) = discard_staging(&data_dir, arrival.move_id) {
                log::warn!(
                    "could not remove the staging of move {}: {error}",
                    arrival.move_id
                );
            }
            Ok(metas)
        }
        Err(error) => {
            let mut left = made.undo();
            if let Err(cleanup) = remove_dir(&dir.join("threads")) {
                left.push(format!("{}: {cleanup}", dir.join("threads").display()));
            }
            Err(if left.is_empty() {
                error
            } else {
                format!("{error}; and could not take back {}", left.join(", "))
            })
        }
    }
}

fn place(
    store: &SessionStore,
    arrival: &Arrival<'_>,
    staged: &Staged,
    manifest: &Manifest,
    dir: &Path,
    made: &mut Made,
) -> Result<Vec<SessionMeta>, String> {
    let error = |error: io::Error| error.to_string();
    let data_dir = store.root().clone();
    let root = arrival.root;
    let existed = root.exists();
    if existed && fs::read_dir(root).map_err(error)?.next().is_some() {
        return Err(format!("{} is not empty", root.display()));
    }
    fs::create_dir_all(root).map_err(error)?;
    made.root = Some((root.to_path_buf(), !existed));
    let mut paths = Vec::new();
    match (&manifest.git, staged.git) {
        (Some(git_manifest), true) => {
            let Some(root_checkout) = git_manifest.checkouts.first() else {
                return Err("the move names no root checkout".into());
            };
            git::restore_root(
                &dir.join(CODE_BUNDLE),
                root,
                &root_checkout.snapshot,
                &git_manifest.config,
            )?;
            let mut placed = vec![(root_checkout.label.clone(), root.to_path_buf())];
            for checkout in &git_manifest.checkouts[1..] {
                let CheckoutKind::Worktree { name, .. } = &checkout.kind else {
                    continue;
                };
                let path = crate::worktree::worktree_path(root, name, &data_dir)
                    .map_err(|error| error.to_string())?;
                if path.exists() {
                    return Err(format!("{} is already there", path.display()));
                }
                made.dirs.push(path.clone());
                git::restore_worktree(root, &path, &checkout.snapshot)?;
                paths.push((checkout.source_path.clone(), path.clone()));
                placed.push((checkout.label.clone(), path));
            }
            let ignored = dir.join(IGNORED_TAR);
            if staged.parts.iter().any(|part| part.name == IGNORED_TAR) {
                let mut archive =
                    tar::Archive::new(BufReader::new(File::open(&ignored).map_err(error)?));
                for entry in archive.entries().map_err(error)? {
                    let mut entry = entry.map_err(error)?;
                    let name = entry.path().map_err(error)?.into_owned();
                    let mut components = name.components();
                    let label = components
                        .next()
                        .map(|label| label.as_os_str().to_string_lossy().into_owned());
                    let rest = components.as_path().to_path_buf();
                    let Some((_, checkout)) = placed
                        .iter()
                        .find(|(placed, _)| Some(placed) == label.as_ref())
                    else {
                        continue;
                    };
                    if !entry.header().entry_type().is_file() {
                        continue;
                    }
                    if rest.as_os_str().is_empty()
                        || rest
                            .components()
                            .any(|part| !matches!(part, std::path::Component::Normal(_)))
                    {
                        continue;
                    }
                    if let Some(parent) = rest.parent() {
                        fs::create_dir_all(checkout.join(parent)).map_err(error)?;
                    }
                    entry.unpack(checkout.join(&rest)).map_err(error)?;
                }
            }
            git::remove_refs(root, git::REFS_ROOT)?;
        }
        (None, false) => {
            tar::Archive::new(BufReader::new(
                File::open(dir.join(CODE_TAR)).map_err(error)?,
            ))
            .unpack(root)
            .map_err(error)?;
        }
        _ => return Err("the move's manifest and its parts disagree on Git".into()),
    }
    paths.push((manifest.source_root.clone(), root.to_path_buf()));
    let paths = PathMap(paths);

    let threads_dir = dir.join("threads");
    remove_dir(&threads_dir).map_err(error)?;
    fs::create_dir_all(&threads_dir).map_err(error)?;
    tar::Archive::new(BufReader::new(
        File::open(dir.join(THREADS_TAR)).map_err(error)?,
    ))
    .unpack(&threads_dir)
    .map_err(error)?;
    let mut metas = Vec::new();
    let mut mutations = vec![Mutation::upsert_project(arrival.project.clone())];
    for id in &manifest.threads {
        if !safe_name(id) || !staged.thread_ids.contains(id) {
            return Err(format!("thread {id:?} was not announced"));
        }
        let thread = threads_dir.join(id);
        let meta: SessionMeta = read_json(&thread.join("meta.json")).map_err(error)?;
        if &meta.id != id {
            return Err(format!("thread {id} carries the meta of {}", meta.id));
        }
        let meta = arrived_meta(meta, &paths, arrival);
        for (from, to) in [
            (thread.join("attachments"), attachment_dir(&data_dir, id)),
            (
                thread.join("write-journal"),
                write_journal_dir(&data_dir, id),
            ),
        ] {
            if from.is_dir() {
                if to.exists() {
                    return Err(format!("{} is already there", to.display()));
                }
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(error)?;
                }
                fs::rename(&from, &to).map_err(error)?;
                made.dirs.push(to);
            }
        }
        mutations.push(Mutation::clear_redirect(id));
        mutations.push(Mutation::upsert_meta(meta.clone()));
        mutations.push(Mutation::import_event_log(id, &thread.join("events.jsonl")));
        mutations.push(
            Mutation::append_event(
                id,
                arrival.now_ms,
                &agent::AgentEvent::ThreadMoved {
                    from_machine: arrival.from_machine.to_owned(),
                    from_machine_name: arrival.from_machine_name.to_owned(),
                    to_machine: arrival.to_machine.to_owned(),
                },
            )
            .map_err(error)?,
        );
        metas.push(meta);
    }
    mutations.push(Mutation::record_move_in(
        arrival.move_id,
        &arrival.project.id,
    ));
    store.apply(&mutations).map_err(error)?;
    Ok(metas)
}

/// A thread's meta as it arrives: in the new project, its paths where they
/// are here, and its next turn starting a fresh session of its harness from
/// the conversation so far, as a move to another directory does when the
/// native session cannot go along — no native session travels.
fn arrived_meta(mut meta: SessionMeta, paths: &PathMap, arrival: &Arrival<'_>) -> SessionMeta {
    let root = arrival.root.to_path_buf();
    let source_cwd = meta.cwd.clone();
    let cwd = paths.map(&meta.cwd);
    if cwd.is_none() {
        // A worktree that did not go along: the thread continues at the root.
        meta.worktree = None;
    }
    meta.cwd = cwd.unwrap_or_else(|| root.clone());
    if let Some(worktree) = &mut meta.worktree {
        worktree.root_project_path = paths
            .map(&worktree.root_project_path)
            .unwrap_or_else(|| root.clone());
    }
    meta.other_worktrees
        .retain_mut(|owned| match paths.map(&owned.path) {
            Some(path) => {
                owned.path = path;
                owned.info.root_project_path = paths
                    .map(&owned.info.root_project_path)
                    .unwrap_or_else(|| root.clone());
                true
            }
            _ => false,
        });
    meta.project_id = Some(arrival.project.id.clone());
    // The transcript the next turn relays names paths where the thread last
    // had a native session, on the machine it left.
    meta.moved_from = Some(meta.moved_from.take().unwrap_or(source_cwd));
    meta.resume_cursor = None;
    meta.pending_fork = None;
    meta.session_transfer_from = None;
    meta.profile_id = (arrival.profile)(&meta);
    meta
}
