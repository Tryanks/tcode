//! Moving the data dir an older build kept in the platform data directory
//! (`~/Library/Application Support/tcode`, `~/.local/share/tcode`,
//! `%APPDATA%\tcode`), or the one `LEGACY_TCODE_DATA_DIR` names, into the data
//! dir, before anything else reads either.
//!
//! Every entry of the older directory is renamed into the data dir. Across
//! filesystems an entry is instead copied into `tcode.relocating/` in the data
//! dir and compared byte for byte with its source; the source is then retired
//! into the older directory's `tcode.relocated/`, the copy renamed into place
//! and the retired source deleted. `tcode.relocating/` marks the move as
//! started and is removed last, after the emptied older directory.
//!
//! A start after an interruption publishes a staged copy whose source was
//! retired (it was verified first), discards any other staged copy, deletes
//! retired sources whose copy is in place, and moves what is left.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use super::migrate::{
    CHUNK_BYTES, Migration, MigrationPhase, MigrationProgress, Progress, is_cancelled, sync_dir,
};
use super::{DB_FILE, LOCK_FILE};

const LEGACY_DATA_DIR_ENV: &str = "LEGACY_TCODE_DATA_DIR";
/// In the data dir: present while a move is unfinished, and the staging area
/// of a copy.
const STAGING_DIR: &str = "tcode.relocating";
/// In the older directory: sources whose verified copy is staged.
const RETIRED_DIR: &str = "tcode.relocated";
/// Not data: the older directory's lock and Finder's view settings are deleted
/// with it rather than moved, so they never collide with the data dir's own.
const LEFT_BEHIND: [&str; 2] = [LOCK_FILE, ".DS_Store"];
/// What makes a directory a data dir rather than, say, `~/.tcode` holding only
/// Orchestrate's `worktrees/`.
const DATA_FILES: [&str; 7] = [
    DB_FILE,
    "tcode.db.migrating",
    "sessions.json",
    "settings.json",
    "secrets.json",
    "traverse.json",
    "device.json",
];

/// The directory to move from: `LEGACY_TCODE_DATA_DIR`, else the platform
/// data directory an older build used, unless the data dir was chosen
/// explicitly (`TCODE_DATA_DIR` or `--data-dir`).
pub(super) fn source(explicit_root: bool) -> Option<PathBuf> {
    match std::env::var_os(LEGACY_DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None if explicit_root => None,
        None => dirs::data_dir().map(|dir| dir.join("tcode")),
    }
}

/// Whether `previous` has to move into `root` before the store opens: a move
/// was started, or `root` holds no data and `previous`, another directory,
/// does.
pub(super) fn needed(root: &Path, previous: Option<&Path>) -> io::Result<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    if root.join(STAGING_DIR).exists() {
        return Ok(true);
    }
    let previous = match fs::canonicalize(previous) {
        Ok(previous) => previous,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if previous == fs::canonicalize(root)? {
        return Ok(false);
    }
    Ok(!holds_data(root)? && holds_data(&previous)?)
}

fn holds_data(dir: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if DATA_FILES.contains(&name.as_ref())
            || (name.ends_with(".jsonl") && entry.file_type()?.is_file())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Move `previous` into `root` if [`needed`], reporting
/// [`MigrationPhase::Relocating`]. `cancel` is checked after every entry and
/// every chunk copied; a cancelled move keeps what it moved, discards a
/// partial copy and is continued by the next start. Called with the data dir's
/// ownership lock held.
pub(super) fn run(
    root: &Path,
    previous: Option<&Path>,
    report: &mut dyn FnMut(MigrationProgress),
    cancel: &AtomicBool,
) -> io::Result<Migration> {
    let Some(previous) = previous else {
        return Ok(Migration::Completed);
    };
    if !needed(root, Some(previous))? {
        return Ok(Migration::Completed);
    }
    let mut progress = Progress {
        report,
        cancel,
        current: MigrationProgress {
            phase: MigrationPhase::Relocating,
            threads_done: 0,
            threads_total: 0,
            bytes_done: 0,
            bytes_total: 0,
        },
    };
    match relocate(root, previous, &mut progress) {
        Ok(()) => Ok(Migration::Completed),
        Err(error) if is_cancelled(&error) => {
            log::info!(
                "moving {} into {} cancelled; the next start continues",
                previous.display(),
                root.display()
            );
            Ok(Migration::Cancelled)
        }
        Err(error) => Err(error),
    }
}

fn relocate(root: &Path, previous: &Path, progress: &mut Progress) -> io::Result<()> {
    let staging = root.join(STAGING_DIR);
    if !previous.exists() {
        // Interrupted after the older directory was removed.
        fs::remove_dir_all(&staging)?;
        return sync_dir(root);
    }
    let (canonical_root, canonical_previous) =
        (fs::canonicalize(root)?, fs::canonicalize(previous)?);
    if canonical_root.starts_with(&canonical_previous)
        || canonical_previous.starts_with(&canonical_root)
    {
        return Err(io::Error::other(format!(
            "cannot move {} into {}: one contains the other",
            previous.display(),
            root.display()
        )));
    }
    let lock = lock(previous)?;
    recover(root, previous)?;
    let entries = entries(previous)?;
    if let Some(name) = entries
        .iter()
        .find(|name| fs::symlink_metadata(root.join(name)).is_ok())
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "cannot move {} into {}: both contain {name}; stopped without moving or deleting \
                 anything. Move or remove one of them, then start Tcode again",
                previous.display(),
                root.display(),
                name = name.to_string_lossy()
            ),
        ));
    }
    progress.start(MigrationPhase::Relocating, entries.len(), 0)?;
    fs::create_dir_all(&staging)?;
    sync_dir(root)?;
    let mut copying = false;
    for (index, name) in entries.iter().enumerate() {
        let source = previous.join(name);
        if !copying {
            match fs::rename(&source, root.join(name)) {
                Ok(()) => {
                    progress.advance(1, 0)?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                    copying = true;
                    let mut total = 0;
                    for name in &entries[index..] {
                        total += tree_size(&previous.join(name))?;
                    }
                    progress.current.bytes_total = total;
                    log::info!(
                        "{} and {} are on different filesystems; copying {total} bytes",
                        previous.display(),
                        root.display()
                    );
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "could not move {} into {}: {error}",
                            source.display(),
                            root.display()
                        ),
                    ));
                }
            }
        }
        copy_entry(root, previous, name, progress)?;
        progress.advance(1, 0)?;
    }
    sync_dir(root)?;

    // Windows keeps a deleted file's name until its last handle closes.
    drop(lock);
    for name in LEFT_BEHIND {
        remove_if_present(&previous.join(name))?;
    }
    remove_if_present(&previous.join(RETIRED_DIR))?;
    fs::remove_dir(previous).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "moved every entry of {} into {}, but could not remove it: {error}",
                previous.display(),
                root.display()
            ),
        )
    })?;
    if let Some(parent) = previous.parent() {
        sync_dir(parent)?;
    }
    fs::remove_dir_all(&staging)?;
    sync_dir(root)?;
    log::info!(
        "moved {} entries from {} into {}",
        entries.len(),
        previous.display(),
        root.display()
    );
    Ok(())
}

/// Hold the older directory's lock, which the build that used it takes while
/// it runs. Kept until the lock file is deleted with the directory.
fn lock(previous: &Path) -> io::Result<File> {
    let path = previous.join(LOCK_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::Error(error)) => Err(error),
        Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!(
                "another Tcode is still using {}; quit it, then start this one again to move \
                 its data",
                previous.display()
            ),
        )),
    }
}

/// The older directory's entries to move, by name.
fn entries(previous: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(previous)? {
        let name = entry?.file_name();
        if name != RETIRED_DIR && !LEFT_BEHIND.iter().any(|left| name == *left) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Finish what an interrupted copy left: a retired source's staged copy is
/// complete and is published, a retired source whose copy is in place is
/// deleted, and any other staged copy is partial and discarded.
fn recover(root: &Path, previous: &Path) -> io::Result<()> {
    let staging = root.join(STAGING_DIR);
    let retired = previous.join(RETIRED_DIR);
    if retired.exists() {
        for entry in fs::read_dir(&retired)? {
            let name = entry?.file_name();
            let staged = staging.join(&name);
            let destination = root.join(&name);
            if fs::symlink_metadata(&staged).is_ok() {
                if fs::symlink_metadata(&destination).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "cannot finish moving {}: {} already exists",
                            retired.join(&name).display(),
                            destination.display()
                        ),
                    ));
                }
                fs::rename(&staged, &destination)?;
                sync_dir(root)?;
            } else if fs::symlink_metadata(&destination).is_err() {
                // Retiring follows the staged copy, so this is not one of
                // ours; it goes back to be moved like any other entry.
                fs::rename(retired.join(&name), previous.join(&name))?;
                continue;
            }
            remove_if_present(&retired.join(&name))?;
        }
    }
    if staging.exists() {
        for entry in fs::read_dir(&staging)? {
            let path = entry?.path();
            log::warn!("discarding {}, a partial copy", path.display());
            remove_if_present(&path)?;
        }
    }
    Ok(())
}

/// Move one entry across filesystems: stage a verified copy, retire the
/// source, publish the copy, delete the source.
fn copy_entry(
    root: &Path,
    previous: &Path,
    name: &std::ffi::OsStr,
    progress: &mut Progress,
) -> io::Result<()> {
    let staging = root.join(STAGING_DIR);
    let staged = staging.join(name);
    let source = previous.join(name);
    if let Err(error) = copy_tree(&source, &staged, progress) {
        if let Err(cleanup) = remove_if_present(&staged) {
            log::warn!("could not discard {}: {cleanup}", staged.display());
        }
        return Err(error);
    }
    sync_dir(&staging)?;
    let retired = previous.join(RETIRED_DIR);
    fs::create_dir_all(&retired)?;
    fs::rename(&source, retired.join(name))?;
    sync_dir(previous)?;
    sync_dir(&retired)?;
    fs::rename(&staged, root.join(name))?;
    sync_dir(root)?;
    remove_if_present(&retired.join(name))
}

fn copy_tree(source: &Path, destination: &Path, progress: &mut Progress) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(source)?;
        symlink(source, &target, destination)?;
        if fs::read_link(destination)? != target {
            return Err(mismatch(source, destination));
        }
    } else if kind.is_dir() {
        fs::create_dir(destination)?;
        let mut names = fs::read_dir(source)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        for name in names {
            copy_tree(&source.join(&name), &destination.join(&name), progress)?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
        sync_dir(destination)?;
    } else if kind.is_file() {
        copy_file(source, destination, progress)?;
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        log::warn!(
            "not moving {}: not a file, directory or link",
            source.display()
        );
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path, progress: &mut Progress) -> io::Result<()> {
    let mut reader = File::open(source)?;
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        progress.advance(0, read as u64)?;
    }
    writer.sync_all()?;
    drop(writer);
    verify_file(source, destination)
}

/// Read the copy back and compare it with its source, length and bytes.
fn verify_file(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::metadata(source)?.len() != fs::metadata(destination)?.len() {
        return Err(mismatch(source, destination));
    }
    let (mut original, mut copy) = (File::open(source)?, File::open(destination)?);
    let (mut expected, mut actual) = (vec![0; CHUNK_BYTES], vec![0; CHUNK_BYTES]);
    loop {
        let read = original.read(&mut expected)?;
        if read == 0 {
            return if copy.read(&mut actual[..1])? == 0 {
                Ok(())
            } else {
                Err(mismatch(source, destination))
            };
        }
        copy.read_exact(&mut actual[..read])?;
        if expected[..read] != actual[..read] {
            return Err(mismatch(source, destination));
        }
    }
}

fn mismatch(source: &Path, destination: &Path) -> io::Error {
    io::Error::other(format!(
        "the copy {} differs from {}; the source is kept",
        destination.display(),
        source.display()
    ))
}

/// Bytes of every file under `path`, not following links.
fn tree_size(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(if metadata.is_file() {
            metadata.len()
        } else {
            0
        });
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += tree_size(&entry?.path())?;
    }
    Ok(total)
}

/// Delete a file, link or directory tree, without following links.
fn remove_if_present(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            make_removable(path).and_then(|()| fs::remove_dir_all(path))
        }
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Unix removes an entry only from a writable directory, and a profile
/// home's Go module cache keeps its directories read-only.
#[cfg(unix)]
fn make_removable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = fs::symlink_metadata(dir)?.permissions();
    if permissions.mode() & 0o700 != 0o700 {
        permissions.set_mode(permissions.mode() | 0o700);
        fs::set_permissions(dir, permissions)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_removable(&entry.path())?;
        }
    }
    Ok(())
}

/// std's Windows `remove_dir_all` deletes with POSIX semantics, which ignore
/// the read-only attribute on NTFS.
#[cfg(not(unix))]
fn make_removable(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(_source: &Path, target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows distinguishes links to directories from links to files.
#[cfg(windows)]
fn symlink(source: &Path, target: &Path, link: &Path) -> io::Result<()> {
    if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests;
