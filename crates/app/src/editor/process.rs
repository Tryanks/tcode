//! What the operating system says about a process: its parent, executable,
//! start time, and whether it is Zed.

use std::path::PathBuf;

pub(crate) struct ProcessInfo {
    pub(crate) parent: Option<u32>,
    pub(crate) exe: Option<PathBuf>,
    /// Opaque start-time token, equal for the same process and different for
    /// a later process reusing the pid.
    pub(crate) started: Option<u64>,
}

/// `None` when the process no longer exists.
pub(crate) fn process(pid: u32) -> Option<ProcessInfo> {
    platform::process(pid)
}

/// Whether `pid` is a Zed main process, verified by the platform's code identity.
pub(crate) fn is_zed(pid: u32) -> bool {
    platform::is_zed(pid)
}

/// This executable as `process(..).exe` reports it.
pub(crate) fn current_exe() -> Option<PathBuf> {
    process(std::process::id())?.exe
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::{c_int, c_void};

    /// Zed, Zed Preview or Zed Nightly as signed by Zed Industries' team. The
    /// identifiers leave out the team's other executables, such as Zed's `cli`.
    const ZED_REQUIREMENT: &str = "anchor apple generic \
        and certificate leaf[subject.OU] = \"MQ55VZLNZQ\" \
        and (identifier \"dev.zed.Zed\" or identifier \"dev.zed.Zed-Preview\" \
        or identifier \"dev.zed.Zed-Nightly\")";

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecGuestAttributePid: CFStringRef;
        fn SecCodeCopyGuestWithAttributes(
            host: *const c_void,
            attributes: CFDictionaryRef,
            flags: u32,
            guest: *mut *const c_void,
        ) -> i32;
        fn SecRequirementCreateWithString(
            text: CFStringRef,
            flags: u32,
            requirement: *mut *const c_void,
        ) -> i32;
        fn SecCodeCheckValidity(code: *const c_void, flags: u32, requirement: *const c_void)
        -> i32;
    }

    pub(super) fn process(pid: u32) -> Option<ProcessInfo> {
        let pid = c_int::try_from(pid).ok()?;
        // SAFETY: plain data, filled by the kernel when the size matches.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: `info` is `size` bytes.
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if read != size {
            return None;
        }
        Some(ProcessInfo {
            parent: Some(info.pbi_ppid),
            exe: executable(pid),
            started: Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec),
        })
    }

    fn executable(pid: c_int) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is as large as it says.
        let length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length <= 0 {
            return None;
        }
        path.truncate(length as usize);
        Some(PathBuf::from(std::ffi::OsString::from_vec(path)))
    }

    pub(super) fn is_zed(pid: u32) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        // SAFETY: a constant CFString owned by Security.framework.
        let key = unsafe { CFString::wrap_under_get_rule(kSecGuestAttributePid) };
        let attributes =
            CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFNumber::from(pid).as_CFType())]);
        let mut code = std::ptr::null();
        // SAFETY: a null host asks the system for the guest; `code` is a valid out pointer.
        let status = unsafe {
            SecCodeCopyGuestWithAttributes(
                std::ptr::null(),
                attributes.as_concrete_TypeRef(),
                0,
                &mut code,
            )
        };
        if status != 0 || code.is_null() {
            return false;
        }
        // SAFETY: returned +1 by a Copy function.
        let code = unsafe { CFType::wrap_under_create_rule(code) };
        let text = CFString::from_static_string(ZED_REQUIREMENT);
        let mut requirement = std::ptr::null();
        // SAFETY: `requirement` is a valid out pointer.
        let status = unsafe {
            SecRequirementCreateWithString(text.as_concrete_TypeRef(), 0, &mut requirement)
        };
        if status != 0 || requirement.is_null() {
            return false;
        }
        // SAFETY: returned +1 by a Create function.
        let requirement = unsafe { CFType::wrap_under_create_rule(requirement) };
        // SAFETY: both are live Security objects.
        unsafe { SecCodeCheckValidity(code.as_CFTypeRef(), 0, requirement.as_CFTypeRef()) == 0 }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! Linux has no platform code-signing identity, so Zed's installation
    //! layout — `<prefix>/libexec/zed-editor` beside `<prefix>/bin/zed`, as
    //! the official tarball and most packages install it — stands in for one,
    //! provided nobody but root or this user can change the executable.
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    pub(super) fn process(pid: u32) -> Option<ProcessInfo> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `comm` may itself contain spaces and parentheses.
        let fields: Vec<&str> = stat
            .get(stat.rfind(')')? + 1..)?
            .split_whitespace()
            .collect();
        // Field n of proc_pid_stat(5) is `fields[n - 3]` once `pid` and `comm` are cut.
        Some(ProcessInfo {
            parent: fields.get(1).and_then(|field| field.parse().ok()),
            exe: fs::read_link(format!("/proc/{pid}/exe")).ok(),
            started: fields.get(19).and_then(|field| field.parse().ok()),
        })
    }

    pub(super) fn is_zed(pid: u32) -> bool {
        // SAFETY: no preconditions.
        let uid = unsafe { libc::getuid() };
        if !owned_by(pid, uid) {
            return false;
        }
        let Some(exe) = process(pid)
            .and_then(|process| process.exe)
            .and_then(|exe| fs::canonicalize(exe).ok())
        else {
            return false;
        };
        let Some(libexec) = exe.parent() else {
            return false;
        };
        let Some(prefix) = libexec.parent() else {
            return false;
        };
        if exe.file_name().is_none_or(|name| name != "zed-editor")
            || libexec.file_name().is_none_or(|name| name != "libexec")
            || !prefix.join("bin").join("zed").is_file()
        {
            return false;
        }
        fs::metadata(&exe).is_ok_and(|metadata| {
            (metadata.uid() == 0 || metadata.uid() == uid) && metadata.mode() & 0o022 == 0
        })
    }

    /// Whether the process's real and effective uids are `uid`.
    fn owned_by(pid: u32, uid: libc::uid_t) -> bool {
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .is_some_and(|ids| {
                let mut ids = ids.split_whitespace().map(str::parse::<libc::uid_t>);
                matches!(
                    (ids.next(), ids.next()),
                    (Some(Ok(real)), Some(Ok(effective))) if real == uid && effective == uid
                )
            })
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use windows::Win32::Foundation::{FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Cryptography::{
        CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_ORGANIZATION_NAME,
    };
    use windows::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY,
        WTD_UI_NONE, WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain,
        WTHelperProvDataFromStateData, WinVerifyTrust,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::{PCWSTR, PWSTR};

    /// The organization in the certificate Zed Industries signs Zed with.
    const ZED_SIGNER: &str = "Zed Industries Inc";

    pub(super) fn process(pid: u32) -> Option<ProcessInfo> {
        let parent = parent(pid)?;
        let (exe, started) = image(pid).unwrap_or_default();
        Some(ProcessInfo {
            parent: Some(parent),
            exe,
            started,
        })
    }

    /// The parent pid the system recorded at creation; `None` when no
    /// process has `pid`.
    fn parent(pid: u32) -> Option<u32> {
        // SAFETY: no preconditions.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
        // SAFETY: a fresh handle owned by nothing else.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.0) };
        let snapshot = HANDLE(snapshot.as_raw_handle());
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry` is a sized PROCESSENTRY32W.
        let mut found = unsafe { Process32FirstW(snapshot, &mut entry) };
        while found.is_ok() {
            if entry.th32ProcessID == pid {
                return Some(entry.th32ParentProcessID);
            }
            // SAFETY: as above.
            found = unsafe { Process32NextW(snapshot, &mut entry) };
        }
        None
    }

    fn image(pid: u32) -> Option<(Option<PathBuf>, Option<u64>)> {
        // SAFETY: no preconditions.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        // SAFETY: a fresh handle owned by nothing else.
        let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
        let process = HANDLE(process.as_raw_handle());
        let mut path = vec![0u16; 32_768];
        let mut length = path.len() as u32;
        // SAFETY: `path` holds `length` UTF-16 units.
        let exe = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        }
        .ok()
        .map(|()| {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]))
        });
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: four valid out pointers.
        let started = unsafe { GetProcessTimes(process, created, exited, kernel, user) }
            .ok()
            .map(|()| {
                (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime)
            });
        Some((exe, started))
    }

    pub(super) fn is_zed(pid: u32) -> bool {
        process(pid)
            .and_then(|process| process.exe)
            .and_then(|exe| signer_organization(&exe))
            .is_some_and(|organization| organization == ZED_SIGNER)
    }

    /// The organization of the certificate that signed `exe`, when its
    /// Authenticode signature verifies.
    fn signer_organization(exe: &Path) -> Option<String> {
        let path: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let mut file = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: PCWSTR(path.as_ptr()),
            ..Default::default()
        };
        let mut data = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
            dwStateAction: WTD_STATEACTION_VERIFY,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let no_window = HWND(INVALID_HANDLE_VALUE.0);
        // SAFETY: `data` and the file info and path it points to outlive the call.
        let verified =
            unsafe { WinVerifyTrust(no_window, &mut action, (&raw mut data).cast()) } == 0;
        // SAFETY: the state data stays valid until it is closed below.
        let organization = verified
            .then(|| unsafe { leaf_organization(data.hWVTStateData) })
            .flatten();
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        // SAFETY: closes the state the verification opened.
        unsafe { WinVerifyTrust(no_window, &mut action, (&raw mut data).cast()) };
        organization
    }

    /// # Safety
    /// `state` is the open state data of a successful verification.
    unsafe fn leaf_organization(state: HANDLE) -> Option<String> {
        // SAFETY: per the caller; each pointer is checked before use.
        unsafe {
            let provider = WTHelperProvDataFromStateData(state);
            if provider.is_null() {
                return None;
            }
            let signer = WTHelperGetProvSignerFromChain(provider, 0, false, 0);
            if signer.is_null() {
                return None;
            }
            let certificate = WTHelperGetProvCertFromChain(signer, 0);
            if certificate.is_null() || (*certificate).pCert.is_null() {
                return None;
            }
            let context = (*certificate).pCert;
            let oid = Some(szOID_ORGANIZATION_NAME.0.cast());
            let length = CertGetNameStringW(context, CERT_NAME_ATTR_TYPE, 0, oid, None);
            if length <= 1 {
                return None;
            }
            let mut name = vec![0u16; length as usize];
            let length = CertGetNameStringW(context, CERT_NAME_ATTR_TYPE, 0, oid, Some(&mut name));
            String::from_utf16(&name[..(length as usize).saturating_sub(1)]).ok()
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use super::*;

    pub(super) fn process(_pid: u32) -> Option<ProcessInfo> {
        None
    }

    pub(super) fn is_zed(_pid: u32) -> bool {
        false
    }
}
