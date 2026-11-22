//! What a thread's sessions wrote through Tcode, kept so the writes can be
//! taken back for a harness with no rewind of its own (CONTRIBUTING,
//! Principle 1).
//!
//! The journal holds, for the first write to a file in each turn, the bytes
//! the write replaced, and for every write a digest of what it left. It lives
//! in Tcode's own data, never in the workspace, and costs what the sessions
//! themselves overwrote: nothing is scanned, snapshotted or hashed but the
//! files an agent wrote, as it wrote them.
//!
//! A restore undoes a suffix of the journal: every write from the first one
//! of the turns it is given, in the order they were made. Files the agent
//! changed by running commands were never written through Tcode and are not
//! in it ([`crate::RewindControl::restores_command_edits`]).

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{RewindConflict, RewindConflictKind, RewindFile, RewindFiles, RewindPreview};

const INDEX: &str = "index.jsonl";
const BLOBS: &str = "blobs";

/// One write, as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Line {
    /// The restore point of the turn the write was made in
    /// ([`crate::AgentEvent::TurnCheckpoint`]).
    checkpoint: String,
    path: PathBuf,
    /// What the turn's first write to `path` replaced; absent on its later
    /// writes to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<Before>,
    /// The digest of what the write left.
    after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Before {
    /// There was no file.
    Absent,
    /// The file's bytes, by the digest they are kept under.
    Blob(String),
}

/// A thread's journal, in a directory of its own.
#[derive(Debug)]
pub struct WriteJournal {
    dir: PathBuf,
    /// The turn and file pairs whose replaced bytes are already kept.
    kept: HashSet<(String, PathBuf)>,
}

/// What undoing a suffix of the journal does to one file.
struct Undo {
    before: Before,
    after: String,
}

impl WriteJournal {
    /// The journal in `dir`, which is made with its first write.
    pub fn open(dir: PathBuf) -> Self {
        let kept = read_lines(&dir)
            .into_iter()
            .filter(|line| line.before.is_some())
            .map(|line| (line.checkpoint, line.path))
            .collect();
        Self { dir, kept }
    }

    /// Keep what a write of `content` to `path` in the turn of `checkpoint`
    /// is about to replace. Called before the write is made, so a write that
    /// then fails leaves a record that restores the file to what it still is.
    pub fn before_write(
        &mut self,
        checkpoint: &str,
        path: &Path,
        content: &[u8],
    ) -> io::Result<()> {
        let key = (checkpoint.to_owned(), path.to_path_buf());
        let before = if self.kept.contains(&key) {
            None
        } else {
            Some(match std::fs::read(path) {
                Ok(bytes) => Before::Blob(self.keep_blob(&bytes)?),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Before::Absent,
                Err(err) => return Err(err),
            })
        };
        let line = Line {
            checkpoint: key.0.clone(),
            path: key.1.clone(),
            before,
            after: digest(content),
        };
        std::fs::create_dir_all(&self.dir)?;
        let mut index = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(INDEX))?;
        let mut record = serde_json::to_vec(&line).map_err(io::Error::other)?;
        record.push(b'\n');
        index.write_all(&record)?;
        if line.before.is_some() {
            self.kept.insert(key);
        }
        Ok(())
    }

    fn keep_blob(&self, bytes: &[u8]) -> io::Result<String> {
        let name = digest(bytes);
        let blobs = self.dir.join(BLOBS);
        let path = blobs.join(&name);
        if !path.exists() {
            std::fs::create_dir_all(&blobs)?;
            // Renamed into place, so a blob that is there is whole.
            let partial = blobs.join(format!("{name}.partial"));
            std::fs::write(&partial, bytes)?;
            std::fs::rename(&partial, &path)?;
        }
        Ok(name)
    }

    /// What [`restore`] to before the turns of `checkpoints` would change,
    /// and which of those files something other than the sessions changed
    /// since, without changing anything.
    pub fn preview(dir: &Path, checkpoints: &[String]) -> RewindPreview {
        let mut preview = RewindPreview::default();
        for (path, undo) in undos(&read_lines(dir), checkpoints).1 {
            let current = std::fs::read(&path);
            let conflict = match &current {
                Ok(bytes) if digest(bytes) == undo.after => None,
                Ok(_) => Some(RewindConflictKind::ModifiedExternally),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    Some(RewindConflictKind::DeletedExternally)
                }
                Err(_) => Some(RewindConflictKind::Other),
            };
            let restored = match (&undo.before, &current) {
                (Before::Absent, Err(err)) => err.kind() == io::ErrorKind::NotFound,
                (Before::Blob(kept), Ok(bytes)) => digest(bytes) == *kept,
                _ => false,
            };
            if restored {
                continue;
            }
            let path = path.to_string_lossy().into_owned();
            if let Some(kind) = conflict {
                preview.conflicts.push(RewindConflict {
                    path: path.clone(),
                    kind,
                });
            }
            preview.files.files.push(RewindFile::path(path));
        }
        preview
    }

    /// Put every file the sessions wrote from the turns of `checkpoints` on
    /// back as it was before the first of those writes, and forget the writes.
    /// A file a write created is removed. What changed a file since is
    /// overwritten, as [`preview`] says beforehand. Fails on the first file
    /// that cannot be put back, with the journal as it was.
    pub fn restore(dir: &Path, checkpoints: &[String]) -> io::Result<RewindFiles> {
        let lines = read_lines(dir);
        let (from, undos) = undos(&lines, checkpoints);
        let mut files = RewindFiles::default();
        for (path, undo) in undos {
            match &undo.before {
                Before::Absent => match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                },
                Before::Blob(kept) => {
                    let bytes = std::fs::read(dir.join(BLOBS).join(kept))?;
                    if std::fs::read(&path).is_ok_and(|current| current == bytes) {
                        continue;
                    }
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, bytes)?;
                }
            }
            files
                .files
                .push(RewindFile::path(path.to_string_lossy().into_owned()));
        }
        if let Some(from) = from {
            forget(dir, &lines[..from], &lines[from..])?;
        }
        Ok(files)
    }
}

/// Rewrite the index as `kept` and drop the blobs only `dropped` named.
fn forget(dir: &Path, kept: &[Line], dropped: &[Line]) -> io::Result<()> {
    let mut index = Vec::new();
    for line in kept {
        serde_json::to_writer(&mut index, line).map_err(io::Error::other)?;
        index.push(b'\n');
    }
    let partial = dir.join(format!("{INDEX}.partial"));
    std::fs::write(&partial, index)?;
    std::fs::rename(&partial, dir.join(INDEX))?;
    let blob = |line: &Line| match &line.before {
        Some(Before::Blob(name)) => Some(name.clone()),
        _ => None,
    };
    let needed: HashSet<String> = kept.iter().filter_map(blob).collect();
    for name in dropped.iter().filter_map(blob) {
        if !needed.contains(&name) {
            let _ = std::fs::remove_file(dir.join(BLOBS).join(name));
        }
    }
    Ok(())
}

/// The index's whole lines, in the order they were written. A line a crash
/// cut short is no write that was made: the write follows its line.
fn read_lines(dir: &Path) -> Vec<Line> {
    let Ok(text) = std::fs::read_to_string(dir.join(INDEX)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Where the writes of the turns of `checkpoints` begin in `lines`, and what
/// undoing everything from there does to each file: back to what the first of
/// those writes replaced, from what the last one left.
fn undos(lines: &[Line], checkpoints: &[String]) -> (Option<usize>, BTreeMap<PathBuf, Undo>) {
    let mut undos = BTreeMap::new();
    let Some(from) = lines
        .iter()
        .position(|line| checkpoints.contains(&line.checkpoint))
    else {
        return (None, undos);
    };
    for line in &lines[from..] {
        match undos.get_mut(&line.path) {
            Some(Undo { after, .. }) => *after = line.after.clone(),
            None => {
                // A turn's later write to a file follows its first, which is
                // in the suffix with it.
                let Some(before) = line.before.clone() else {
                    continue;
                };
                undos.insert(
                    line.path.clone(),
                    Undo {
                        before,
                        after: line.after.clone(),
                    },
                );
            }
        }
    }
    (Some(from), undos)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("tcode-write-journal-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("work")).unwrap();
            Self(root)
        }

        fn journal(&self) -> PathBuf {
            self.0.join("journal")
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join("work").join(name)
        }

        /// A session's write of `content` to `name` in the turn of `checkpoint`.
        fn write(&self, journal: &mut WriteJournal, checkpoint: &str, name: &str, content: &[u8]) {
            let path = self.file(name);
            journal.before_write(checkpoint, &path, content).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn read(&self, name: &str) -> Option<Vec<u8>> {
            std::fs::read(self.file(name)).ok()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn turns(checkpoints: &[&str]) -> Vec<String> {
        checkpoints.iter().map(|id| (*id).to_owned()).collect()
    }

    fn paths(files: &RewindFiles) -> Vec<String> {
        let mut paths: Vec<String> = files
            .files
            .iter()
            .map(|file| {
                Path::new(&file.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        paths.sort();
        paths
    }

    /// Restoring to before a turn puts each file back byte for byte as the
    /// turn's first write found it, whatever later turns wrote, removes the
    /// files those turns created, and leaves earlier turns' writes alone. The
    /// bytes include what a text diff does not carry: carriage returns, a
    /// missing final newline, and bytes that are no text.
    #[test]
    fn a_restore_puts_back_what_the_turns_first_writes_replaced() {
        let scratch = Scratch::new();
        std::fs::write(scratch.file("kept.txt"), b"one\r\ntwo").unwrap();
        std::fs::write(scratch.file("binary"), [0xff, 0x00, 0xfe]).unwrap();
        let mut journal = WriteJournal::open(scratch.journal());
        scratch.write(&mut journal, "t1", "kept.txt", b"first turn\n");
        scratch.write(&mut journal, "t2", "kept.txt", b"second turn\n");
        scratch.write(&mut journal, "t2", "kept.txt", b"second turn again\n");
        scratch.write(&mut journal, "t2", "binary", b"text now\n");
        scratch.write(&mut journal, "t3", "made.txt", b"new\n");
        scratch.write(&mut journal, "t3", "kept.txt", b"third turn\n");

        let preview = WriteJournal::preview(&scratch.journal(), &turns(&["t2", "t3"]));
        assert_eq!(paths(&preview.files), ["binary", "kept.txt", "made.txt"]);
        assert_eq!(preview.conflicts, []);

        let restored = WriteJournal::restore(&scratch.journal(), &turns(&["t2", "t3"])).unwrap();
        assert_eq!(paths(&restored), ["binary", "kept.txt", "made.txt"]);
        assert_eq!(scratch.read("kept.txt").unwrap(), b"first turn\n");
        assert_eq!(scratch.read("binary").unwrap(), [0xff, 0x00, 0xfe]);
        assert_eq!(scratch.read("made.txt"), None);

        // The first turn's write is still the journal's to take back.
        let restored = WriteJournal::restore(&scratch.journal(), &turns(&["t1"])).unwrap();
        assert_eq!(paths(&restored), ["kept.txt"]);
        assert_eq!(scratch.read("kept.txt").unwrap(), b"one\r\ntwo");
        assert_eq!(
            WriteJournal::preview(&scratch.journal(), &turns(&["t1", "t2", "t3"])),
            RewindPreview::default()
        );
    }

    /// A file something else changed, or removed, after the sessions' last
    /// write is named a conflict before the restore, which then overwrites it.
    #[test]
    fn a_preview_names_what_changed_outside_the_sessions_since() {
        let scratch = Scratch::new();
        std::fs::write(scratch.file("edited.txt"), b"original\n").unwrap();
        std::fs::write(scratch.file("removed.txt"), b"original\n").unwrap();
        let mut journal = WriteJournal::open(scratch.journal());
        scratch.write(&mut journal, "t1", "edited.txt", b"agent\n");
        scratch.write(&mut journal, "t1", "removed.txt", b"agent\n");
        scratch.write(&mut journal, "t1", "clean.txt", b"agent\n");
        std::fs::write(scratch.file("edited.txt"), b"mine\n").unwrap();
        std::fs::remove_file(scratch.file("removed.txt")).unwrap();

        let preview = WriteJournal::preview(&scratch.journal(), &turns(&["t1"]));
        let conflict = |name: &str| {
            preview
                .conflicts
                .iter()
                .find(|conflict| conflict.path.ends_with(name))
                .map(|conflict| conflict.kind)
        };
        assert_eq!(
            conflict("edited.txt"),
            Some(RewindConflictKind::ModifiedExternally)
        );
        assert_eq!(
            conflict("removed.txt"),
            Some(RewindConflictKind::DeletedExternally)
        );
        assert_eq!(conflict("clean.txt"), None);

        WriteJournal::restore(&scratch.journal(), &turns(&["t1"])).unwrap();
        assert_eq!(scratch.read("edited.txt").unwrap(), b"original\n");
        assert_eq!(scratch.read("removed.txt").unwrap(), b"original\n");
        assert_eq!(scratch.read("clean.txt"), None);
    }

    /// The writes of turns a conversation-only rewind took out of the log are
    /// still on disk and still in the journal: a later restore to before an
    /// earlier turn takes them back too, and one to before a turn made after
    /// that rewind leaves them.
    #[test]
    fn a_restore_undoes_every_write_made_since_its_first_turn() {
        let scratch = Scratch::new();
        std::fs::write(scratch.file("a.txt"), b"start\n").unwrap();
        let mut journal = WriteJournal::open(scratch.journal());
        scratch.write(&mut journal, "t1", "a.txt", b"t1\n");
        scratch.write(
            &mut journal,
            "gone",
            "b.txt",
            b"from a turn rewound out of the log\n",
        );
        // The journal a later session of the thread opens.
        let mut journal = WriteJournal::open(journal.dir);
        scratch.write(&mut journal, "t2", "c.txt", b"t2\n");

        WriteJournal::restore(&scratch.journal(), &turns(&["t2"])).unwrap();
        assert_eq!(scratch.read("c.txt"), None);
        assert!(scratch.read("b.txt").is_some());

        // `gone` is no turn of the log now, and `t2` wrote nothing since.
        WriteJournal::restore(&scratch.journal(), &turns(&["t1", "t2"])).unwrap();
        assert_eq!(scratch.read("a.txt").unwrap(), b"start\n");
        assert_eq!(scratch.read("b.txt"), None);
    }
}
