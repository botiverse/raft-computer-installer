//! Process identity comes from the OS executable path and creation identity,
//! never a name, command-line substring, or an unqualified persisted PID.
use crate::{Error, Result};
use k_carrier::error::invalid;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub pid: u32,
    pub executable: PathBuf,
    pub created: String,
}

pub fn same_instance(left: &Identity, right: &Identity) -> bool {
    left.pid == right.pid && left.created == right.created
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub fn immediate_parent(binary: &Path) -> Result<Option<Identity>> {
    let first = native::parent()?;
    let identity = observe(first)?;
    if first != native::parent()? {
        return Err(Error::Uncertain(
            "parent process changed during attestation".into(),
        ));
    }
    #[cfg(windows)]
    if let Some(parent) = &identity {
        let own =
            observe(std::process::id())?.ok_or_else(|| invalid("process identity unavailable"))?;
        let ticks = |created: &str| -> Result<u64> {
            let (high, low) = created
                .split_once(':')
                .ok_or_else(|| invalid("invalid creation time"))?;
            let high = high
                .parse::<u32>()
                .map_err(|_| invalid("invalid creation time"))?;
            let low = low
                .parse::<u32>()
                .map_err(|_| invalid("invalid creation time"))?;
            Ok((u64::from(high) << 32) | u64::from(low))
        };
        if ticks(&parent.created)? > ticks(&own.created)? {
            return Err(Error::Uncertain("parent process id was reused".into()));
        }
    }
    Ok(identity.filter(|p| same_path(&p.executable, binary)))
}

pub fn observe(pid: u32) -> Result<Option<Identity>> {
    if pid <= 1 {
        return Ok(None);
    }
    native::observe(pid)
}

pub fn instance_matches(identity: &Identity) -> Result<bool> {
    match observe(identity.pid)? {
        Some(current) => Ok(same_instance(identity, &current)),
        None if native::definitely_exited(identity.pid)? => Ok(false),
        None => Err(Error::Uncertain(
            "recorded caller identity is no longer observable".into(),
        )),
    }
}

pub fn matches(identity: &Identity) -> Result<bool> {
    match observe(identity.pid)? {
        Some(current) => Ok(current.created == identity.created
            && same_path(&current.executable, &identity.executable)),
        None if native::definitely_exited(identity.pid)? => Ok(false),
        None => Err(Error::Uncertain(
            "recorded process identity is no longer observable".into(),
        )),
    }
}

pub fn attest(pid: u32, binary: &Path) -> Result<Identity> {
    observe(pid)?
        .filter(|p| same_path(&p.executable, binary))
        .ok_or_else(|| invalid("product attestation does not identify a live installed executable"))
}

pub fn installed(binary: &Path) -> Result<Vec<Identity>> {
    let mut identities = Vec::new();
    for pid in native::pids()? {
        if let Some(identity) = observe(pid)?
            && same_path(&identity.executable, binary)
        {
            identities.push(identity);
        }
    }
    identities.sort_by_key(|p| p.pid);
    Ok(identities)
}

pub async fn wait_gone(identities: &[Identity], budget: Duration) -> Result<Vec<Identity>> {
    let deadline = Instant::now() + budget;
    loop {
        let mut left = Vec::new();
        for identity in identities {
            match matches(identity) {
                Ok(false) => {}
                // During exit the OS can remove the executable mapping before
                // exposing a zombie/exited state. Keep waiting within the
                // existing budget; unobservable never means gone or safe to kill.
                Ok(true) | Err(Error::Uncertain(_)) => left.push(identity.clone()),
                Err(error) => return Err(error),
            }
        }
        if left.is_empty() || Instant::now() >= deadline {
            return Ok(left);
        }
        sleep(Duration::from_millis(100)).await;
    }
}

pub async fn terminate(identities: &[Identity]) -> Result<Vec<u32>> {
    let mut forced = Vec::new();
    for identity in identities {
        if matches(identity)? {
            native::signal(identity, false)?;
            forced.push(identity.pid);
        }
    }
    let remaining = wait_gone(identities, Duration::from_secs(15)).await?;
    for identity in &remaining {
        native::signal(identity, true)?;
    }
    if !wait_gone(&remaining, Duration::from_secs(5))
        .await?
        .is_empty()
    {
        return Err(Error::Uncertain(
            "product processes survived termination".into(),
        ));
    }
    Ok(forced)
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::{Child, Command, Stdio};

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn unobservable_live_process_does_not_block_inventory_or_prove_exit() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("reader.c");
        let executable = temporary.path().join("reader");
        fs::write(&source, b"#include <sys/prctl.h>\n#include <unistd.h>\nint main(void) { char b; while (read(0, &b, 1) == 1) { if ((b == '0' || b == '1') && prctl(PR_SET_DUMPABLE, b - '0') != 0) return 1; if (write(1, &b, 1) != 1) return 1; } return 0; }\n").unwrap();
        let built = Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let mut child = OwnedChild(
            Command::new(&executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        fn exchange(child: &mut Child, value: u8) {
            child.stdin.as_mut().unwrap().write_all(&[value]).unwrap();
            let mut reply = [0];
            child
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut reply)
                .unwrap();
            assert_eq!(reply, [value]);
        }
        exchange(&mut child.0, b'1');
        let identity = attest(child.0.id(), &executable).unwrap();
        exchange(&mut child.0, b'0');
        // Reproduce a live same-user service whose executable cannot be read.
        assert_eq!(
            fs::read_link(format!("/proc/{}/exe", child.0.id()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(observe(identity.pid).unwrap().is_none());
        assert!(installed(&executable).unwrap().is_empty());
        assert!(matches!(matches(&identity), Err(Error::Uncertain(_))));
        assert_eq!(
            wait_gone(std::slice::from_ref(&identity), Duration::from_millis(50))
                .await
                .unwrap(),
            vec![identity.clone()]
        );
        assert!(native::signal(&identity, true).is_err());
        exchange(&mut child.0, b'1');
        assert!(matches(&identity).unwrap());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!matches(&identity).unwrap());
    }
}

#[cfg(target_os = "linux")]
mod native {
    use super::*;
    use std::{
        ffi::OsString,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{
                ffi::{OsStrExt, OsStringExt},
                fs::MetadataExt,
            },
        },
    };

    fn read_stat(pid: u32) -> Result<Option<String>> {
        let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let fields = text
            .rsplit_once(") ")
            .ok_or_else(|| invalid("invalid process identity"))?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        if matches!(fields.first(), Some(&"Z" | &"X")) {
            return Ok(None);
        }
        let start = fields
            .get(19)
            .ok_or_else(|| invalid("missing process creation identity"))?;
        if !start.bytes().all(|c| c.is_ascii_digit()) {
            return Err(invalid("invalid process creation identity"));
        }
        // /proc start ticks are scoped to the current boot; the boot UUID makes
        // a record safe to inspect after reboot as well as PID reuse.
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(Some(format!("{}:{start}", boot.trim())))
    }

    pub fn parent() -> Result<u32> {
        Ok(unsafe { libc::getppid() } as u32)
    }

    pub fn definitely_exited(pid: u32) -> Result<bool> {
        Ok(read_stat(pid)?.is_none())
    }

    pub fn observe(pid: u32) -> Result<Option<Identity>> {
        let root = PathBuf::from(format!("/proc/{pid}"));
        match fs::metadata(&root) {
            Ok(meta) if meta.uid() != unsafe { libc::geteuid() } => return Ok(None),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let Some(created) = read_stat(pid)? else {
            return Ok(None);
        };
        let executable = match fs::read_link(root.join("exe")) {
            Ok(path) => {
                let raw = path.as_os_str().as_bytes();
                PathBuf::from(OsString::from_vec(
                    raw.strip_suffix(b" (deleted)").unwrap_or(raw).to_vec(),
                ))
            }
            // Same-user system services may deny /proc/<pid>/exe (for
            // example a non-dumpable PAM session). They are not identifiable
            // installation candidates. Recorded identities remain fail-closed:
            // matches() separately requires proof of exit when observe is None.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if read_stat(pid)?.as_ref() != Some(&created) {
            return Ok(None);
        }
        Ok(Some(Identity {
            pid,
            executable,
            created,
        }))
    }

    pub fn pids() -> Result<Vec<u32>> {
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc")? {
            if let Some(pid) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    pub fn signal(identity: &Identity, force: bool) -> Result<()> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(Error::Uncertain(
                "cannot retain product process identity for termination".into(),
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        if !matches(identity)? {
            return Ok(());
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{
        ffi::OsString,
        mem::{MaybeUninit, size_of},
        os::unix::ffi::OsStringExt,
    };

    fn info(pid: u32) -> Result<Option<libc::proc_bsdinfo>> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if count == 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EPERM)) {
                return Ok(None);
            }
            return Err(error.into());
        }
        if count as usize != size_of::<libc::proc_bsdinfo>() {
            return Err(invalid("incomplete process identity"));
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_uid != unsafe { libc::geteuid() } || info.pbi_status == 5 {
            return Ok(None);
        }
        Ok(Some(info))
    }

    pub fn parent() -> Result<u32> {
        Ok(unsafe { libc::getppid() } as u32)
    }

    pub fn definitely_exited(pid: u32) -> Result<bool> {
        if !k_carrier::lock::process_alive(pid) {
            return Ok(true);
        }
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                // XNU searches the zombie list only when arg is nonzero.
                // Require an explicit exited state, independent of parent reap.
                1,
                info.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        Ok(count as usize == size_of::<libc::proc_bsdinfo>()
            && unsafe { info.assume_init().pbi_status } == 5)
    }

    pub fn observe(pid: u32) -> Result<Option<Identity>> {
        if pid > i32::MAX as u32 {
            return Ok(None);
        }
        let Some(first) = info(pid)? else {
            return Ok(None);
        };
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let count =
            unsafe { libc::proc_pidpath(pid as i32, path.as_mut_ptr().cast(), path.len() as u32) };
        if count <= 0 {
            let error = std::io::Error::last_os_error();
            // An unlinked executable or an inaccessible unrelated process is
            // not an identifiable candidate. For recorded identities matches()
            // still requires independent proof of exit, so it stays uncertain.
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ESRCH | libc::EPERM | libc::EACCES)
            ) {
                return Ok(None);
            }
            if info(pid)?.is_none() {
                return Ok(None);
            }
            return Err(error.into());
        }
        path.truncate(path.iter().position(|c| *c == 0).unwrap_or(path.len()));
        let Some(last) = info(pid)? else {
            return Ok(None);
        };
        if first.pbi_start_tvsec != last.pbi_start_tvsec
            || first.pbi_start_tvusec != last.pbi_start_tvusec
        {
            return Ok(None);
        }
        Ok(Some(Identity {
            pid,
            executable: PathBuf::from(OsString::from_vec(path)),
            created: format!("{}:{}", first.pbi_start_tvsec, first.pbi_start_tvusec),
        }))
    }

    pub fn pids() -> Result<Vec<u32>> {
        // libproc's PROC_ALL_PIDS enumerates identities, not command-line text.
        let needed = unsafe { libc::proc_listpids(1, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut pids = vec![0i32; needed as usize / size_of::<i32>() + 1024];
        loop {
            let capacity = pids.len() * size_of::<i32>();
            let count =
                unsafe { libc::proc_listpids(1, 0, pids.as_mut_ptr().cast(), capacity as i32) };
            if count < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if count as usize >= capacity {
                if pids.len() > 1_000_000 {
                    return Err(invalid("process listing too large"));
                }
                pids.resize(pids.len() * 2, 0);
                continue;
            }
            pids.truncate(count as usize / size_of::<i32>());
            return Ok(pids
                .into_iter()
                .filter(|p| *p > 1)
                .map(|p| p as u32)
                .collect());
        }
    }

    pub fn signal(identity: &Identity, force: bool) -> Result<()> {
        // macOS has no pidfd: recheck OS start time and executable immediately
        // before each signal. A stale persisted PID alone never authorizes it.
        if !matches(identity)? {
            return Ok(());
        }
        if unsafe {
            libc::kill(
                identity.pid as i32,
                if force { libc::SIGKILL } else { libc::SIGTERM },
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{ffi::OsString, mem::size_of, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn definitely_exited(pid: u32) -> Result<bool> {
        Ok(!k_carrier::lock::process_alive(pid))
    }

    fn identity(handle: HANDLE, pid: u32) -> Result<Option<Identity>> {
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 => return Ok(None),
            WAIT_TIMEOUT => {}
            _ => return Err(std::io::Error::last_os_error().into()),
        }
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        path.truncate(length as usize);
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Some(Identity {
            pid,
            executable: PathBuf::from(OsString::from_wide(&path)),
            created: format!("{}:{}", created.dwHighDateTime, created.dwLowDateTime),
        }))
    }

    pub fn observe(pid: u32) -> Result<Option<Identity>> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Ok(None);
        } // Exited or outside this user's permissions.
        let handle = Handle(handle);
        match identity(handle.0, pid) {
            // Windows system/pseudo processes can be opened but expose no
            // executable (ACCESS_DENIED / GEN_FAILURE). Do not abort inventory
            // of our installation because an unrelated process is protected.
            Err(Error::Io(error)) if matches!(error.raw_os_error(), Some(5 | 31)) => Ok(None),
            result => result,
        }
    }

    pub fn parent() -> Result<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = Handle(snapshot);
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while found != 0 {
            if entry.th32ProcessID == std::process::id() {
                return Ok(entry.th32ParentProcessID);
            }
            found = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        Err(Error::Uncertain("parent process unavailable".into()))
    }

    pub fn pids() -> Result<Vec<u32>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = Handle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut result = Vec::new();
        let mut found = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while found != 0 {
            result.push(entry.th32ProcessID);
            found = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        Ok(result)
    }

    pub fn signal(expected: &Identity, _force: bool) -> Result<()> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                expected.pid,
            )
        };
        if handle.is_null() {
            if !matches(expected)? {
                return Ok(());
            }
            return Err(Error::Uncertain(
                "cannot retain product process handle".into(),
            ));
        }
        let handle = Handle(handle);
        let Some(current) = identity(handle.0, expected.pid)? else {
            return Ok(());
        };
        if current.created != expected.created
            || !same_path(&current.executable, &expected.executable)
        {
            return Ok(());
        }
        // Windows has no SIGTERM equivalent for a detached native service.
        // The graceful product stop command has already run before this fallback.
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::process::{Child, Command, Stdio};

    #[test]
    fn protected_process_child() {
        if std::env::var_os("RCI_PROTECTED_PROCESS_TEST").is_none() {
            return;
        }
        println!("ready");
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) }, 0);
        println!("protected");
        std::io::stdin().read_exact(&mut byte).unwrap();
    }

    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn inaccessible_process_is_not_a_candidate_or_proven_exited() {
        let mut child = Reap(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::tests::protected_process_child",
                    "--nocapture",
                ])
                .env("RCI_PROTECTED_PROCESS_TEST", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut output = std::io::BufReader::new(child.0.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("ready") {
            line.clear();
            assert_ne!(output.read_line(&mut line).unwrap(), 0);
        }
        let identity = observe(child.0.id()).unwrap().unwrap();
        child.0.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        assert!(line.contains("protected"));
        assert!(observe(child.0.id()).unwrap().is_none());
        assert!(
            installed(&identity.executable)
                .unwrap()
                .iter()
                .all(|p| p.pid != child.0.id())
        );
        assert!(matches!(matches(&identity), Err(Error::Uncertain(_))));
        let remaining = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(wait_gone(
                std::slice::from_ref(&identity),
                Duration::from_millis(20),
            ))
            .unwrap();
        assert_eq!(remaining, vec![identity]);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::{Child, Command, Stdio};

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn terminated_child_need_not_be_reaped_to_prove_exit() {
        let child = OwnedChild(Command::new("/bin/sleep").arg("60").spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Ok(identity) = attest(child.0.id(), Path::new("/bin/sleep")) {
                break identity;
            }
            assert!(Instant::now() < deadline, "child did not become observable");
            sleep(Duration::from_millis(20)).await;
        };
        // Keep the Child unreaped until Drop, as a waiting CLI owned by another
        // process would be. terminate() must finish without that parent's help.
        terminate(std::slice::from_ref(&identity)).await.unwrap();
        assert!(!matches(&identity).unwrap());
    }

    #[tokio::test]
    async fn unlinked_live_executable_does_not_block_inventory_or_prove_exit() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("reader");
        let source = temporary.path().join("reader.c");
        fs::write(&source, b"#include <unistd.h>\nint main(void) { char b[64]; ssize_t n; while ((n = read(0, b, sizeof b)) > 0) { if (write(1, b, n) != n) return 1; } return n < 0; }\n").unwrap();
        // Build an ordinary owned executable: resigning a copied Apple system
        // binary is not portable across supported macOS runner versions.
        let build = Command::new("/usr/bin/cc")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&executable)
                .output()
                .unwrap()
                .status
                .success()
        );
        let mut child = OwnedChild(
            Command::new(&executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        fn exchange(child: &mut Child) {
            child.stdin.as_mut().unwrap().write_all(b"ready\n").unwrap();
            let mut output = [0; 6];
            child
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut output)
                .unwrap_or_else(|error| {
                    panic!(
                        "fixture stopped responding: {error}; exit: {:?}",
                        child.try_wait()
                    )
                });
            assert_eq!(&output, b"ready\n");
        }
        exchange(&mut child.0); // The executable is loaded and accepting input.
        let deadline = Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Ok(identity) = attest(child.0.id(), &executable) {
                break identity;
            }
            assert!(Instant::now() < deadline, "child did not become observable");
            sleep(Duration::from_millis(20)).await;
        };
        assert!(installed(&executable).unwrap().contains(&identity));
        fs::remove_file(&executable).unwrap();
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(
            observe(identity.pid).unwrap().is_none(),
            "macOS loses the unlinked executable path"
        );
        // The same live, unobservable process must not stop unrelated scans,
        // or allow a saved identity to be considered exited or safe to signal.
        installed(&std::env::current_exe().unwrap()).unwrap();
        assert!(matches!(matches(&identity), Err(Error::Uncertain(_))));
        assert_eq!(
            wait_gone(std::slice::from_ref(&identity), Duration::from_millis(50))
                .await
                .unwrap(),
            vec![identity.clone()]
        );
        assert!(native::signal(&identity, true).is_err());
        exchange(&mut child.0);
        assert!(child.0.try_wait().unwrap().is_none());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!matches(&identity).unwrap());
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;
    #[test]
    fn caller_instance_survives_rename_but_not_pid_reuse() {
        let original = Identity {
            pid: 42,
            created: "boot:100".into(),
            executable: "/bin/raft-computer".into(),
        };
        let renamed = Identity {
            executable: "/bin/.k-image-old".into(),
            ..original.clone()
        };
        assert!(same_instance(&original, &renamed));
        assert!(!same_instance(
            &original,
            &Identity {
                created: "boot:101".into(),
                ..renamed.clone()
            }
        ));
        assert!(!same_instance(&original, &Identity { pid: 43, ..renamed }));
    }
}
