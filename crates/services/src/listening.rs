//! The TCP listeners on this machine held by the user's own processes.
//!
//! Only listeners a local page can reach are read: those bound to loopback
//! or to every interface. Processes of other users, and system executables
//! running for the user (macOS's AirPlay receiver in Control Center, a Linux
//! desktop's `libexec` daemons, Windows services), are left out. Each read is
//! one pass over the operating system's tables through its own API; nothing
//! here spawns a process or connects to a port.

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// One process listening on one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub pid: u32,
    /// The executable's file name (without `.exe` on Windows).
    pub process: String,
    /// The process's working directory, where the platform tells it cheaply
    /// (macOS and Linux).
    pub cwd: Option<PathBuf>,
    /// When the process started, comparable only between the listeners of
    /// one read: a later start is greater.
    pub started: Option<u64>,
}

/// Read the listeners now, each port once per process.
pub fn listeners() -> io::Result<Vec<Listener>> {
    let mut listeners = platform::scan()?;
    listeners.sort_by_key(|listener| (listener.port, listener.pid));
    listeners.dedup_by_key(|listener| (listener.port, listener.pid));
    Ok(listeners)
}

/// Whether a page on this machine reaches a listener bound to `ip`.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    allow(dead_code)
)]
fn reachable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback() || v4.is_unspecified(),
            None => v6.is_loopback() || v6.is_unspecified(),
        },
        ip => ip.is_loopback() || ip.is_unspecified(),
    }
}

/// Whether `executable` is part of the operating system rather than
/// something the user runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_system(executable: &Path) -> bool {
    executable.starts_with("/System")
        || executable.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("libexec" | "sbin" | "systemd")
            )
        })
}

#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    allow(dead_code)
)]
fn file_name(executable: &Path) -> Option<String> {
    let name = if cfg!(windows) {
        executable.file_stem()
    } else {
        executable.file_name()
    };
    Some(name?.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
mod platform {
    //! libproc: every pid, each one's descriptors, and each socket's TCP
    //! state. libc declares the calls and the process structures but not the
    //! socket ones, which are laid out here as `<sys/proc_info.h>` has them.
    use super::*;
    use std::ffi::{CStr, c_int, c_void};
    use std::mem::{size_of, zeroed};
    use std::net::{Ipv4Addr, Ipv6Addr};

    const PROC_PIDFDSOCKETINFO: c_int = 3;
    const SOCKINFO_TCP: c_int = 2;
    const TSI_S_LISTEN: c_int = 1;
    const INI_IPV4: u8 = 0x1;
    const INI_IPV6: u8 = 0x2;

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct SockbufInfo {
        sbi_cc: u32,
        sbi_hiwat: u32,
        sbi_mbcnt: u32,
        sbi_mbmax: u32,
        sbi_lowat: u32,
        sbi_flags: i16,
        sbi_timeo: i16,
    }

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct InSockInfo {
        insi_fport: c_int,
        insi_lport: c_int,
        insi_gencnt: u64,
        insi_flags: u32,
        insi_flow: u32,
        insi_vflag: u8,
        insi_ip_ttl: u8,
        rfu_1: u32,
        insi_faddr: [u8; 16],
        insi_laddr: [u8; 16],
        insi_v4: u8,
        insi_v6: [u32; 3],
    }

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct TcpSockInfo {
        tcpsi_ini: InSockInfo,
        tcpsi_state: c_int,
        tcpsi_timer: [c_int; 4],
        tcpsi_mss: c_int,
        tcpsi_flags: u32,
        rfu_1: u32,
        tcpsi_tp: u64,
    }

    /// `soi_proto`, whose largest member (`un_sockinfo`) is 528 bytes.
    #[repr(C)]
    union SocketProto {
        pri_tcp: std::mem::ManuallyDrop<TcpSockInfo>,
        size: [u64; 66],
    }

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct SocketInfo {
        soi_stat: libc::vinfo_stat,
        soi_so: u64,
        soi_pcb: u64,
        soi_type: c_int,
        soi_protocol: c_int,
        soi_family: c_int,
        soi_options: i16,
        soi_linger: i16,
        soi_state: i16,
        soi_qlen: i16,
        soi_incqlen: i16,
        soi_qlimit: i16,
        soi_timeo: i16,
        soi_error: u16,
        soi_oobmark: u32,
        soi_rcv: SockbufInfo,
        soi_snd: SockbufInfo,
        soi_kind: c_int,
        rfu_1: u32,
        soi_proto: SocketProto,
    }

    #[repr(C)]
    #[allow(dead_code, reason = "laid out as the kernel writes it")]
    struct SocketFdInfo {
        pfi: ProcFileInfo,
        psi: SocketInfo,
    }

    const _: () = assert!(size_of::<InSockInfo>() == 80);
    const _: () = assert!(size_of::<TcpSockInfo>() == 120);
    const _: () = assert!(std::mem::offset_of!(SocketInfo, soi_proto) == 240);
    const _: () = assert!(size_of::<SocketFdInfo>() == 792);

    pub(super) fn scan() -> io::Result<Vec<Listener>> {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let mut found = Vec::new();
        for pid in pids()? {
            let Some(info) = bsd_info(pid) else {
                continue;
            };
            if info.pbi_uid != uid {
                continue;
            }
            let ports = listening_ports(pid);
            if ports.is_empty() {
                continue;
            }
            let Some(executable) = executable(pid) else {
                continue;
            };
            if is_system(&executable) {
                continue;
            }
            let Some(process) = file_name(&executable) else {
                continue;
            };
            let cwd = cwd(pid);
            let started = info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec);
            found.extend(ports.into_iter().map(|port| Listener {
                port,
                pid: pid as u32,
                process: process.clone(),
                cwd: cwd.clone(),
                started: Some(started),
            }));
        }
        Ok(found)
    }

    fn pids() -> io::Result<Vec<c_int>> {
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        // Room for processes started between the two calls.
        let mut pids = vec![0 as c_int; count as usize + 64];
        // SAFETY: the buffer holds `pids.len()` pids, and its size is given in bytes.
        let filled = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * size_of::<c_int>()) as c_int,
            )
        };
        if filled <= 0 {
            return Err(io::Error::last_os_error());
        }
        pids.truncate(filled as usize);
        Ok(pids)
    }

    fn bsd_info(pid: c_int) -> Option<libc::proc_bsdinfo> {
        // SAFETY: plain data, filled by the kernel when the size matches.
        let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: `info` is `size` bytes.
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (read == size).then_some(info)
    }

    fn listening_ports(pid: c_int) -> Vec<u16> {
        // SAFETY: a null buffer asks only for the size.
        let needed =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let entry = size_of::<libc::proc_fdinfo>();
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(needed as usize / entry + 16);
        // SAFETY: the buffer has room for its capacity, given in bytes.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (fds.capacity() * entry) as c_int,
            )
        };
        if read <= 0 {
            return Vec::new();
        }
        // SAFETY: the kernel filled `read` bytes of whole entries.
        unsafe { fds.set_len(read as usize / entry) };
        fds.iter()
            .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32)
            .filter_map(|fd| listening_port(pid, fd.proc_fd))
            .collect()
    }

    fn listening_port(pid: c_int, fd: i32) -> Option<u16> {
        // SAFETY: plain data, filled by the kernel.
        let mut info: SocketFdInfo = unsafe { zeroed() };
        let size = size_of::<SocketFdInfo>() as c_int;
        // SAFETY: `info` is `size` bytes.
        let read = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd,
                PROC_PIDFDSOCKETINFO,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if read < size || info.psi.soi_kind != SOCKINFO_TCP {
            return None;
        }
        // SAFETY: `soi_kind` says the union holds the TCP member.
        let tcp = unsafe { &info.psi.soi_proto.pri_tcp };
        if tcp.tcpsi_state != TSI_S_LISTEN {
            return None;
        }
        let inet = &tcp.tcpsi_ini;
        let address = &inet.insi_laddr;
        let ip = if inet.insi_vflag & INI_IPV6 != 0 {
            IpAddr::V6(Ipv6Addr::from(*address))
        } else if inet.insi_vflag & INI_IPV4 != 0 {
            IpAddr::V4(Ipv4Addr::new(
                address[12],
                address[13],
                address[14],
                address[15],
            ))
        } else {
            return None;
        };
        reachable(ip).then(|| u16::from_be(inet.insi_lport as u16))
    }

    fn executable(pid: c_int) -> Option<PathBuf> {
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is as large as it says.
        let length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length <= 0 {
            return None;
        }
        path.truncate(length as usize);
        Some(PathBuf::from(String::from_utf8(path).ok()?))
    }

    fn cwd(pid: c_int) -> Option<PathBuf> {
        // SAFETY: plain data, filled by the kernel when the size matches.
        let mut info: libc::proc_vnodepathinfo = unsafe { zeroed() };
        let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
        // SAFETY: `info` is `size` bytes.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        // SAFETY: `vip_path` is one MAXPATHLEN array of chars, kept as 32
        // rows by libc.
        let path: &[u8; 1024] = unsafe { &*(&raw const info.pvi_cdir.vip_path).cast() };
        let path = CStr::from_bytes_until_nul(path).ok()?.to_str().ok()?;
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! procfs: the listening sockets in `/proc/net/tcp{,6}`, owned by the
    //! user, matched to processes by their descriptors' socket inodes.
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const LISTEN: &str = "0A";

    pub(super) fn scan() -> io::Result<Vec<Listener>> {
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let mut ports = HashMap::new();
        for (table, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
            let text = match fs::read_to_string(table) {
                Ok(text) => text,
                // A kernel without IPv6 has no tcp6 table.
                Err(_) if v6 => continue,
                Err(error) => return Err(error),
            };
            ports.extend(
                text.lines()
                    .skip(1)
                    .filter_map(|line| listening_socket(line, v6, uid)),
            );
        }
        if ports.is_empty() {
            return Ok(Vec::new());
        }
        let mut found = Vec::new();
        for entry in fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|pid| pid.parse().ok()) else {
                continue;
            };
            // Another user's descriptors are unreadable.
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            let held: Vec<u16> = fds
                .flatten()
                .filter_map(|fd| fs::read_link(fd.path()).ok())
                .filter_map(|target| {
                    let inode = target
                        .to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .parse::<u64>()
                        .ok()?;
                    ports.get(&inode).copied()
                })
                .collect();
            if held.is_empty() {
                continue;
            }
            let Ok(executable) = fs::read_link(entry.path().join("exe")) else {
                continue;
            };
            if is_system(&executable) {
                continue;
            }
            let Some(process) = file_name(&executable) else {
                continue;
            };
            let cwd = fs::read_link(entry.path().join("cwd")).ok();
            let started = start_ticks(&entry.path());
            found.extend(held.into_iter().map(|port| Listener {
                port,
                pid,
                process: process.clone(),
                cwd: cwd.clone(),
                started,
            }));
        }
        Ok(found)
    }

    /// One row of a `/proc/net/tcp` table: its socket inode and port, when
    /// it is the user's, listening where a local page reaches it.
    fn listening_socket(line: &str, v6: bool, uid: u32) -> Option<(u64, u16)> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
        if fields.len() < 10 || fields[3] != LISTEN || fields[7].parse::<u32>().ok()? != uid {
            return None;
        }
        let (address, port) = fields[1].split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        // Each 32-bit word of the address is printed as the host reads it.
        let words = (0..address.len() / 8)
            .map(|word| u32::from_str_radix(address.get(word * 8..word * 8 + 8)?, 16).ok())
            .collect::<Option<Vec<u32>>>()?;
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
        let ip = if v6 {
            IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?))
        } else {
            IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?))
        };
        let inode = fields[9].parse().ok().filter(|inode| *inode != 0)?;
        reachable(ip).then_some((inode, port))
    }

    /// The process's start, in clock ticks since boot (`stat`'s 22nd field).
    fn start_ticks(process: &Path) -> Option<u64> {
        let stat = fs::read_to_string(process.join("stat")).ok()?;
        // The name in parentheses may hold spaces; the fields after it start
        // at the third.
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19)?.parse().ok()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    //! IP Helper's TCP listener tables with their owning pids. Listeners of
    //! another logon session — services run in session 0 — and executables
    //! under the Windows directory are the system's.
    use super::*;
    use std::mem::{offset_of, size_of};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP_STATE_LISTEN, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
        MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    pub(super) fn scan() -> io::Result<Vec<Listener>> {
        let mut sockets: Vec<(u32, u16)> = Vec::new();
        let v4 = table(AF_INET.0.into())?;
        for row in rows::<MIB_TCPROW_OWNER_PID>(&v4, offset_of!(MIB_TCPTABLE_OWNER_PID, table)) {
            let ip = IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()));
            if row.dwState == MIB_TCP_STATE_LISTEN.0 as u32 && reachable(ip) {
                sockets.push((row.dwOwningPid, port(row.dwLocalPort)));
            }
        }
        let v6 = table(AF_INET6.0.into())?;
        for row in rows::<MIB_TCP6ROW_OWNER_PID>(&v6, offset_of!(MIB_TCP6TABLE_OWNER_PID, table)) {
            let ip = IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr));
            if row.dwState == MIB_TCP_STATE_LISTEN.0 as u32 && reachable(ip) {
                sockets.push((row.dwOwningPid, port(row.dwLocalPort)));
            }
        }
        // SAFETY: no preconditions.
        let session = session_of(unsafe { GetCurrentProcessId() });
        let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
        let mut processes: std::collections::HashMap<u32, Option<(String, Option<u64>)>> =
            Default::default();
        let mut found = Vec::new();
        for (pid, port) in sockets {
            let process = processes.entry(pid).or_insert_with(|| {
                if session.is_none() || session_of(pid) != session {
                    return None;
                }
                let (executable, started) = image(pid)?;
                if system_root
                    .as_ref()
                    .is_some_and(|root| starts_with_ignoring_case(&executable, root))
                {
                    return None;
                }
                Some((file_name(&executable)?, started))
            });
            if let Some((process, started)) = process {
                found.push(Listener {
                    port,
                    pid,
                    process: process.clone(),
                    cwd: None,
                    started: *started,
                });
            }
        }
        Ok(found)
    }

    /// The port in a row's `dwLocalPort`: network byte order in its low 16 bits.
    fn port(raw: u32) -> u16 {
        u16::from_be(raw as u16)
    }

    fn table(family: u32) -> io::Result<Vec<u64>> {
        let mut size = 0u32;
        let mut buffer: Vec<u64> = Vec::new();
        loop {
            let pointer = (!buffer.is_empty()).then(|| buffer.as_mut_ptr().cast());
            // SAFETY: the buffer, when given, is `size` bytes or more.
            let result = unsafe {
                GetExtendedTcpTable(
                    pointer,
                    &mut size,
                    false,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == NO_ERROR.0 && !buffer.is_empty() {
                buffer.truncate((size as usize).div_ceil(size_of::<u64>()));
                return Ok(buffer);
            }
            if result != ERROR_INSUFFICIENT_BUFFER.0 && result != NO_ERROR.0 {
                return Err(io::Error::from_raw_os_error(result as i32));
            }
            buffer = vec![0; (size as usize).div_ceil(size_of::<u64>()).max(1)];
        }
    }

    /// The rows of a `MIB_TCP*TABLE_OWNER_PID` in `table`, whose rows start at `offset`.
    fn rows<Row: Copy>(table: &[u64], offset: usize) -> Vec<Row> {
        let bytes = size_of_val(table);
        // SAFETY: the table starts with its row count.
        let count = unsafe { *table.as_ptr().cast::<u32>() } as usize;
        let count = count.min(bytes.saturating_sub(offset) / size_of::<Row>());
        // SAFETY: `count` whole rows lie inside the table, aligned as the
        // kernel wrote them.
        unsafe {
            std::slice::from_raw_parts(table.as_ptr().cast::<u8>().add(offset).cast::<Row>(), count)
        }
        .to_vec()
    }

    fn session_of(pid: u32) -> Option<u32> {
        let mut session = 0;
        // SAFETY: `session` is a valid out pointer.
        unsafe { ProcessIdToSessionId(pid, &mut session) }
            .ok()
            .map(|()| session)
    }

    /// The process's executable and when it started.
    fn image(pid: u32) -> Option<(PathBuf, Option<u64>)> {
        // SAFETY: the handle is closed below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut path = vec![0u16; 32_768];
        let mut length = path.len() as u32;
        // SAFETY: `path` holds `length` UTF-16 units.
        let named = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        };
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: four valid out pointers.
        let started = unsafe { GetProcessTimes(handle, created, exited, kernel, user) }
            .ok()
            .map(|()| {
                (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime)
            });
        // SAFETY: opened above, closed once.
        let _ = unsafe { CloseHandle(handle) };
        named.ok()?;
        path.truncate(length as usize);
        Some((PathBuf::from(String::from_utf16_lossy(&path)), started))
    }

    fn starts_with_ignoring_case(path: &Path, prefix: &Path) -> bool {
        let path = path.to_string_lossy().to_lowercase();
        let prefix = prefix.to_string_lossy().to_lowercase();
        path.strip_prefix(prefix.trim_end_matches('\\'))
            .is_some_and(|rest| rest.starts_with('\\'))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    use super::*;

    /// A phone or the browser is never a host.
    pub(super) fn scan() -> io::Result<Vec<Listener>> {
        Ok(Vec::new())
    }
}

#[cfg(all(
    test,
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn own(port: u16) -> Option<Listener> {
        listeners()
            .unwrap()
            .into_iter()
            .find(|listener| listener.port == port && listener.pid == std::process::id())
    }

    #[test]
    fn a_listener_this_process_opens_is_reported_with_its_process_until_it_closes() {
        let executable = std::env::current_exe().unwrap();
        let expected_name = if cfg!(windows) {
            executable.file_stem()
        } else {
            executable.file_name()
        }
        .unwrap()
        .to_string_lossy()
        .into_owned();
        for address in ["127.0.0.1:0", "0.0.0.0:0", "[::1]:0"] {
            let Ok(socket) = TcpListener::bind(address) else {
                // A machine without IPv6 loopback has no [::1].
                assert_eq!(address, "[::1]:0");
                continue;
            };
            let port = socket.local_addr().unwrap().port();
            let listener = own(port).unwrap_or_else(|| panic!("{address} is not reported"));
            assert_eq!(listener.process, expected_name, "{address}");
            if cfg!(any(target_os = "macos", target_os = "linux")) {
                assert_eq!(
                    listener.cwd.map(|cwd| cwd.canonicalize().unwrap()),
                    Some(std::env::current_dir().unwrap().canonicalize().unwrap()),
                    "{address}"
                );
            }
            drop(socket);
            assert_eq!(own(port), None, "{address} is still reported once closed");
        }
    }

    #[test]
    fn a_listener_on_one_outside_address_is_not_offered() {
        let Some(outside) = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| {
                socket.connect("192.0.2.1:9")?;
                socket.local_addr()
            })
            .ok()
            .map(|address| address.ip())
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        else {
            // No outside interface to bind to.
            return;
        };
        let socket = TcpListener::bind((outside, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        assert_eq!(own(port), None);
    }
}
