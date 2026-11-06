//! Process identity comes from the OS executable path and creation identity,
//! never a name, command-line substring, or an unqualified persisted PID.
//! Arguments only ever exclude an identified product process: the product
//! binary's short-lived CLI entry (`__cli`) is not its service.
use crate::{Error, Result};
use k_carrier::error::invalid;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
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

pub(crate) fn same_path(left: &Path, right: &Path) -> bool {
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

/// The immediate parent when it runs from one of `executables`, else `None`.
/// A legacy two-layer installation keeps a launcher at the installed path that
/// execs into K's slot artifact, so a waiting product CLI may run from the
/// slot rather than the installed binary; callers pass every product image.
pub fn immediate_parent(executables: &[PathBuf]) -> Result<Option<Identity>> {
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
    Ok(identity.filter(|p| executables.iter().any(|e| same_path(&p.executable, e))))
}

pub fn observe(pid: u32) -> Result<Option<Identity>> {
    if pid <= 1 {
        return Ok(None);
    }
    native::observe(pid)
}

/// The parent pid of a same-user process; `None` once it has exited, belongs
/// to another user, or is the top of the tree.
pub fn parent_of(pid: u32) -> Result<Option<u32>> {
    if pid <= 1 {
        return Ok(None);
    }
    Ok(native::parent_of(pid)?.filter(|parent| *parent > 0))
}

/// The environment of a same-user process, used to tell apart Computer
/// instances that run one shared installed binary with different state homes
/// (`RAFT_HOME`). `None` when the OS does not expose it (Windows, access
/// denied) or the instance is no longer the one observed.
pub fn environment(identity: &Identity) -> Result<Option<Vec<(OsString, OsString)>>> {
    let Some(environment) = native::environment(identity.pid)? else {
        return Ok(None);
    };
    // The PID may have been reused while reading; only the observed instance counts.
    if observe(identity.pid)?.as_ref() != Some(identity) {
        return Ok(None);
    }
    Ok(Some(environment))
}

/// The first argument with which the product binary runs as a command-line
/// client: the `raft` command agents run is `<binary> __cli ...`. Such a
/// process is short-lived and is not the service, so it is never counted,
/// stopped or waited for as a running product.
pub const CLI_ENTRY: &str = "__cli";

/// `argv` runs the CLI entry: `__cli` is the first argument after the
/// executable. Only the exact argument counts, not a prefix or a later one.
pub fn is_cli_entry<S: AsRef<std::ffi::OsStr>>(argv: &[S]) -> bool {
    argv.get(1)
        .is_some_and(|argument| argument.as_ref() == CLI_ENTRY)
}

/// `later` was created strictly after `earlier`; `None` when their creation
/// identities cannot be compared (unparsable, or another Linux boot).
pub fn created_after(later: &Identity, earlier: &Identity) -> Option<bool> {
    let (later_scope, later_at) = creation_instant(&later.created)?;
    let (earlier_scope, earlier_at) = creation_instant(&earlier.created)?;
    (later_scope == earlier_scope).then_some(later_at > earlier_at)
}

#[cfg(windows)]
fn creation_instant(created: &str) -> Option<(&str, u128)> {
    windows_creation_instant(created)
}
#[cfg(target_os = "linux")]
fn creation_instant(created: &str) -> Option<(&str, u128)> {
    linux_creation_instant(created)
}
#[cfg(target_os = "macos")]
fn creation_instant(created: &str) -> Option<(&str, u128)> {
    macos_creation_instant(created)
}

/// Windows: the creation FILETIME as `high:low`.
#[cfg(any(windows, test))]
fn windows_creation_instant(created: &str) -> Option<(&str, u128)> {
    let (high, low) = created.split_once(':')?;
    let ticks =
        (u128::from(high.parse::<u32>().ok()?) << 32) | u128::from(low.parse::<u32>().ok()?);
    Some(("", ticks))
}

/// Linux: `boot-id:start-ticks`; ticks compare only within one boot.
#[cfg(any(target_os = "linux", test))]
fn linux_creation_instant(created: &str) -> Option<(&str, u128)> {
    let (boot, ticks) = created.rsplit_once(':')?;
    Some((boot, ticks.parse().ok()?))
}

/// macOS: `seconds:microseconds` of the process start.
#[cfg(any(target_os = "macos", test))]
fn macos_creation_instant(created: &str) -> Option<(&str, u128)> {
    let (seconds, micros) = created.split_once(':')?;
    let micros = micros.parse::<u128>().ok()?;
    (micros < 1_000_000).then_some(())?;
    Some(("", seconds.parse::<u128>().ok()? * 1_000_000 + micros))
}

/// A creation identity at `instant` in this platform's format, for tests.
#[cfg(test)]
pub(crate) fn test_creation(instant: u64) -> String {
    if cfg!(windows) {
        format!("{}:{}", instant >> 32, instant & 0xffff_ffff)
    } else if cfg!(target_os = "macos") {
        format!("{instant}:0")
    } else {
        format!("test-boot:{instant}")
    }
}

/// The observed instance runs the product binary's CLI entry. Arguments that
/// cannot be read (access denied, exited, any OS error) leave it a product
/// process, as it was before this check existed.
pub fn is_cli_invocation(identity: &Identity) -> bool {
    let Ok(Some(argv)) = native::arguments(identity.pid) else {
        return false;
    };
    // The PID may have been reused while reading; only the observed instance counts.
    is_cli_entry(&argv)
        && matches!(observe(identity.pid), Ok(Some(current)) if current == *identity)
}

/// `/proc/<pid>/cmdline`: each argument NUL-terminated.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn parse_cmdline(bytes: &[u8]) -> Vec<OsString> {
    use std::os::unix::ffi::OsStrExt;
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .strip_suffix(&[0])
        .unwrap_or(bytes)
        .split(|b| *b == 0)
        .map(|argument| std::ffi::OsStr::from_bytes(argument).to_owned())
        .collect()
}

/// KERN_PROCARGS2 (macOS): argc, the exec path, NUL padding, argv, then the
/// environment, each NUL-terminated. Returns argc and the entries from argv[0].
#[cfg(any(target_os = "macos", test))]
fn procargs_entries(buffer: &[u8]) -> Option<(usize, Vec<&[u8]>)> {
    let (count, rest) = buffer.split_first_chunk::<4>()?;
    let argc = i32::from_ne_bytes(*count).max(0) as usize;
    let mut entries = rest.split(|b| *b == 0);
    // The exec path, then the padding NULs before argv[0].
    entries.next()?;
    Some((argc, entries.skip_while(|entry| entry.is_empty()).collect()))
}

/// A Windows command line split as the Microsoft C runtime splits it into
/// argv: in the program name quotes only toggle quoting, with no escapes, and
/// a blank outside quotes ends it; in later arguments 2n backslashes before a
/// quote are n backslashes and toggle quoting, 2n+1 are n and a literal
/// quote, and `""` inside quotes is a literal quote.
#[cfg(any(windows, test))]
fn split_windows_command_line(line: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut chars = line.chars().peekable();
    let mut program = String::new();
    let mut quoted = false;
    while let Some(&c) = chars.peek() {
        if !quoted && (c == ' ' || c == '\t') {
            break;
        }
        chars.next();
        if c == '"' {
            quoted = !quoted;
        } else {
            program.push(c);
        }
    }
    arguments.push(program);
    loop {
        while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        if chars.peek().is_none() {
            return arguments;
        }
        let mut argument = String::new();
        let mut quoted = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let mut backslashes = 1;
                    while chars.next_if_eq(&'\\').is_some() {
                        backslashes += 1;
                    }
                    if chars.peek() == Some(&'"') {
                        argument.extend(std::iter::repeat_n('\\', backslashes / 2));
                        if backslashes % 2 == 1 {
                            argument.push('"');
                            chars.next();
                        }
                    } else {
                        argument.extend(std::iter::repeat_n('\\', backslashes));
                    }
                }
                '"' if quoted && chars.peek() == Some(&'"') => {
                    argument.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ' ' | '\t' if !quoted => break,
                c => argument.push(c),
            }
        }
        arguments.push(argument);
    }
}

/// `NAME=value` entries separated by NUL, as in `/proc/<pid>/environ`; stops
/// at the first empty entry.
#[cfg(unix)]
fn parse_environment<'a>(entries: impl Iterator<Item = &'a [u8]>) -> Vec<(OsString, OsString)> {
    use std::os::unix::ffi::OsStrExt;
    entries
        .take_while(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let split = entry.iter().position(|b| *b == b'=')?;
            Some((
                std::ffi::OsStr::from_bytes(&entry[..split]).to_owned(),
                std::ffi::OsStr::from_bytes(&entry[split + 1..]).to_owned(),
            ))
        })
        .collect()
}

const PROCESS_EXIT_SETTLE_ATTEMPTS: usize = 10;
const PROCESS_EXIT_SETTLE_INTERVAL: Duration = Duration::from_millis(20);

fn observe_until_known_with<O, E, W>(
    identity: &Identity,
    mut observe: O,
    mut definitely_exited: E,
    mut wait: W,
    unobservable_message: &'static str,
) -> Result<Option<Identity>>
where
    O: FnMut(u32) -> Result<Option<Identity>>,
    E: FnMut(u32) -> Result<bool>,
    W: FnMut(Duration),
{
    for attempt in 0..=PROCESS_EXIT_SETTLE_ATTEMPTS {
        match observe(identity.pid)? {
            Some(current) => return Ok(Some(current)),
            None if definitely_exited(identity.pid)? => return Ok(None),
            None if attempt == PROCESS_EXIT_SETTLE_ATTEMPTS => {
                return Err(Error::Uncertain(unobservable_message.into()));
            }
            // On macOS, libproc can stop exposing the executable before the
            // kernel reports the exiting instance as a zombie. Give that
            // bounded transition a chance to settle without treating an
            // inaccessible live process as gone.
            None => wait(PROCESS_EXIT_SETTLE_INTERVAL),
        }
    }
    unreachable!("bounded caller identity loop always returns")
}

fn instance_matches_with<O, E, W>(
    identity: &Identity,
    observe: O,
    definitely_exited: E,
    wait: W,
) -> Result<bool>
where
    O: FnMut(u32) -> Result<Option<Identity>>,
    E: FnMut(u32) -> Result<bool>,
    W: FnMut(Duration),
{
    Ok(observe_until_known_with(
        identity,
        observe,
        definitely_exited,
        wait,
        "recorded caller identity is no longer observable",
    )?
    .is_some_and(|current| same_instance(identity, &current)))
}

pub fn instance_matches(identity: &Identity) -> Result<bool> {
    instance_matches_with(
        identity,
        observe,
        native::definitely_exited,
        std::thread::sleep,
    )
}

#[cfg(test)]
mod caller_exit_tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            pid: 42,
            executable: PathBuf::from("/fixture/caller"),
            created: "fixture-created".into(),
        }
    }

    #[test]
    fn transient_unobservable_exit_settles_before_returning_false() {
        let identity = identity();
        let mut exit_checks = 0;
        let mut waits = Vec::new();

        assert!(
            !instance_matches_with(
                &identity,
                |_| Ok(None),
                |_| {
                    exit_checks += 1;
                    Ok(exit_checks == 3)
                },
                |duration| waits.push(duration),
            )
            .unwrap()
        );
        assert_eq!(exit_checks, 3);
        assert_eq!(waits, vec![PROCESS_EXIT_SETTLE_INTERVAL; 2]);
    }

    #[test]
    fn transient_unobservable_live_caller_must_reappear_with_the_same_identity() {
        let identity = identity();
        let current = identity.clone();
        let mut observations = 0;
        let mut waits = Vec::new();

        assert!(
            instance_matches_with(
                &identity,
                |_| {
                    observations += 1;
                    Ok((observations == 2).then(|| current.clone()))
                },
                |_| Ok(false),
                |duration| waits.push(duration),
            )
            .unwrap()
        );
        assert_eq!(observations, 2);
        assert_eq!(waits, vec![PROCESS_EXIT_SETTLE_INTERVAL]);
    }

    #[test]
    fn unobservable_live_caller_stays_uncertain_after_bounded_wait() {
        let identity = identity();
        let mut exit_checks = 0;
        let mut waits = Vec::new();

        let result = instance_matches_with(
            &identity,
            |_| Ok(None),
            |_| {
                exit_checks += 1;
                Ok(false)
            },
            |duration| waits.push(duration),
        );
        assert!(matches!(result, Err(Error::Uncertain(_))));
        assert_eq!(exit_checks, PROCESS_EXIT_SETTLE_ATTEMPTS + 1);
        assert_eq!(
            waits,
            vec![PROCESS_EXIT_SETTLE_INTERVAL; PROCESS_EXIT_SETTLE_ATTEMPTS]
        );
    }
}

fn matches_with<O, E, W>(
    identity: &Identity,
    observe: O,
    definitely_exited: E,
    wait: W,
) -> Result<bool>
where
    O: FnMut(u32) -> Result<Option<Identity>>,
    E: FnMut(u32) -> Result<bool>,
    W: FnMut(Duration),
{
    Ok(observe_until_known_with(
        identity,
        observe,
        definitely_exited,
        wait,
        "recorded process identity is no longer observable",
    )?
    .is_some_and(|current| {
        current.created == identity.created && same_path(&current.executable, &identity.executable)
    }))
}

pub fn matches(identity: &Identity) -> Result<bool> {
    matches_with(
        identity,
        observe,
        native::definitely_exited,
        std::thread::sleep,
    )
}

#[cfg(test)]
mod process_exit_tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            pid: 43,
            executable: PathBuf::from("/fixture/product"),
            created: "fixture-created".into(),
        }
    }

    #[test]
    fn transient_unobservable_exit_settles_before_process_returns_false() {
        let identity = identity();
        let mut exit_checks = 0;
        let mut waits = Vec::new();

        assert!(
            !matches_with(
                &identity,
                |_| Ok(None),
                |_| {
                    exit_checks += 1;
                    Ok(exit_checks == 3)
                },
                |duration| waits.push(duration),
            )
            .unwrap()
        );
        assert_eq!(exit_checks, 3);
        assert_eq!(waits, vec![PROCESS_EXIT_SETTLE_INTERVAL; 2]);
    }

    #[test]
    fn transient_unobservable_process_must_reappear_with_the_same_identity() {
        let identity = identity();
        let current = identity.clone();
        let mut observations = 0;
        let mut waits = Vec::new();

        assert!(
            matches_with(
                &identity,
                |_| {
                    observations += 1;
                    Ok((observations == 2).then(|| current.clone()))
                },
                |_| Ok(false),
                |duration| waits.push(duration),
            )
            .unwrap()
        );
        assert_eq!(observations, 2);
        assert_eq!(waits, vec![PROCESS_EXIT_SETTLE_INTERVAL]);
    }

    #[test]
    fn unobservable_live_process_stays_uncertain_after_bounded_wait() {
        let identity = identity();
        let mut exit_checks = 0;
        let mut waits = Vec::new();

        let result = matches_with(
            &identity,
            |_| Ok(None),
            |_| {
                exit_checks += 1;
                Ok(false)
            },
            |duration| waits.push(duration),
        );
        assert!(matches!(result, Err(Error::Uncertain(_))));
        assert_eq!(exit_checks, PROCESS_EXIT_SETTLE_ATTEMPTS + 1);
        assert_eq!(
            waits,
            vec![PROCESS_EXIT_SETTLE_INTERVAL; PROCESS_EXIT_SETTLE_ATTEMPTS]
        );
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

    /// A process that exits while /proc is read reports ESRCH instead of
    /// NotFound; either way it is gone.
    fn exited(e: &std::io::Error) -> bool {
        e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
    }

    #[test]
    fn a_process_exiting_mid_read_counts_as_exited() {
        assert!(exited(&std::io::Error::from_raw_os_error(libc::ESRCH)));
        assert!(exited(&std::io::ErrorKind::NotFound.into()));
        assert!(!exited(&std::io::ErrorKind::PermissionDenied.into()));
    }

    fn read_stat(pid: u32) -> Result<Option<String>> {
        let text = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(e) if exited(&e) => return Ok(None),
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

    pub fn parent_of(pid: u32) -> Result<Option<u32>> {
        let root = PathBuf::from(format!("/proc/{pid}"));
        match fs::metadata(&root) {
            Ok(meta) if meta.uid() != unsafe { libc::geteuid() } => return Ok(None),
            Ok(_) => {}
            Err(e) if exited(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let text = match fs::read_to_string(root.join("stat")) {
            Ok(text) => text,
            Err(e) if exited(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(crate::systemd::parent_from_stat(&text))
    }

    pub fn definitely_exited(pid: u32) -> Result<bool> {
        Ok(read_stat(pid)?.is_none())
    }

    pub fn observe(pid: u32) -> Result<Option<Identity>> {
        let root = PathBuf::from(format!("/proc/{pid}"));
        match fs::metadata(&root) {
            Ok(meta) if meta.uid() != unsafe { libc::geteuid() } => return Ok(None),
            Ok(_) => {}
            Err(e) if exited(&e) => return Ok(None),
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
            Err(e) if exited(&e) || e.kind() == std::io::ErrorKind::PermissionDenied => {
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

    pub fn environment(pid: u32) -> Result<Option<Vec<(OsString, OsString)>>> {
        match fs::read(format!("/proc/{pid}/environ")) {
            Ok(bytes) => Ok(Some(parse_environment(bytes.split(|b| *b == 0)))),
            // A process exiting mid-read reports ESRCH; like an inaccessible
            // one, its environment is simply unknown (counted as this home's).
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) || e.raw_os_error() == Some(libc::ESRCH) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn arguments(pid: u32) -> Result<Option<Vec<OsString>>> {
        match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(bytes) => Ok(Some(parse_cmdline(&bytes))),
            Err(e) if exited(&e) || e.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
            Err(e) => Err(e.into()),
        }
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

    pub fn parent_of(pid: u32) -> Result<Option<u32>> {
        if pid > i32::MAX as u32 {
            return Ok(None);
        }
        Ok(info(pid)?.map(|info| info.pbi_ppid))
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

    /// The KERN_PROCARGS2 buffer: argv, then the environment.
    fn procargs(pid: u32) -> Result<Option<Vec<u8>>> {
        if pid > i32::MAX as u32 {
            return Ok(None);
        }
        let mut argmax: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || argmax <= 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut buffer = vec![0u8; argmax as usize];
        let mut size = buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ESRCH | libc::EPERM)
            ) {
                return Ok(None);
            }
            return Err(error.into());
        }
        buffer.truncate(size);
        Ok(Some(buffer))
    }

    pub fn environment(pid: u32) -> Result<Option<Vec<(OsString, OsString)>>> {
        let Some(buffer) = procargs(pid)? else {
            return Ok(None);
        };
        let Some((argc, entries)) = procargs_entries(&buffer) else {
            return Ok(None);
        };
        Ok(Some(parse_environment(entries.into_iter().skip(argc))))
    }

    pub fn arguments(pid: u32) -> Result<Option<Vec<OsString>>> {
        let Some(buffer) = procargs(pid)? else {
            return Ok(None);
        };
        let Some((argc, entries)) = procargs_entries(&buffer) else {
            return Ok(None);
        };
        Ok(Some(
            entries
                .into_iter()
                .take(argc)
                .map(|argument| OsString::from_vec(argument.to_vec()))
                .collect(),
        ))
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

    /// Another process's environment is not exposed by a supported Windows API.
    pub fn environment(_pid: u32) -> Result<Option<Vec<(OsString, OsString)>>> {
        Ok(None)
    }

    /// The command line through ProcessCommandLineInformation (Windows 8.1
    /// and later; limited query access suffices), split as the C runtime
    /// splits it. `None` when the process cannot be opened or queried.
    pub fn arguments(pid: u32) -> Result<Option<Vec<OsString>>> {
        use windows_sys::{
            Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
            Win32::Foundation::UNICODE_STRING,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Ok(None);
        }
        let handle = Handle(handle);
        let mut needed = 0u32;
        // Sizing call: fails with STATUS_INFO_LENGTH_MISMATCH and the length.
        unsafe {
            NtQueryInformationProcess(
                handle.0,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        let header = size_of::<UNICODE_STRING>();
        if (needed as usize) < header || needed > 1 << 20 {
            return Ok(None);
        }
        // u64 storage keeps the UNICODE_STRING header aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        let length = (buffer.len() * size_of::<u64>()) as u32;
        let status = unsafe {
            NtQueryInformationProcess(
                handle.0,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                length,
                &mut needed,
            )
        };
        if status < 0 {
            return Ok(None);
        }
        let text = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
        let units = usize::from(text.Length) / 2;
        let start = buffer.as_ptr() as usize;
        let end = start + buffer.len() * size_of::<u64>();
        let first = text.Buffer as usize;
        // The text is returned inside the same buffer, after the header.
        if units == 0
            || text.Buffer.is_null()
            || first < start + header
            || first.saturating_add(units * 2) > end
        {
            return Ok(None);
        }
        let line = unsafe { std::slice::from_raw_parts(text.Buffer, units) };
        Ok(Some(
            split_windows_command_line(&String::from_utf16_lossy(line))
                .into_iter()
                .map(OsString::from)
                .collect(),
        ))
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
        parent_of(std::process::id())?
            .ok_or_else(|| Error::Uncertain("parent process unavailable".into()))
    }

    pub fn parent_of(pid: u32) -> Result<Option<u32>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }
        let snapshot = Handle(snapshot);
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        while found != 0 {
            if entry.th32ProcessID == pid {
                return Ok(Some(entry.th32ParentProcessID));
            }
            found = unsafe { Process32NextW(snapshot.0, &mut entry) };
        }
        Ok(None)
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
mod immediate_parent_tests {
    use super::*;

    // The test process's own parent stands in for a waiting product CLI: it
    // is attested only when its image is in the list, whatever else is listed.
    #[test]
    fn parent_is_attested_against_any_listed_executable() {
        let Some(parent) = observe(native::parent().unwrap()).unwrap() else {
            return;
        };
        let unrelated = PathBuf::from("/fixture/not-a-product-image");
        let listed = immediate_parent(&[unrelated.clone(), parent.executable.clone()]).unwrap();
        assert_eq!(listed.map(|p| p.pid), Some(parent.pid));
        assert!(immediate_parent(&[unrelated]).unwrap().is_none());
        assert!(immediate_parent(&[]).unwrap().is_none());
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

#[cfg(all(test, unix))]
mod environment_tests {
    use super::*;
    use std::process::{Child, Command};

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Not a test: the child the next test reads. Sleeps only when asked.
    #[test]
    #[ignore]
    fn environment_fixture_child() {
        if std::env::var_os("RAFT_ENVIRONMENT_FIXTURE_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    // A Computer's state home is read from its environment, including on
    // macOS (KERN_PROCARGS2), where argv precedes the environment.
    //
    // The child is this test binary, never an Apple platform binary such as
    // /bin/sleep: macOS 26 (with SIP) hides a platform binary's environment
    // from KERN_PROCARGS2, while a third-party one's (raft-computer's) stays
    // readable, so /bin/sleep made this test depend on the macOS version.
    #[test]
    fn reads_a_child_environment_after_its_arguments() {
        let child = OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::environment_tests::environment_fixture_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env_clear()
                .env("RAFT_ENVIRONMENT_FIXTURE_CHILD", "1")
                .env("RAFT_HOME", "/fixture/home with space")
                .env("HOME", "/fixture/user")
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let environment = loop {
            let identity = observe(child.0.id()).unwrap();
            let read = match identity {
                Some(identity) => environment(&identity).unwrap(),
                None => None,
            };
            match read {
                Some(environment) if environment.iter().any(|(k, _)| k == "RAFT_HOME") => {
                    break environment;
                }
                _ if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => panic!("child environment was not read: {other:?}"),
            }
        };
        assert!(environment.contains(&("RAFT_HOME".into(), "/fixture/home with space".into())));
        assert!(environment.contains(&("HOME".into(), "/fixture/user".into())));
        assert!(
            !environment
                .iter()
                .any(|(k, _)| k == "--exact" || k == "--ignored"),
            "argv is not environment"
        );
    }

    #[test]
    fn parses_entries_until_the_first_empty_one() {
        let bytes = b"A=1\0B=x=y\0NOEQUALS\0\0C=after\0";
        assert_eq!(
            parse_environment(bytes.split(|b| *b == 0)),
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())],
        );
    }
}

#[cfg(test)]
mod cli_entry_tests {
    use super::*;
    use std::process::{Child, Command};

    #[test]
    fn only_cli_as_the_first_argument_is_the_cli_entry() {
        assert!(is_cli_entry(&["/x/raft-computer", "__cli"]));
        assert!(is_cli_entry(&["raft-computer", "__cli", "agent", "list"]));
        assert!(!is_cli_entry(&["/x/raft-computer"]));
        assert!(!is_cli_entry::<&str>(&[]));
        assert!(!is_cli_entry(&["/x/raft-computer", "serve"]));
        assert!(!is_cli_entry(&["/x/raft-computer", "start", "__cli"]));
        assert!(!is_cli_entry(&["/x/raft-computer", "__cli2"]));
        assert!(!is_cli_entry(&["/x/raft-computer", "__CLI"]));
        assert!(!is_cli_entry(&["/x/raft-computer", " __cli"]));
        // argv[0] is the executable, never the entry.
        assert!(!is_cli_entry(&["__cli", "serve"]));
    }

    #[cfg(unix)]
    #[test]
    fn linux_cmdline_splits_on_nul() {
        let argv = parse_cmdline(b"/x/raft-computer\0__cli\0agent\0\0list\0");
        assert_eq!(argv, ["/x/raft-computer", "__cli", "agent", "", "list"]);
        assert!(is_cli_entry(&argv));
        assert!(parse_cmdline(b"").is_empty());
        // A process that rewrote its title into one space-joined string.
        assert!(!is_cli_entry(&parse_cmdline(b"raft-computer __cli agent")));
    }

    #[test]
    fn macos_procargs_yield_argv_before_the_environment() {
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/x/raft-computer\0\0\0\0");
        buffer.extend_from_slice(b"raft-computer\0__cli\0status\0HOME=/u\0\0");
        let (argc, entries) = procargs_entries(&buffer).unwrap();
        assert_eq!(argc, 3);
        assert_eq!(
            &entries[..argc],
            [&b"raft-computer"[..], b"__cli", b"status"]
        );
        assert!(is_cli_entry(
            &entries[..argc]
                .iter()
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect::<Vec<_>>()
        ));
        assert_eq!(
            parse_environment_for_test(entries.into_iter().skip(argc)),
            vec![("HOME".to_owned(), "/u".to_owned())]
        );
        assert!(procargs_entries(b"\x01\0").is_none());
    }

    fn parse_environment_for_test<'a>(
        entries: impl Iterator<Item = &'a [u8]>,
    ) -> Vec<(String, String)> {
        entries
            .take_while(|entry| !entry.is_empty())
            .filter_map(|entry| {
                let text = String::from_utf8_lossy(entry);
                let (name, value) = text.split_once('=')?;
                Some((name.to_owned(), value.to_owned()))
            })
            .collect()
    }

    #[test]
    fn windows_command_lines_split_as_the_c_runtime_does() {
        let split = split_windows_command_line;
        assert_eq!(
            split(r#""C:\Program Files\Raft\raft-computer.exe" __cli agent list"#),
            [
                r"C:\Program Files\Raft\raft-computer.exe",
                "__cli",
                "agent",
                "list"
            ]
        );
        assert_eq!(
            split(r"C:\Users\a\raft-computer.exe   __cli"),
            [r"C:\Users\a\raft-computer.exe", "__cli"]
        );
        // A quoted entry is the same argument; a quote-joined one is not.
        assert_eq!(split(r#"raft-computer.exe "__cli" x"#)[1], "__cli");
        assert_eq!(split(r#"raft-computer.exe __cli"x y" z"#)[1], "__clix y");
        // Backslashes before a quote, and "" inside quotes.
        assert_eq!(
            split(r#"a.exe "x\"y" \\"z" w\\\"v "p""q""#),
            ["a.exe", r#"x"y"#, r"\z", r#"w\"v"#, r#"p"q"#]
        );
        assert_eq!(split(r"a.exe c:\dir\ x"), ["a.exe", r"c:\dir\", "x"]);
        // argv[0] takes no escapes: its quotes only toggle quoting.
        assert_eq!(split(r#""a\"b __cli"#), [r"a\b", "__cli"]);
        assert_eq!(split(r#"a"b c"d x"#), ["ab cd", "x"]);
        assert_eq!(split("a.exe"), ["a.exe"]);
        assert_eq!(split("a.exe \t "), ["a.exe"]);
        assert!(is_cli_entry(&split("raft-computer.exe\t__cli")));
        assert!(!is_cli_entry(&split("raft-computer.exe serve __cli")));
    }

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    const FIXTURE: &str = "process::cli_entry_tests::cli_fixture_child";

    /// Not a test: the child the next test reads. Sleeps only when asked.
    #[test]
    #[ignore]
    fn cli_fixture_child() {
        if std::env::var_os("RAFT_CLI_FIXTURE_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// The test binary as a child with these arguments; libtest takes
    /// `__cli` as one more name filter, so the child runs only the fixture.
    fn spawn(arguments: &[&str]) -> OwnedChild {
        OwnedChild(
            Command::new(std::env::current_exe().unwrap())
                .args(arguments)
                .env("RAFT_CLI_FIXTURE_CHILD", "1")
                .spawn()
                .unwrap(),
        )
    }

    fn classified(child: &OwnedChild) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(identity) = observe(child.0.id()).unwrap()
                && native::arguments(identity.pid)
                    .unwrap()
                    .is_some_and(|argv| argv.len() > 1)
            {
                return is_cli_invocation(&identity);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child arguments were not read"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // Reads a live process's arguments through this platform's API
    // (/proc cmdline, KERN_PROCARGS2, or the Windows command line).
    #[test]
    fn a_live_process_is_classified_by_its_first_argument() {
        let cli = spawn(&[CLI_ENTRY, "--exact", FIXTURE, "--ignored"]);
        let other = spawn(&["--exact", FIXTURE, "--ignored", CLI_ENTRY]);
        assert!(classified(&cli), "`<binary> __cli ...` is a CLI invocation");
        assert!(!classified(&other), "a later __cli is not the entry");
    }
}

#[cfg(test)]
mod creation_order_tests {
    use super::*;

    fn at(created: &str) -> Identity {
        Identity {
            pid: 1,
            executable: PathBuf::from("/x"),
            created: created.into(),
        }
    }

    #[test]
    fn each_platform_format_orders_creation() {
        assert_eq!(windows_creation_instant("1:0"), Some(("", 1 << 32)));
        assert!(
            windows_creation_instant("0:4294967295").unwrap().1
                < windows_creation_instant("1:0").unwrap().1
        );
        assert_eq!(windows_creation_instant("x:1"), None);
        let boot = "6d3a1c1e-0000-4000-8000-000000000000";
        assert_eq!(
            linux_creation_instant(&format!("{boot}:42")),
            Some((boot, 42))
        );
        assert_eq!(linux_creation_instant("no-ticks"), None);
        assert_eq!(macos_creation_instant("2:5"), Some(("", 2_000_005)));
        assert!(
            macos_creation_instant("1:999999").unwrap().1
                < macos_creation_instant("2:0").unwrap().1
        );
        assert_eq!(macos_creation_instant("1:1000000"), None);
    }

    #[test]
    fn created_after_compares_this_platforms_identities() {
        let (old, new) = (at(&test_creation(10)), at(&test_creation(20)));
        assert_eq!(created_after(&new, &old), Some(true));
        assert_eq!(created_after(&old, &new), Some(false));
        assert_eq!(created_after(&old, &old), Some(false));
        assert_eq!(created_after(&at("garbage"), &old), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn another_boot_is_not_ordered() {
        assert_eq!(created_after(&at("boot-b:1"), &at("boot-a:9")), None);
    }

    #[test]
    fn a_child_is_never_created_before_its_live_parent() {
        let own = observe(std::process::id()).unwrap().unwrap();
        let Some(parent) = observe(native::parent().unwrap()).unwrap() else {
            return; // a parent of another user or a protected one
        };
        assert_ne!(created_after(&parent, &own), Some(true));
        assert!(
            created_after(&own, &own).is_some(),
            "own identity is ordered"
        );
    }
}
