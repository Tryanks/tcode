//! The local endpoint through which `tcode ssh-proxy` reaches the running app:
//! a Unix socket inside the data directory, or a named pipe on Windows. Only
//! the current user can connect.

use std::io::{self, Read, Write};
use std::path::Path;

pub(crate) use platform::*;

impl Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self).read(buf)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs::{self, DirBuilder, Permissions};
    use std::net::Shutdown;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

    pub(crate) struct Listener {
        listener: UnixListener,
    }

    impl Listener {
        /// Bind the endpoint that belongs to `data_dir`, replacing a stale one
        /// left by an earlier run.
        pub(crate) fn bind(data_dir: &Path) -> io::Result<Listener> {
            let directory = data_dir.join("ssh");
            match DirBuilder::new().mode(0o700).create(&directory) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    fs::set_permissions(&directory, Permissions::from_mode(0o700))?;
                }
                result => result?,
            }
            let (path, address) = address(data_dir)?;
            let listener = match UnixListener::bind_addr(&address) {
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    if UnixStream::connect_addr(&address).is_ok() {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("another Tcode is listening at {}", path.display()),
                        ));
                    }
                    fs::remove_file(&path)?;
                    UnixListener::bind_addr(&address)?
                }
                result => result?,
            };
            // Until this narrows it, the socket's mode comes from the umask;
            // the 0700 directory keeps other users out in the meantime.
            fs::set_permissions(&path, Permissions::from_mode(0o600))?;
            Ok(Listener { listener })
        }

        /// The connection may already be closed: another Tcode starting on
        /// the same data directory connects once to learn this one is alive.
        pub(crate) fn accept(&self) -> io::Result<LocalStream> {
            let (stream, _) = self.listener.accept()?;
            Ok(LocalStream { stream })
        }
    }

    fn address(data_dir: &Path) -> io::Result<(std::path::PathBuf, SocketAddr)> {
        let path = data_dir.join("ssh").join("proxy.sock");
        let address = SocketAddr::from_pathname(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{} cannot be a socket address: {error}", path.display()),
            )
        })?;
        Ok((path, address))
    }

    pub(crate) struct LocalStream {
        stream: UnixStream,
    }

    impl LocalStream {
        pub(crate) fn connect(data_dir: &Path) -> io::Result<LocalStream> {
            let (_, address) = address(data_dir)?;
            Ok(LocalStream {
                stream: UnixStream::connect_addr(&address)?,
            })
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        pub(crate) fn peer_pid(&self) -> io::Result<u32> {
            use std::os::fd::AsRawFd;
            // SAFETY: plain data, filled by the kernel.
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length = size_of::<libc::ucred>() as libc::socklen_t;
            // SAFETY: `credentials` is `length` bytes.
            let result = unsafe {
                libc::getsockopt(
                    self.stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&raw mut credentials).cast(),
                    &mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            u32::try_from(credentials.pid).map_err(io::Error::other)
        }

        #[cfg(target_os = "macos")]
        pub(crate) fn peer_pid(&self) -> io::Result<u32> {
            use std::os::fd::AsRawFd;
            let mut pid: libc::pid_t = 0;
            let mut length = size_of::<libc::pid_t>() as libc::socklen_t;
            // SAFETY: `pid` is `length` bytes.
            let result = unsafe {
                libc::getsockopt(
                    self.stream.as_raw_fd(),
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&raw mut pid).cast(),
                    &mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            u32::try_from(pid).map_err(io::Error::other)
        }

        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
        pub(crate) fn peer_pid(&self) -> io::Result<u32> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub(crate) fn try_clone(&self) -> io::Result<LocalStream> {
            Ok(LocalStream {
                stream: self.stream.try_clone()?,
            })
        }

        /// End the connection in both directions, so a thread blocked
        /// reading or writing it on either side returns.
        pub(crate) fn shutdown(&self) {
            let _ = self.stream.shutdown(Shutdown::Both);
        }
    }

    impl Read for &LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&self.stream).read(buf)
        }
    }

    impl Write for &LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&self.stream).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            (&self.stream).flush()
        }
    }
}

#[cfg(windows)]
mod platform {
    //! The pipe is opened for overlapped I/O even though every call here
    //! blocks: the I/O manager serializes synchronous operations on one file
    //! object, so a read pending in one thread would hold back a write, a
    //! disconnect or a cancellation from another.
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, WIN32_ERROR,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT, WriteFile,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT, WaitNamedPipeW,
    };
    use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    const BUFFER: u32 = 64 * 1024;

    pub(crate) struct Listener {
        name: Vec<u16>,
        security: SecurityDescriptor,
        pending: Mutex<Option<OwnedHandle>>,
    }

    impl Listener {
        /// Create the first instance of the pipe that belongs to `data_dir`;
        /// a pipe of that name held by anyone else fails with `AddrInUse`.
        pub(crate) fn bind(data_dir: &Path) -> io::Result<Listener> {
            let name = pipe_name(data_dir)?;
            let security = SecurityDescriptor::current_user_only()?;
            let first = instance(&name, &security, true).map_err(|error| {
                if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("{} is already open", String::from_utf16_lossy(&name)),
                    )
                } else {
                    error
                }
            })?;
            Ok(Listener {
                name,
                security,
                pending: Mutex::new(Some(first)),
            })
        }

        pub(crate) fn accept(&self) -> io::Result<LocalStream> {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let pipe = match pending {
                Some(pipe) => pipe,
                None => instance(&self.name, &self.security, false)?,
            };
            let handle = raw(&pipe);
            let connected = match overlapped(handle, None, |overlapped| {
                // SAFETY: the handle is open and `overlapped` outlives the call.
                unsafe { ConnectNamedPipe(handle, Some(overlapped)) }
            }) {
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED.0 as i32) => Ok(()),
                result => result.map(drop),
            };
            // The next instance exists before this one can close, so the name
            // never lapses for another process to take.
            *self.pending.lock().unwrap_or_else(PoisonError::into_inner) =
                instance(&self.name, &self.security, false).ok();
            connected?;
            Ok(LocalStream::new(pipe, true))
        }
    }

    fn instance(
        name: &[u16],
        security: &SecurityDescriptor,
        first: bool,
    ) -> io::Result<OwnedHandle> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0.0,
            bInheritHandle: false.into(),
        };
        let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` is NUL-terminated and `attributes` outlives the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                Some(&attributes),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle owned by nothing else.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    /// `\\.\pipe\tcode-ssh-<hash of the canonical data directory>`, NUL-terminated.
    fn pipe_name(data_dir: &Path) -> io::Result<Vec<u16>> {
        let directory = std::fs::canonicalize(data_dir)?;
        let directory = directory.to_string_lossy().to_lowercase();
        // FNV-1a: every Tcode build must derive the same name, which std's
        // hasher does not promise.
        let hash = directory
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
            });
        Ok(format!(r"\\.\pipe\tcode-ssh-{hash:016x}")
            .encode_utf16()
            .chain([0])
            .collect())
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    // SAFETY: the descriptor is immutable after creation and only read by the
    // kernel.
    unsafe impl Send for SecurityDescriptor {}
    // SAFETY: as above.
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        /// A protected DACL granting everything to this process's user and
        /// nothing to anyone else.
        fn current_user_only() -> io::Result<SecurityDescriptor> {
            let mut token = HANDLE::default();
            // SAFETY: the pseudo handle needs no closing; `token` is a valid out pointer.
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
                .map_err(os_error)?;
            // SAFETY: a fresh handle owned by nothing else.
            let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
            let mut size = 0u32;
            // SAFETY: a null buffer asks only for the size.
            let _ = unsafe { GetTokenInformation(raw(&token), TokenUser, None, 0, &mut size) };
            let mut buffer = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
            // SAFETY: the buffer is `size` bytes or more, aligned for `TOKEN_USER`.
            unsafe {
                GetTokenInformation(
                    raw(&token),
                    TokenUser,
                    Some(buffer.as_mut_ptr().cast()),
                    size,
                    &mut size,
                )
            }
            .map_err(os_error)?;
            // SAFETY: filled above with a `TOKEN_USER` and the SID it points into.
            let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
            let mut sid = PWSTR::null();
            // SAFETY: the SID lives in `buffer`; `sid` is a valid out pointer.
            unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }.map_err(os_error)?;
            // SAFETY: a NUL-terminated string allocated by the call above.
            let sid_text = unsafe { sid.to_string() };
            // SAFETY: allocated with LocalAlloc by ConvertSidToStringSidW, freed once.
            unsafe { LocalFree(Some(HLOCAL(sid.0.cast()))) };
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", sid_text.map_err(io::Error::other)?)
                .encode_utf16()
                .chain([0])
                .collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            // SAFETY: `sddl` is NUL-terminated; `descriptor` is a valid out pointer.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    None,
                )
            }
            .map_err(os_error)?;
            Ok(SecurityDescriptor(descriptor))
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated with LocalAlloc by the conversion, freed once.
            unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }

    struct Pipe {
        handle: OwnedHandle,
        server: bool,
        shut: AtomicBool,
    }

    pub(crate) struct LocalStream {
        pipe: Arc<Pipe>,
    }

    impl LocalStream {
        fn new(handle: OwnedHandle, server: bool) -> LocalStream {
            LocalStream {
                pipe: Arc::new(Pipe {
                    handle,
                    server,
                    shut: AtomicBool::new(false),
                }),
            }
        }

        pub(crate) fn connect(data_dir: &Path) -> io::Result<LocalStream> {
            let name = pipe_name(data_dir)?;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // SAFETY: `name` is NUL-terminated.
                let opened = unsafe {
                    CreateFileW(
                        PCWSTR(name.as_ptr()),
                        (GENERIC_READ | GENERIC_WRITE).0,
                        FILE_SHARE_NONE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                        None,
                    )
                };
                match opened {
                    Ok(handle) => {
                        // SAFETY: a fresh handle owned by nothing else.
                        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
                        return Ok(LocalStream::new(handle, false));
                    }
                    Err(error)
                        if error.code() == ERROR_PIPE_BUSY.to_hresult()
                            && Instant::now() < deadline =>
                    {
                        // SAFETY: `name` is NUL-terminated.
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 500) };
                    }
                    Err(error) => return Err(os_error(error)),
                }
            }
        }

        pub(crate) fn peer_pid(&self) -> io::Result<u32> {
            let mut pid = 0;
            // SAFETY: the handle is open; `pid` is a valid out pointer.
            unsafe { GetNamedPipeClientProcessId(self.handle(), &mut pid) }.map_err(os_error)?;
            Ok(pid)
        }

        pub(crate) fn try_clone(&self) -> io::Result<LocalStream> {
            Ok(LocalStream {
                pipe: self.pipe.clone(),
            })
        }

        /// End the connection in both directions, so a thread blocked
        /// reading or writing it on either side returns. A client's peer sees
        /// the end once the client's last clone is dropped.
        pub(crate) fn shutdown(&self) {
            self.pipe.shut.store(true, Ordering::SeqCst);
            // SAFETY: the handle is open.
            let _ = unsafe { CancelIoEx(self.handle(), None) };
            if self.pipe.server {
                // SAFETY: the handle is a server end, open.
                let _ = unsafe { DisconnectNamedPipe(self.handle()) };
            }
        }

        fn handle(&self) -> HANDLE {
            raw(&self.pipe.handle)
        }
    }

    impl Read for &LocalStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pipe.shut.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let handle = self.handle();
            let length = buf.len().min(u32::MAX as usize);
            let read = overlapped(handle, Some(&self.pipe.shut), |overlapped| {
                // SAFETY: `buf` and `overlapped` outlive the operation, which
                // `overlapped` waits for.
                unsafe { ReadFile(handle, Some(&mut buf[..length]), None, Some(overlapped)) }
            });
            match read {
                Ok(read) => Ok(read as usize),
                Err(error)
                    if [
                        ERROR_BROKEN_PIPE,
                        ERROR_PIPE_NOT_CONNECTED,
                        ERROR_OPERATION_ABORTED,
                    ]
                    .iter()
                    .any(|code| error.raw_os_error() == Some(code.0 as i32)) =>
                {
                    Ok(0)
                }
                Err(error) => Err(error),
            }
        }
    }

    impl Write for &LocalStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.pipe.shut.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let handle = self.handle();
            let length = buf.len().min(u32::MAX as usize);
            overlapped(handle, Some(&self.pipe.shut), |overlapped| {
                // SAFETY: `buf` and `overlapped` outlive the operation, which
                // `overlapped` waits for.
                unsafe { WriteFile(handle, Some(&buf[..length]), None, Some(overlapped)) }
            })
            .map(|written| written as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Start one overlapped operation on `handle` and wait for it to finish.
    /// When `shut` is set by the time the operation is pending, it is
    /// cancelled, closing the gap between a shutdown and an operation that
    /// started after its cancellation.
    fn overlapped(
        handle: HANDLE,
        shut: Option<&AtomicBool>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<u32> {
        // SAFETY: an unnamed manual-reset event.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(os_error)?;
        // SAFETY: a fresh handle owned by nothing else.
        let event = unsafe { OwnedHandle::from_raw_handle(event.0) };
        let mut overlapped = OVERLAPPED {
            hEvent: raw(&event),
            ..Default::default()
        };
        if let Err(error) = start(&mut overlapped) {
            if error.code() != ERROR_IO_PENDING.to_hresult() {
                return Err(os_error(error));
            }
            if shut.is_some_and(|shut| shut.load(Ordering::SeqCst)) {
                // SAFETY: cancels only this operation, which is still waited for below.
                let _ = unsafe { CancelIoEx(handle, Some(&overlapped)) };
            }
        }
        let mut transferred = 0;
        // SAFETY: `overlapped` belongs to an operation started on `handle`.
        unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, true) }
            .map_err(os_error)?;
        Ok(transferred)
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle())
    }

    fn os_error(error: windows::core::Error) -> io::Error {
        match WIN32_ERROR::from_error(&error) {
            Some(code) => io::Error::from_raw_os_error(code.0 as i32),
            None => io::Error::other(error),
        }
    }
}
