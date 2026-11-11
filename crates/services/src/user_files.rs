use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

pub fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let too_large = || {
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("file exceeds the {limit} byte size limit"),
        )
    };
    if file.metadata()?.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // Metadata can become stale while the file is read.
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Return the directory used to persist attachments for a session.
pub fn attachment_dir(data_root: &Path, session_id: &str) -> PathBuf {
    attachments_root(data_root).join(session_id)
}

/// The directory every session's attachment directory lives under; a client
/// may only save attachments below it.
pub fn attachments_root(data_root: &Path) -> PathBuf {
    data_root.join("attachments")
}

/// The folder under a day's scratch directory that holds uploaded files.
const UPLOADS_DIR: &str = "uploads";

/// A client's file name as one safe path component; the client's own folders
/// mean nothing on the host.
fn upload_file_name(name: &str) -> String {
    let name: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        .collect();
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." {
        return "file".to_string();
    }
    // Most filesystems stop at 255 bytes; the tail carries the extension.
    let mut start = name.len().saturating_sub(MAX_UPLOAD_NAME_BYTES);
    while !name.is_char_boundary(start) {
        start += 1;
    }
    name[start..].to_string()
}

const MAX_UPLOAD_NAME_BYTES: usize = 200;

/// Write `bytes` at `offset` of a reserved upload, returning its new length.
/// Only the next bytes in order are taken, so a chunk can never overwrite or
/// leave a hole in what already arrived.
pub fn append_upload(path: &Path, offset: u64, bytes: &[u8]) -> io::Result<u64> {
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    let length = file.metadata()?.len();
    if length != offset {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("upload is at byte {length}, not {offset}"),
        ));
    }
    file.write_all(bytes)?;
    Ok(length + bytes.len() as u64)
}

/// Remove a reserved upload and the folder made for it.
pub fn discard_upload(path: &Path) -> io::Result<()> {
    let folder = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    std::fs::remove_dir_all(folder)
}

#[derive(Clone, Debug)]
pub struct UserDirectories {
    pub home: Option<PathBuf>,
    pub documents: Option<PathBuf>,
}

impl Default for UserDirectories {
    fn default() -> Self {
        Self {
            home: dirs::home_dir(),
            documents: dirs::document_dir(),
        }
    }
}

impl UserDirectories {
    /// The folders directly inside `path` on this machine, for picking a
    /// project root: a host path as [`Self::resolve_host_path`] reads it, an
    /// empty one being the home itself.
    pub fn browse_directories(&self, path: &str) -> io::Result<tcode_protocol::DirectoryListing> {
        let path = match path.trim() {
            "" => "~",
            path => path,
        };
        let directory = self.resolve_host_path(Path::new(path))?;
        let read = std::fs::read_dir(&directory).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", directory.display()))
        })?;
        let mut entries = Vec::new();
        for entry in read.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // A link counts as the folder it leads to.
            let is_dir = kind.is_dir()
                || (kind.is_symlink() && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()));
            if is_dir {
                entries.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        entries.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
        Ok(tcode_protocol::DirectoryListing {
            parent: directory.parent().map(Path::to_path_buf),
            directory,
            home: self.home.clone(),
            entries,
        })
    }

    /// `path` as this machine names it: a leading `~` component is the home;
    /// anything else must be absolute. `..` and `.` are folded away without
    /// following links, so the path keeps the shape the user typed.
    pub fn resolve_host_path(&self, path: &Path) -> io::Result<PathBuf> {
        let expanded = match path.strip_prefix("~") {
            Ok(rest) => self
                .home
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Host home directory is unavailable.",
                    )
                })?
                .join(rest),
            Err(_) => path.to_path_buf(),
        };
        if !expanded.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path.", path.display()),
            ));
        }
        let mut normal = PathBuf::new();
        for component in expanded.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normal.pop();
                }
                other => normal.push(other),
            }
        }
        Ok(normal)
    }

    /// Create the folder `name` inside `parent` (Settings' new-project folder,
    /// `~` standing for the home) for a new project.
    pub fn new_project_root(&self, parent: &str, name: &str) -> std::io::Result<PathBuf> {
        let name = name.trim();
        if name.is_empty()
            || name.contains(['/', '\\'])
            || !matches!(
                Path::new(name).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            )
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Project name must be a non-empty single directory name without / or \\.",
            ));
        }
        let root = self.resolve_host_path(Path::new(parent.trim()))?.join(name);
        create_directory(&root)?;
        Ok(root)
    }

    pub fn scratch_root(&self) -> std::io::Result<PathBuf> {
        let documents = self
            .documents
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join("Documents")))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Host documents directory is unavailable.",
                )
            })?;
        Ok(documents.join("Tcode"))
    }

    /// The scratch root and its folder for `today`, the host's local date.
    pub fn scratch_directory(
        &self,
        today: chrono::NaiveDate,
    ) -> std::io::Result<(PathBuf, PathBuf)> {
        let root = self.scratch_root()?;
        let daily = root.join(today.to_string());
        create_directory(&daily)?;
        Ok((root, daily))
    }

    /// Reserve an empty host file for an upload to `session_id`, in today's
    /// scratch directory, which is also where quick threads work. Every
    /// upload has a folder of its own so it keeps the name the user gave it.
    pub fn reserve_upload(
        &self,
        session_id: &str,
        name: &str,
        today: chrono::NaiveDate,
    ) -> io::Result<PathBuf> {
        let (_, daily) = self.scratch_directory(today)?;
        let folder = daily
            .join(UPLOADS_DIR)
            .join(session_id)
            .join(uuid::Uuid::new_v4().to_string());
        create_directory(&folder)?;
        let path = folder.join(upload_file_name(name));
        std::fs::File::create_new(&path)?;
        Ok(path)
    }

    /// Whether `path` is a file [`Self::reserve_upload`] made for `session_id`.
    pub fn is_upload_of(&self, session_id: &str, path: &Path) -> bool {
        let Ok(root) = self.scratch_root() else {
            return false;
        };
        let Ok(tail) = path.strip_prefix(root) else {
            return false;
        };
        let parts: Vec<_> = tail.components().collect();
        matches!(
            parts.as_slice(),
            [
                Component::Normal(_),
                Component::Normal(uploads),
                Component::Normal(session),
                Component::Normal(_),
                Component::Normal(_),
            ] if *uploads == UPLOADS_DIR && *session == session_id
        )
    }

    /// Remove every day's uploads of `session_ids`. The day folders stay:
    /// quick threads work in them.
    pub fn remove_thread_uploads(&self, session_ids: &[String]) -> io::Result<()> {
        let root = self.scratch_root()?;
        let days = match std::fs::read_dir(&root) {
            Ok(days) => days,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for day in days {
            let day = day?;
            if !day.file_type()?.is_dir() {
                continue;
            }
            let uploads = day.path().join(UPLOADS_DIR);
            for session_id in session_ids {
                match std::fs::remove_dir_all(uploads.join(session_id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            // Fails while another thread still has uploads here.
            let _ = std::fs::remove_dir(&uploads);
        }
        Ok(())
    }
}

fn create_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("Could not create {}: {error}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Add Project's folder list: `~` is this host's home, `..` folds without
    /// following links, only folders are listed (a link to one included, in
    /// name order ignoring case), and a path the host cannot place is refused.
    #[test]
    fn browsing_lists_the_folders_of_a_host_directory() {
        let home = std::env::temp_dir().join(format!("tcode-browse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("dev/beta")).unwrap();
        std::fs::create_dir_all(home.join("dev/Alpha")).unwrap();
        std::fs::create_dir_all(home.join("dev/.hidden")).unwrap();
        std::fs::write(home.join("dev/notes.txt"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.join("dev/beta"), home.join("dev/linked")).unwrap();
        let directories = UserDirectories {
            home: Some(home.clone()),
            documents: None,
        };

        let listing = directories.browse_directories("~/dev/beta/../").unwrap();
        assert_eq!(listing.directory, home.join("dev"));
        assert_eq!(listing.parent.as_deref(), Some(home.as_path()));
        assert_eq!(listing.home.as_deref(), Some(home.as_path()));
        let mut expected = vec![".hidden", "Alpha", "beta"];
        if cfg!(unix) {
            expected.push("linked");
        }
        assert_eq!(listing.entries, expected);

        assert_eq!(directories.browse_directories("").unwrap().directory, home);
        assert_eq!(
            directories.browse_directories("~").unwrap().entries,
            ["dev"]
        );
        assert_eq!(
            directories.browse_directories("dev").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            directories
                .browse_directories(home.join("missing").to_str().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound,
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
