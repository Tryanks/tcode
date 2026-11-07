//! The base64 runs cut out of event rows, each kept once as a file of its
//! decoded bytes named by their SHA-256: `blobs/sha256/ab/<62 hex>`.
//!
//! A blob is on disk before any row naming it commits, so a crash leaves at
//! most an unreferenced file. Writing a blob that exists refreshes its mtime;
//! [`Blobs::sweep`] removes only files older than its cutoff that no row
//! names, so a blob written for a row about to commit is never taken.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use super::codec::{Blob, Hash};

pub(super) const DIR: &str = "blobs";

pub(super) struct Blobs {
    root: PathBuf,
}

fn hex(hash: &Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hex(name: &str) -> Option<Hash> {
    if name.len() != 64 {
        return None;
    }
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(name.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(hash)
}

impl Blobs {
    pub(super) fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(DIR),
        }
    }

    fn path(&self, hash: &Hash) -> PathBuf {
        let name = hex(hash);
        self.root.join("sha256").join(&name[..2]).join(&name[2..])
    }

    /// Make every blob durable on disk, writing the missing ones.
    pub(super) fn put(&self, blobs: &[Blob]) -> io::Result<()> {
        if blobs.is_empty() {
            return Ok(());
        }
        let tmp = self.root.join("tmp");
        let mut dirs = BTreeSet::new();
        for blob in blobs {
            let path = self.path(&blob.hash);
            match File::options().write(true).open(&path) {
                Ok(file) => {
                    file.set_modified(SystemTime::now())?;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let dir = path.parent().expect("a blob path has a directory");
            if !dir.is_dir() {
                fs::create_dir_all(dir)?;
                dirs.insert(self.root.join("sha256"));
                dirs.insert(self.root.clone());
            }
            fs::create_dir_all(&tmp)?;
            let staged = tmp.join(format!("{}-{}", hex(&blob.hash), uuid::Uuid::new_v4()));
            let mut file = File::options().write(true).create_new(true).open(&staged)?;
            file.write_all(&blob.bytes)?;
            flush(&file)?;
            drop(file);
            fs::rename(&staged, &path)?;
            dirs.insert(dir.to_path_buf());
        }
        for dir in &dirs {
            flush(&File::open(dir)?)?;
        }
        // Apple drives keep what a plain fsync handed them in a volatile
        // cache: one F_FULLFSYNC (what `sync_all` issues there) commits every
        // file and entry above before any row naming them can commit.
        #[cfg(target_vendor = "apple")]
        if let Some(dir) = dirs.first() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// The blob's bytes, or `None` when it is missing.
    pub(super) fn read(&self, hash: &Hash) -> io::Result<Option<Arc<Vec<u8>>>> {
        match fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(Arc::new(bytes))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Every blob file and staged temporary last written before `cutoff`,
    /// with whether it is a temporary.
    pub(super) fn older_than(
        &self,
        cutoff: SystemTime,
    ) -> io::Result<Vec<(PathBuf, Option<Hash>)>> {
        let mut found = Vec::new();
        let modified_before =
            |path: &Path| -> io::Result<bool> { Ok(fs::metadata(path)?.modified()? < cutoff) };
        for (dir, sharded) in [
            (self.root.join("sha256"), true),
            (self.root.join("tmp"), false),
        ] {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                if !sharded {
                    if modified_before(&entry.path())? {
                        found.push((entry.path(), None));
                    }
                    continue;
                }
                let prefix = entry.file_name().to_string_lossy().into_owned();
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                for file in fs::read_dir(entry.path())? {
                    let file = file?;
                    let name = format!("{prefix}{}", file.file_name().to_string_lossy());
                    if let Some(hash) = parse_hex(&name)
                        && modified_before(&file.path())?
                    {
                        found.push((file.path(), Some(hash)));
                    }
                }
            }
        }
        Ok(found)
    }

    /// Remove the candidates no row names and that are still older than
    /// `cutoff`, returning how many files and bytes went.
    pub(super) fn sweep(
        &self,
        candidates: &[(PathBuf, Option<Hash>)],
        referenced: &HashSet<Hash>,
        cutoff: SystemTime,
    ) -> io::Result<(usize, u64)> {
        let (mut files, mut bytes) = (0, 0);
        for (path, hash) in candidates {
            if hash.is_some_and(|hash| referenced.contains(&hash)) {
                continue;
            }
            let metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.modified()? >= cutoff {
                continue;
            }
            match fs::remove_file(path) {
                Ok(()) => {
                    files += 1;
                    bytes += metadata.len();
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok((files, bytes))
    }
}

/// Hand a file's data or a directory's entries to the drive.
fn flush(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is open for the call.
        if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        // Windows cannot open a directory as a file; its entries are
        // committed with the metadata journal.
        if file.metadata()?.is_dir() {
            return Ok(());
        }
        file.sync_all()
    }
}
