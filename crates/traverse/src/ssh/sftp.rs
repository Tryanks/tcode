//! The `sftp` subsystem: the machine user's files as OpenSSH's sftp-server
//! serves them, relative paths starting from the home directory. On Windows
//! a path reads `/C:/Users/...`, as Win32-OpenSSH writes it.
use std::{
    collections::HashMap,
    io,
    path::{Component, Path, PathBuf},
};

use russh_sftp::{
    protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
    },
    server::StatusReply,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

pub(super) struct Sftp {
    home: PathBuf,
    handles: HashMap<String, Open>,
    next_handle: u64,
}

/// Directory entries one `readdir` answer holds at most, as OpenSSH's
/// sftp-server sends them, so an answer stays within any client's packet.
const ENTRIES_PER_READ: usize = 100;

enum Open {
    File(tokio::fs::File),
    /// `entries` is `None` until the first `readdir` reads the directory.
    Dir {
        path: PathBuf,
        entries: Option<std::collections::VecDeque<File>>,
    },
}

impl Sftp {
    pub(super) fn new(home: PathBuf) -> Self {
        Self {
            home,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn local(&self, path: &str) -> PathBuf {
        #[cfg(windows)]
        let path = match path.as_bytes() {
            [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => &path[1..],
            _ => path,
        };
        normalize(&self.home.join(path))
    }

    fn insert(&mut self, open: Open) -> String {
        self.next_handle += 1;
        let handle = self.next_handle.to_string();
        self.handles.insert(handle.clone(), open);
        handle
    }

    fn file(&mut self, handle: &str) -> Result<&mut tokio::fs::File, StatusReply> {
        match self.handles.get_mut(handle) {
            Some(Open::File(file)) => Ok(file),
            _ => Err(StatusCode::Failure.with_message("no such handle")),
        }
    }
}

/// `path` without `.` and `..`, which never climbs above its root.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normal.parent().is_some() {
                    normal.pop();
                }
            }
            other => normal.push(other),
        }
    }
    normal
}

fn remote(path: &Path) -> String {
    #[cfg(windows)]
    {
        format!("/{}", path.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn failure(error: io::Error) -> StatusReply {
    let code = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    };
    code.with_message(error.to_string())
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

/// Apply what `attrs` sets to `path`. Owners are left alone, as a user's
/// sftp-server cannot change them either.
async fn apply(path: &Path, attrs: &FileAttributes) -> io::Result<()> {
    if let Some(size) = attrs.size {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await?
            .set_len(size)
            .await?;
    }
    #[cfg(unix)]
    if let Some(mode) = attrs.permissions {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777)).await?;
    }
    if let (Some(atime), Some(mtime)) = (attrs.atime, attrs.mtime) {
        let path = path.to_path_buf();
        let at =
            |seconds: u32| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds.into());
        let times = std::fs::FileTimes::new()
            .set_accessed(at(atime))
            .set_modified(at(mtime));
        tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .or_else(|_| std::fs::File::open(&path))?
                .set_times(times)
        })
        .await
        .map_err(io::Error::other)??;
    }
    Ok(())
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported.into()
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut options = tokio::fs::OpenOptions::new();
        options
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .append(pflags.contains(OpenFlags::APPEND));
        if pflags.contains(OpenFlags::CREATE) {
            if pflags.contains(OpenFlags::EXCLUDE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        if pflags.contains(OpenFlags::TRUNCATE) {
            options.truncate(true);
        }
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            options.mode(mode & 0o7777);
        }
        #[cfg(not(unix))]
        let _ = attrs;
        let file = options.open(self.local(&filename)).await.map_err(failure)?;
        Ok(Handle {
            id,
            handle: self.insert(Open::File(file)),
        })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(Open::File(mut file)) => file.flush().await.map_err(failure)?,
            Some(Open::Dir { .. }) => {}
            None => return Err(StatusCode::Failure.with_message("no such handle")),
        }
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.file(&handle)?;
        file.seek(io::SeekFrom::Start(offset))
            .await
            .map_err(failure)?;
        let mut data = vec![0; len.min(256 * 1024) as usize];
        let read = file.read(&mut data).await.map_err(failure)?;
        if read == 0 && !data.is_empty() {
            return Err(StatusCode::Eof.into());
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.file(&handle)?;
        file.seek(io::SeekFrom::Start(offset))
            .await
            .map_err(failure)?;
        file.write_all(&data).await.map_err(failure)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = tokio::fs::symlink_metadata(self.local(&path))
            .await
            .map_err(failure)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = tokio::fs::metadata(self.local(&path))
            .await
            .map_err(failure)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let metadata = match self.handles.get(&handle) {
            Some(Open::File(file)) => file.metadata().await,
            Some(Open::Dir { path, .. }) => tokio::fs::metadata(path).await,
            None => return Err(StatusCode::Failure.with_message("no such handle")),
        }
        .map_err(failure)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        apply(&self.local(&path), &attrs).await.map_err(failure)?;
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let file = self.file(&handle)?;
        if let Some(size) = attrs.size {
            file.set_len(size).await.map_err(failure)?;
        }
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))
                .await
                .map_err(failure)?;
        }
        if let (Some(atime), Some(mtime)) = (attrs.atime, attrs.mtime) {
            let at = |seconds: u32| {
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds.into())
            };
            let times = std::fs::FileTimes::new()
                .set_accessed(at(atime))
                .set_modified(at(mtime));
            let std_file = file.try_clone().await.map_err(failure)?.into_std().await;
            tokio::task::spawn_blocking(move || std_file.set_times(times))
                .await
                .map_err(|error| failure(io::Error::other(error)))?
                .map_err(failure)?;
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = self.local(&path);
        let metadata = tokio::fs::metadata(&path).await.map_err(failure)?;
        if !metadata.is_dir() {
            return Err(StatusCode::Failure.with_message("not a directory"));
        }
        Ok(Handle {
            id,
            handle: self.insert(Open::Dir {
                path,
                entries: None,
            }),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(Open::Dir { path, entries }) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure.with_message("no such handle"));
        };
        if entries.is_none() {
            let mut listing = tokio::fs::read_dir(&path).await.map_err(failure)?;
            let mut read = std::collections::VecDeque::new();
            while let Some(entry) = listing.next_entry().await.map_err(failure)? {
                let Ok(metadata) = entry.metadata().await else {
                    continue;
                };
                read.push_back(File::new(
                    entry.file_name().to_string_lossy(),
                    FileAttributes::from(&metadata),
                ));
            }
            *entries = Some(read);
        }
        let entries = entries.as_mut().expect("read above");
        if entries.is_empty() {
            return Err(StatusCode::Eof.into());
        }
        let count = entries.len().min(ENTRIES_PER_READ);
        Ok(Name {
            id,
            files: entries.drain(..count).collect(),
        })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        tokio::fs::remove_file(self.local(&filename))
            .await
            .map_err(failure)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.local(&path);
        tokio::fs::create_dir(&path).await.map_err(failure)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt as _;
            let _ =
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777))
                    .await;
        }
        #[cfg(not(unix))]
        let _ = attrs;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        tokio::fs::remove_dir(self.local(&path))
            .await
            .map_err(failure)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(remote(&self.local(&path)))],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        tokio::fs::rename(self.local(&oldpath), self.local(&newpath))
            .await
            .map_err(failure)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let target = tokio::fs::read_link(self.local(&path))
            .await
            .map_err(failure)?;
        Ok(Name {
            id,
            files: vec![File::dummy(target.to_string_lossy())],
        })
    }
}
