//! The answer to a failure that reached main() without an operation reply, and
//! the up-front check that the state directories can be written. Every such
//! answer names a cause the user can act on and one runnable next step; only an
//! unclassified failure keeps the plain "run it again" advice (#proj-raft-computer:#44).
use crate::{Error, config::Config, report::FailureCode, request::Reply};
use std::{
    io,
    path::{Path, PathBuf},
};

/// What kind of failure an error is, from the I/O errors in its source chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    PermissionDenied,
    ReadOnly,
    DiskFull,
    Unclassified,
}

impl Cause {
    /// Running the same command again can only help an unclassified failure:
    /// a permission, read-only or full-disk failure repeats identically.
    pub fn retry_helps(self) -> bool {
        self == Self::Unclassified
    }
}

fn io_cause(error: &io::Error) -> Option<Cause> {
    #[cfg(unix)]
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => return Some(Cause::PermissionDenied),
        Some(libc::EROFS) => return Some(Cause::ReadOnly),
        Some(libc::ENOSPC | libc::EDQUOT) => return Some(Cause::DiskFull),
        _ => {}
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied => Some(Cause::PermissionDenied),
        io::ErrorKind::ReadOnlyFilesystem => Some(Cause::ReadOnly),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => Some(Cause::DiskFull),
        _ => error
            .get_ref()
            .and_then(|inner| chain_cause(inner as &(dyn std::error::Error + 'static))),
    }
}

fn chain_cause(error: &(dyn std::error::Error + 'static)) -> Option<Cause> {
    let mut next = Some(error);
    while let Some(current) = next {
        if let Some(cause) = current.downcast_ref::<io::Error>().and_then(io_cause) {
            return Some(cause);
        }
        next = current.source();
    }
    None
}

/// Classify an error by walking its source chain for an I/O error. K's `Io`
/// variant is transparent (its `source()` skips the I/O error itself), so it
/// is inspected directly.
pub fn classify(error: &Error) -> Cause {
    match error {
        Error::Io(error) => io_cause(error),
        error => chain_cause(error),
    }
    .unwrap_or(Cause::Unclassified)
}

/// A directory this user cannot create entries in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    pub path: PathBuf,
    pub read_only: bool,
}

/// The directories `command` writes to first, and the lock files it must
/// read. Checking them only reads metadata: nothing is created, so `status`
/// stays read-only here. `status` checks only what it always touches (the
/// supervisor's scratch and the operation gate).
fn targets(cfg: &Config, command: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let gate = cfg.installer_dir.join("gate");
    let mut dirs = vec![
        cfg.scratch().join("supervisors"),
        gate.join("upgrade.lock.claims"),
    ];
    let mut files = vec![gate.join("upgrade.lock")];
    if command != "status" {
        dirs.push(cfg.installer_dir.join("receipts"));
        dirs.push(cfg.k_state.join("upgrade.lock.claims"));
        files.push(cfg.k_state.join("upgrade.lock"));
    }
    if ["install", "upgrade", "repair"].contains(&command) {
        dirs.push(cfg.installer_dir.join("bin"));
        dirs.push(cfg.install_dir.clone());
    }
    (dirs, files)
}

/// The first state directory (or its nearest existing ancestor), or existing
/// lock file, that `command` needs and this user cannot write or read.
pub fn blocked(cfg: &Config, command: &str) -> Option<Blocked> {
    let (dirs, files) = targets(cfg, command);
    dirs.iter()
        .find_map(|target| blocked_on_the_way(target))
        .or_else(|| files.iter().find_map(|file| unreadable(file)))
}

#[cfg(unix)]
fn access(path: &Path, mode: libc::c_int) -> Option<Option<i32>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string for the call's duration.
    if unsafe { libc::access(path.as_ptr(), mode) } == 0 {
        return Some(None);
    }
    Some(io::Error::last_os_error().raw_os_error())
}

#[cfg(unix)]
fn denied(path: &Path, errno: Option<i32>) -> Option<Blocked> {
    match errno {
        Some(libc::EACCES | libc::EPERM) => Some(Blocked {
            path: path.to_path_buf(),
            read_only: false,
        }),
        Some(libc::EROFS) => Some(Blocked {
            path: path.to_path_buf(),
            read_only: true,
        }),
        _ => None,
    }
}

/// The nearest existing ancestor of `target` (inclusive) decides whether it
/// can be created or written; an unsearchable ancestor shows up as that
/// ancestor's own missing search permission.
#[cfg(unix)]
pub fn blocked_on_the_way(target: &Path) -> Option<Blocked> {
    let mut existing = target;
    let metadata = loop {
        match std::fs::metadata(existing) {
            Ok(metadata) => break metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    };
    if !metadata.is_dir() {
        return None;
    }
    denied(existing, access(existing, libc::W_OK | libc::X_OK)?)
}

/// An existing file this user cannot read (a lock left by another account).
#[cfg(unix)]
fn unreadable(file: &Path) -> Option<Blocked> {
    if !std::fs::symlink_metadata(file).is_ok_and(|m| m.is_file()) {
        return None;
    }
    denied(file, access(file, libc::R_OK)?)
}

#[cfg(not(unix))]
pub fn blocked_on_the_way(_target: &Path) -> Option<Blocked> {
    None
}

#[cfg(not(unix))]
fn unreadable(_file: &Path) -> Option<Blocked> {
    None
}

/// What to give back to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChownTarget {
    pub path: PathBuf,
    /// The link under home that leads to `path`, when one does: `chown -R`
    /// on a link changes the link, not the folder it points to.
    pub link: Option<PathBuf>,
}

/// The topmost directory between the home (exclusive) and `blocked`
/// (inclusive) that the user does not own. Never the home itself or anything
/// outside it; `None` when every one is owned. A link on the way is followed,
/// and the folder it leads to is checked instead.
pub fn chown_target(
    blocked: &Path,
    home: &Path,
    owned: impl Fn(&Path) -> bool,
) -> Option<ChownTarget> {
    let relative = blocked.strip_prefix(home).ok()?;
    let mut current = home.to_path_buf();
    let mut components = relative.components();
    while let Some(component) = components.next() {
        current.push(component);
        if std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
            let link = current.clone();
            let mut resolved = std::fs::canonicalize(&link).ok()?;
            if !owned(&resolved) {
                return Some(ChownTarget {
                    path: resolved,
                    link: Some(link),
                });
            }
            for component in components {
                resolved.push(component);
                if !owned(&resolved) {
                    return Some(ChownTarget {
                        path: resolved,
                        link: Some(link),
                    });
                }
            }
            return None;
        }
        if !owned(&current) {
            return Some(ChownTarget {
                path: current,
                link: None,
            });
        }
    }
    None
}

#[cfg(unix)]
fn owner_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|metadata| metadata.uid())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 16384];
    // SAFETY: passwd is plain data; getpwuid_r fills it with pointers into
    // `buffer`, which outlives every read below.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: a successful lookup leaves a NUL-terminated name in `buffer`.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    name.to_str()
        .ok()
        .filter(|name| !name.is_empty())
        .map(String::from)
}

/// A name `chown` accepts for this account: the user name, else the uid.
#[cfg(unix)]
fn account(uid: u32) -> String {
    user_name(uid).unwrap_or_else(|| uid.to_string())
}

#[cfg(not(unix))]
fn current_user() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "your user".into())
}

#[cfg(unix)]
fn current_user() -> String {
    account(current_uid())
}

/// A top-level failure as the user sees it: what went wrong, then one runnable
/// next step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub message: String,
    pub next_step: String,
    pub code: FailureCode,
    pub retry_helps: bool,
}

impl Answer {
    pub fn line(&self) -> String {
        format!("{} {}", self.message, self.next_step)
    }

    pub fn reply(&self, id: &str) -> Reply {
        Reply::failure(id, 1, self.line(), self.code, self.message.clone())
            .with_next_step(self.next_step.clone())
    }
}

/// Paths are shown the way people type them (`~/…`). A directory inside the
/// private installation state is shown, and fixed, as the state home: those
/// internal folders are not something the user needs to know about. When the
/// state home is the home itself (or above it), the folder directly under
/// home on the way is shown instead: home is never the thing to fix.
fn shown(path: &Path, cfg: Option<&Config>) -> PathBuf {
    match cfg {
        Some(cfg) if path.starts_with(&cfg.installer_dir) => {
            if !cfg.user_home.starts_with(&cfg.state_home) {
                return cfg.state_home.clone();
            }
            match path
                .strip_prefix(&cfg.user_home)
                .ok()
                .and_then(|rest| rest.components().next())
            {
                Some(first) => cfg.user_home.join(first),
                None => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

fn display(path: &Path, cfg: Option<&Config>) -> String {
    match cfg {
        Some(cfg) => crate::shell_path::display_under_home(path, &cfg.user_home),
        None => path.display().to_string(),
    }
}

#[cfg(unix)]
fn word(path: &Path, cfg: Option<&Config>) -> String {
    crate::shell_path::shell_word(&display(path, cfg))
}

/// What the permission answer needs to know about this machine, so tests can
/// supply owners and names.
#[cfg(unix)]
pub struct Accounts<'a> {
    pub uid: u32,
    pub user: String,
    pub owner: &'a dyn Fn(&Path) -> Option<u32>,
    pub name: &'a dyn Fn(u32) -> String,
}

#[cfg(unix)]
impl Accounts<'static> {
    fn system() -> Self {
        Accounts {
            uid: current_uid(),
            user: current_user(),
            owner: &owner_of,
            name: &account,
        }
    }
}

/// The answer for a directory this user cannot write.
pub fn not_writable(blocked: &Blocked, cfg: Option<&Config>) -> Answer {
    #[cfg(unix)]
    {
        not_writable_for(blocked, cfg, &Accounts::system())
    }
    #[cfg(not(unix))]
    {
        let path = shown(&blocked.path, cfg);
        if blocked.read_only {
            return read_only(&path, cfg);
        }
        Answer {
            message: format!(
                "Raft Computer can't write to {}: permission denied.",
                display(&path, cfg)
            ),
            next_step: format!(
                "Make {} writable by {}, then run the same install command again.",
                display(&path, cfg),
                current_user()
            ),
            code: FailureCode::InstallationDirNotWritable,
            retry_helps: false,
        }
    }
}

fn read_only(path: &Path, cfg: Option<&Config>) -> Answer {
    Answer {
        message: format!(
            "Raft Computer can't write to {}: the file system is read-only.",
            display(path, cfg)
        ),
        next_step: "Make it writable, or set RAFT_HOME to a writable folder, then run the same install command again.".into(),
        code: FailureCode::InstallationDirNotWritable,
        retry_helps: false,
    }
}

#[cfg(unix)]
pub fn not_writable_for(blocked: &Blocked, cfg: Option<&Config>, accounts: &Accounts) -> Answer {
    let path = shown(&blocked.path, cfg);
    if blocked.read_only {
        return read_only(&path, cfg);
    }
    let user = &accounts.user;
    // The reason names the folder that actually blocks; when a parent is
    // shown in its place, it says the blocking folder is inside it.
    let subject = if path == blocked.path {
        "it"
    } else {
        "a folder inside it"
    };
    let why = match (accounts.owner)(&blocked.path) {
        Some(owner) if owner != accounts.uid => {
            format!("{subject} belongs to {}", (accounts.name)(owner))
        }
        _ => format!("{subject} is not writable by {user}"),
    };
    let message = format!(
        "Raft Computer can't write to {}: permission denied ({why}).",
        display(&path, cfg)
    );
    if let Some(cfg) = cfg
        && path.starts_with(&cfg.user_home)
        && path != cfg.user_home
    {
        let owned = |p: &Path| (accounts.owner)(p) == Some(accounts.uid);
        let next_step = match chown_target(&blocked.path, &cfg.user_home, owned) {
            Some(ChownTarget {
                path: target,
                link: Some(link),
            }) if !strictly_inside(&target, &cfg.user_home) => {
                // A link out of home (or to home itself, or to `/`): never
                // hand that folder to a recursive chown. Name it and its
                // owner; the user decides.
                let owner = match (accounts.owner)(&target) {
                    Some(owner) => (accounts.name)(owner),
                    None => "another user".into(),
                };
                format!(
                    "{} is a link to {}, which belongs to {owner}. Fix that folder's owner yourself, or set RAFT_HOME to a folder you own, then run the same install command again.",
                    display(&link, Some(cfg)),
                    display(&target, Some(cfg))
                )
            }
            Some(ChownTarget { path: target, link }) => {
                let target = shown(&target, Some(cfg));
                let command = format!(
                    "sudo chown -R {} {}",
                    crate::shell_path::shell_word(user),
                    word(&target, Some(cfg))
                );
                match link {
                    Some(link) => format!(
                        "{} is a link to {}. Fix the owner of that folder, then run the same install command again: {command}",
                        display(&link, Some(cfg)),
                        display(&target, Some(cfg))
                    ),
                    None => {
                        format!("Fix the owner, then run the same install command again: {command}")
                    }
                }
            }
            None if path == blocked.path => format!(
                "Make it writable, then run the same install command again: chmod u+w {}",
                word(&path, Some(cfg))
            ),
            None => format!(
                "Make it writable, then run the same install command again: chmod -R u+w {}",
                word(&path, Some(cfg))
            ),
        };
        return Answer {
            message,
            next_step,
            code: FailureCode::InstallationDirNotWritable,
            retry_helps: false,
        };
    }
    Answer {
        message,
        next_step: format!(
            "Make {} writable by {user}, then run the same install command again.",
            display(&path, cfg)
        ),
        code: FailureCode::InstallationDirNotWritable,
        retry_helps: false,
    }
}

/// `path` lies below `home` (not home itself, an ancestor of it, or outside
/// it), comparing against the home as given and as resolved.
#[cfg(unix)]
fn strictly_inside(path: &Path, home: &Path) -> bool {
    let resolved = std::fs::canonicalize(home).ok();
    std::iter::once(home)
        .chain(resolved.as_deref())
        .any(|home| path.starts_with(home) && path != home)
}

/// Root with a home that belongs to someone else (`sudo` keeping HOME): every
/// folder this run creates there would belong to root, and the user's own
/// next run could not write it. Refuse before anything is written.
pub fn root_refusal(
    euid: u32,
    home_owner: Option<u32>,
    name: impl Fn(u32) -> String,
) -> Option<Answer> {
    match home_owner {
        Some(owner) if euid == 0 && owner != 0 => Some(Answer {
            message: format!(
                "Don't run this as root: it would create files in {}'s home that only root can change.",
                name(owner)
            ),
            next_step: "Run the same install command again without sudo.".into(),
            code: FailureCode::RunningAsRoot,
            retry_helps: false,
        }),
        _ => None,
    }
}

/// The up-front check before `command` writes anything: refuse root in
/// another user's home, then name a folder this user cannot write.
pub fn preflight(cfg: &Config, command: &str) -> Option<Answer> {
    #[cfg(unix)]
    if let Some(answer) = root_refusal(current_uid(), owner_of(&cfg.user_home), account) {
        return Some(answer);
    }
    blocked(cfg, command).map(|blocked| not_writable(&blocked, Some(cfg)))
}

/// The answer for an error that reached main() or failed an operation.
/// `blocked` is the state directory found unwritable, if any: an I/O error
/// does not carry its path.
pub fn answer(error: &Error, cfg: Option<&Config>, blocked: Option<&Blocked>) -> Answer {
    let cause = classify(error);
    let answer = match (cause, blocked) {
        (Cause::PermissionDenied | Cause::ReadOnly, Some(blocked)) => not_writable(blocked, cfg),
        (Cause::PermissionDenied, None) => unlocated_permission(cfg),
        (Cause::ReadOnly, None) => Answer {
            message: "Raft Computer can't write its files: the file system is read-only.".into(),
            next_step: "Make it writable, or set RAFT_HOME to a writable folder, then run the same install command again.".into(),
            code: FailureCode::InstallationDirNotWritable,
            retry_helps: false,
        },
        (Cause::DiskFull, _) => Answer {
            message: "Raft Computer can't write its files: the disk is full.".into(),
            next_step: "Free up disk space, then run the same install command again.".into(),
            code: FailureCode::UnexpectedError,
            retry_helps: false,
        },
        (Cause::Unclassified, _) => Answer {
            message: "The installation could not finish.".into(),
            next_step: "Run the same install command again.".into(),
            code: FailureCode::UnexpectedError,
            retry_helps: true,
        },
    };
    debug_assert_eq!(answer.retry_helps, cause.retry_helps());
    answer
}

/// Permission denied somewhere no check could find (ownership mixed deeper
/// inside the state home): give the whole state home back when it is a
/// folder of its own under home.
fn unlocated_permission(cfg: Option<&Config>) -> Answer {
    let user = current_user();
    let (message, next_step) = match cfg {
        #[cfg(unix)]
        Some(cfg)
            if cfg.state_home.starts_with(&cfg.user_home) && cfg.state_home != cfg.user_home =>
        {
            (
                format!(
                    "Raft Computer can't write to a folder inside {}: permission denied.",
                    display(&cfg.state_home, Some(cfg))
                ),
                format!(
                    "Fix the owner, then run the same install command again: sudo chown -R {} {}",
                    crate::shell_path::shell_word(&user),
                    word(&cfg.state_home, Some(cfg))
                ),
            )
        }
        Some(cfg) => (
            "Raft Computer can't write to a folder it needs: permission denied.".into(),
            format!(
                "Make {} writable by {user}, then run the same install command again.",
                display(&cfg.state_home, Some(cfg))
            ),
        ),
        None => (
            "Raft Computer can't write to a folder it needs: permission denied.".into(),
            format!(
                "Make your home folder writable by {user}, then run the same install command again."
            ),
        ),
    };
    Answer {
        message,
        next_step,
        code: FailureCode::InstallationDirNotWritable,
        retry_helps: false,
    }
}

/// One line, only when the private record of this failure could not be kept.
pub const NOT_SAVED_LINE: &str = "Details of this failure could not be saved on this computer.";

/// Report an error that reached main() and return the exit code. With
/// `--json`, stdout carries exactly one reply object; the human line (cause
/// and next step together) goes to stderr either way.
pub fn report(error: &Error, json: bool) -> u8 {
    let cfg = Config::load().ok();
    let saved = cfg
        .as_ref()
        .is_some_and(|cfg| crate::diagnostic::preserve_top_level_failure(cfg, error));
    let cause = classify(error);
    let blocked = match (cause, cfg.as_ref()) {
        (Cause::PermissionDenied | Cause::ReadOnly, Some(cfg)) => blocked(cfg, "upgrade"),
        _ => None,
    };
    let answer = answer(error, cfg.as_ref(), blocked.as_ref());
    eprintln!("{}", answer.line());
    if !saved {
        eprintln!("{NOT_SAVED_LINE}");
    }
    if json {
        let id = std::env::var("RAFT_COMPUTER_OPERATION_ID")
            .ok()
            .filter(|id| {
                crate::request::Reply::plain(id, 0, "-")
                    .validate(id)
                    .is_ok()
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let reply = answer
            .reply(&id)
            .with_diagnostic(crate::diagnostic::safe_error(error));
        let reply = if reply.validate(&id).is_ok() {
            reply
        } else {
            answer.reply(&id)
        };
        if let Ok(text) = serde_json::to_string(&reply) {
            println!("{text}");
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use k_carrier::error::invalid;

    fn config(home: &Path) -> Config {
        let state_home = home.join(".raft");
        Config {
            user_home: home.to_path_buf(),
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            state_home,
            install_dir: home.join(".local/bin"),
            binary: home.join(".local/bin/raft-computer"),
            sidecar: home.join(".local/bin/photon_rs_bg.wasm"),
            hands_origin: "https://hands.build".into(),
            hands_app: "raft-computer-cli".into(),
            waiting_caller: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn eacces_is_classified_directly_and_nested_in_a_chain() {
        let denied = || io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(classify(&Error::Io(denied())), Cause::PermissionDenied);
        // Wrapped in another I/O error, as a library would.
        let wrapped = io::Error::other(denied());
        assert_eq!(classify(&Error::Io(wrapped)), Cause::PermissionDenied);
        #[derive(Debug)]
        struct Outer(io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("outer")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let nested = io::Error::other(Outer(denied()));
        assert_eq!(classify(&Error::Io(nested)), Cause::PermissionDenied);
        assert_eq!(
            classify(&Error::Io(io::Error::from(io::ErrorKind::PermissionDenied))),
            Cause::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn full_disk_and_read_only_are_classified() {
        let full = Error::Io(io::Error::from_raw_os_error(libc::ENOSPC));
        assert_eq!(classify(&full), Cause::DiskFull);
        let read_only = Error::Io(io::Error::from_raw_os_error(libc::EROFS));
        assert_eq!(classify(&read_only), Cause::ReadOnly);
        let answer = answer(&full, None, None);
        assert_eq!(
            answer.message,
            "Raft Computer can't write its files: the disk is full."
        );
        assert_eq!(
            answer.next_step,
            "Free up disk space, then run the same install command again."
        );
        assert!(!answer.retry_helps);
    }

    #[test]
    fn other_errors_are_unclassified_and_keep_the_plain_retry() {
        for error in [
            invalid("unknown option"),
            Error::Io(io::Error::from(io::ErrorKind::NotFound)),
            Error::Uncertain("x".into()),
        ] {
            assert_eq!(classify(&error), Cause::Unclassified);
            let answer = answer(&error, None, None);
            assert!(answer.retry_helps);
            assert_eq!(answer.line(), crate::cli::failure_line());
            assert_eq!(answer.code, FailureCode::UnexpectedError);
        }
    }

    #[test]
    fn retry_advice_only_for_retryable_causes() {
        assert!(Cause::Unclassified.retry_helps());
        for cause in [Cause::PermissionDenied, Cause::ReadOnly, Cause::DiskFull] {
            assert!(!cause.retry_helps());
        }
        let denied = Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        let answer = answer(&denied, None, None);
        assert!(!answer.retry_helps);
        assert!(!answer.line().contains("Run the same install command again"));
        assert_eq!(answer.code, FailureCode::InstallationDirNotWritable);
    }

    #[test]
    fn chown_target_is_the_topmost_unowned_directory_under_home() {
        let home = Path::new("/home/u");
        let blocked = Path::new("/home/u/.raft/computer/installer");
        let target = |p: &str| {
            Some(ChownTarget {
                path: PathBuf::from(p),
                link: None,
            })
        };
        let foreign = [
            Path::new("/home/u/.raft"),
            Path::new("/home/u/.raft/computer"),
        ];
        let owned = |p: &Path| !foreign.contains(&p);
        assert_eq!(chown_target(blocked, home, owned), target("/home/u/.raft"));
        // Only a deeper directory is foreign.
        let owned = |p: &Path| p != Path::new("/home/u/.raft/computer");
        assert_eq!(
            chown_target(blocked, home, owned),
            target("/home/u/.raft/computer")
        );
        // Everything owned: nothing to chown (a mode problem).
        assert_eq!(chown_target(blocked, home, |_| true), None);
        // Never the home itself, nor anything outside it.
        assert_eq!(chown_target(home, home, |_| false), None);
        assert_eq!(chown_target(Path::new("/srv/rw"), home, |_| false), None);
        assert_eq!(chown_target(Path::new("/"), home, |_| false), None);
    }

    #[cfg(unix)]
    fn accounts<'a>(
        owner: &'a dyn Fn(&Path) -> Option<u32>,
        name: &'a dyn Fn(u32) -> String,
    ) -> Accounts<'a> {
        Accounts {
            uid: 1000,
            user: "u".into(),
            owner,
            name,
        }
    }

    #[cfg(unix)]
    fn names(uid: u32) -> String {
        if uid == 0 {
            "root".into()
        } else {
            format!("user{uid}")
        }
    }

    #[cfg(unix)]
    fn denied_at(path: &Path) -> Blocked {
        Blocked {
            path: path.to_path_buf(),
            read_only: false,
        }
    }

    /// An odd state home and user name are quoted for pasting.
    #[cfg(unix)]
    #[test]
    fn the_chown_command_quotes_odd_paths_and_names() {
        let home = Path::new("/home/u");
        let mut cfg = config(home);
        cfg.state_home = home.join("my raft $x'q");
        cfg.installer_dir = cfg.state_home.join("computer/installer");
        let foreign = cfg.state_home.clone();
        let owner = move |p: &Path| Some(if p == foreign { 0 } else { 1000 });
        let mut look = accounts(&owner, &names);
        look.user = "a b".into();
        let answer = not_writable_for(&denied_at(&cfg.state_home), Some(&cfg), &look);
        assert_eq!(
            answer.message,
            "Raft Computer can't write to ~/my raft $x'q: permission denied (it belongs to root)."
        );
        assert_eq!(
            answer.next_step,
            "Fix the owner, then run the same install command again: sudo chown -R 'a b' ~/'my raft $x'\\''q'"
        );
    }

    /// Home itself, or `/`, is never handed to chown or chmod.
    #[cfg(unix)]
    #[test]
    fn home_and_root_are_never_the_folder_to_fix() {
        let home = Path::new("/home/u");
        let cfg = config(home);
        let owner = |_: &Path| Some(0);
        let look = accounts(&owner, &names);
        let answer = not_writable_for(&denied_at(home), Some(&cfg), &look);
        assert_eq!(
            answer.next_step,
            "Make ~/ writable by u, then run the same install command again."
        );
        let answer = not_writable_for(&denied_at(Path::new("/")), Some(&cfg), &look);
        assert_eq!(
            answer.next_step,
            "Make / writable by u, then run the same install command again."
        );
    }

    /// A user-owned ~/.raft with a root-owned folder inside: the reason names
    /// that folder's owner, the fix covers the state home.
    #[cfg(unix)]
    #[test]
    fn the_reason_names_the_owner_of_the_folder_that_blocks() {
        let home = Path::new("/home/u");
        let cfg = config(home);
        let inner = cfg.installer_dir.clone();
        let owner = move |p: &Path| Some(if p == inner { 0 } else { 1000 });
        let look = accounts(&owner, &names);
        let answer = not_writable_for(&denied_at(&cfg.installer_dir), Some(&cfg), &look);
        assert_eq!(
            answer.line(),
            "Raft Computer can't write to ~/.raft: permission denied (a folder inside it belongs to root). Fix the owner, then run the same install command again: sudo chown -R u ~/.raft"
        );
    }

    /// RAFT_HOME is the home itself: the folder under home is named, not `~/`.
    #[cfg(unix)]
    #[test]
    fn a_state_home_equal_to_home_names_the_folder_under_it() {
        let home = Path::new("/home/u");
        let mut cfg = config(home);
        cfg.state_home = home.to_path_buf();
        cfg.k_state = home.join("computer/k");
        cfg.installer_dir = home.join("computer/installer");
        let inner = cfg.installer_dir.clone();
        let owner = move |p: &Path| Some(if p == inner { 0 } else { 1000 });
        let look = accounts(&owner, &names);
        let answer = not_writable_for(&denied_at(&cfg.installer_dir), Some(&cfg), &look);
        assert_eq!(
            answer.line(),
            "Raft Computer can't write to ~/computer: permission denied (a folder inside it belongs to root). Fix the owner, then run the same install command again: sudo chown -R u ~/computer"
        );
    }

    /// A linked ~/.raft: chown -R on the link would change only the link, so
    /// the folder it points to is named. Outside home, that folder is never
    /// handed to a recursive chown; inside home, it is.
    #[cfg(unix)]
    #[test]
    fn a_linked_state_home_is_fixed_at_its_target() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let data = elsewhere.path().canonicalize().unwrap().join("data");
        std::fs::create_dir_all(data.join("computer")).unwrap();
        std::os::unix::fs::symlink(&data, home.path().join(".raft")).unwrap();
        let cfg = config(home.path());
        let foreign = data.clone();
        let owner = move |p: &Path| Some(if p == foreign { 0 } else { 1000 });
        let look = accounts(&owner, &names);
        let blocked = denied_at(&cfg.state_home.join("computer"));
        let answer = not_writable_for(&blocked, Some(&cfg), &look);
        assert_eq!(
            answer.next_step,
            format!(
                "~/.raft is a link to {}, which belongs to root. Fix that folder's owner yourself, or set RAFT_HOME to a folder you own, then run the same install command again.",
                data.display()
            )
        );
        assert!(!answer.line().contains("chown"));
    }

    #[cfg(unix)]
    fn linked_answer(target: &Path) -> Answer {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().canonicalize().unwrap();
        let link_target = if target == Path::new("HOME") {
            home_path.clone()
        } else {
            target.to_path_buf()
        };
        std::os::unix::fs::symlink(&link_target, home_path.join(".raft")).unwrap();
        let cfg = config(&home_path);
        // Nothing on the resolved side belongs to this user.
        let owner = |_: &Path| Some(0);
        let look = accounts(&owner, &names);
        not_writable_for(&denied_at(&cfg.state_home), Some(&cfg), &look)
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_root_home_or_outside_never_gets_a_chown() {
        let outside = tempfile::tempdir().unwrap();
        for target in [Path::new("/"), Path::new("HOME"), outside.path()] {
            let answer = linked_answer(target);
            assert!(
                !answer.line().contains("chown"),
                "{target:?}: {}",
                answer.line()
            );
            assert!(
                answer.next_step.contains("is a link to")
                    && answer.next_step.contains("which belongs to root")
                    && answer
                        .next_step
                        .contains("set RAFT_HOME to a folder you own"),
                "{target:?}: {}",
                answer.next_step
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_folder_inside_home_gets_the_chown() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().canonicalize().unwrap();
        let data = home_path.join("data");
        std::fs::create_dir(&data).unwrap();
        std::os::unix::fs::symlink(&data, home_path.join(".raft")).unwrap();
        let cfg = config(&home_path);
        let foreign = data.clone();
        let owner = move |p: &Path| Some(if p == foreign { 0 } else { 1000 });
        let look = accounts(&owner, &names);
        let answer = not_writable_for(&denied_at(&cfg.state_home), Some(&cfg), &look);
        assert_eq!(
            answer.next_step,
            "~/.raft is a link to ~/data. Fix the owner of that folder, then run the same install command again: sudo chown -R u ~/data"
        );
    }

    #[test]
    fn root_is_refused_only_in_another_users_home() {
        let name = |uid: u32| {
            if uid == 1000 {
                "u".to_string()
            } else {
                uid.to_string()
            }
        };
        let answer = root_refusal(0, Some(1000), name).unwrap();
        assert_eq!(
            answer.line(),
            "Don't run this as root: it would create files in u's home that only root can change. Run the same install command again without sudo."
        );
        assert_eq!(answer.code, FailureCode::RunningAsRoot);
        answer.reply("id-r").validate("id-r").unwrap();
        assert_eq!(root_refusal(0, Some(0), name), None);
        assert_eq!(root_refusal(1000, Some(1000), name), None);
        assert_eq!(root_refusal(0, None, name), None);
    }

    /// Permission denied that no check located: the whole state home is
    /// given back.
    #[cfg(unix)]
    #[test]
    fn an_unlocated_permission_failure_offers_the_state_home_chown() {
        let cfg = config(Path::new("/home/u"));
        let error = Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        let answer = answer(&error, Some(&cfg), None);
        assert_eq!(
            answer.message,
            "Raft Computer can't write to a folder inside ~/.raft: permission denied."
        );
        assert!(
            answer.next_step.starts_with(
                "Fix the owner, then run the same install command again: sudo chown -R "
            ),
            "{}",
            answer.next_step
        );
        assert!(answer.next_step.ends_with(" ~/.raft"));
    }

    /// `status` checks only what it always writes; an unwritable lock folder
    /// is found for every command.
    #[cfg(unix)]
    #[test]
    fn status_checks_only_the_folders_it_writes() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            eprintln!("skipped: root bypasses directory permissions");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let bin = cfg.installer_dir.join("bin");
        let claims = cfg.installer_dir.join("gate/upgrade.lock.claims");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&claims).unwrap();
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(&bin, 0o555);
        let status = blocked(&cfg, "status");
        let upgrade = blocked(&cfg, "upgrade");
        mode(&bin, 0o755);
        assert_eq!(status, None);
        assert_eq!(upgrade, Some(denied_at(&bin)));
        mode(&claims, 0o555);
        let status = blocked(&cfg, "status");
        mode(&claims, 0o755);
        assert_eq!(status, Some(denied_at(&claims)));
    }

    #[test]
    fn answer_reply_is_a_valid_failure_reply_with_a_next_step() {
        let home = Path::new("/home/u");
        let cfg = config(home);
        let blocked = Blocked {
            path: home.join(".raft"),
            read_only: false,
        };
        let error = Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        let answer = answer(&error, Some(&cfg), Some(&blocked));
        let reply = answer
            .reply("id-1")
            .with_diagnostic("Permission denied (os error 13)");
        reply.validate("id-1").unwrap();
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(value["exitCode"], 1);
        assert_eq!(value["code"], "installation_dir_not_writable");
        assert_eq!(value["reason"], answer.message.as_str());
        assert_eq!(value["nextStep"], answer.next_step.as_str());
        assert_eq!(value["line"], answer.line().as_str());
        assert!(
            answer
                .message
                .starts_with("Raft Computer can't write to ~/.raft: permission denied")
        );
        // The JSON reply round-trips through the strict reply type.
        let text = serde_json::to_string(&reply).unwrap();
        let parsed: Reply = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.next_step.as_deref(), Some(answer.next_step.as_str()));
    }

    #[test]
    fn an_unclassified_reply_uses_the_generic_code() {
        let reply = answer(&invalid("boom"), None, None).reply("id-2");
        reply.validate("id-2").unwrap();
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(value["code"], "unexpected_error");
        assert_eq!(value["nextStep"], "Run the same install command again.");
    }

    #[cfg(unix)]
    fn is_root() -> bool {
        current_uid() == 0
    }

    /// A temporary home whose `.raft` this user owns but cannot write
    /// (chmod 0555): the preflight finds it without creating anything, and the
    /// answer says how to make it writable. Root bypasses permissions.
    #[cfg(unix)]
    #[test]
    fn a_non_writable_state_home_is_found_before_anything_is_created() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            eprintln!("skipped: root bypasses directory permissions");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        std::fs::create_dir(&cfg.state_home).unwrap();
        std::fs::set_permissions(&cfg.state_home, std::fs::Permissions::from_mode(0o555)).unwrap();
        let found = blocked(&cfg, "status");
        std::fs::set_permissions(&cfg.state_home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            found,
            Some(Blocked {
                path: cfg.state_home.clone(),
                read_only: false
            })
        );
        assert!(
            !cfg.state_home.join("computer").exists(),
            "the check created a directory"
        );
        let user = current_user();
        let answer = not_writable(found.as_ref().unwrap(), Some(&cfg));
        assert_eq!(
            answer.message,
            format!(
                "Raft Computer can't write to ~/.raft: permission denied (it is not writable by {user})."
            )
        );
        assert_eq!(
            answer.next_step,
            "Make it writable, then run the same install command again: chmod u+w ~/.raft"
        );
        // A writable home has nothing blocked.
        assert_eq!(blocked(&cfg, "upgrade"), None);
    }

    /// A blocked folder inside the private state is shown and fixed as the
    /// state home, so the line never names internal folders.
    #[cfg(unix)]
    #[test]
    fn a_blocked_private_folder_is_shown_as_the_state_home() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            eprintln!("skipped: root bypasses directory permissions");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        std::fs::create_dir_all(&cfg.installer_dir).unwrap();
        std::fs::set_permissions(&cfg.installer_dir, std::fs::Permissions::from_mode(0o555))
            .unwrap();
        let found = blocked(&cfg, "status");
        std::fs::set_permissions(&cfg.installer_dir, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let found = found.unwrap();
        assert_eq!(found.path, cfg.installer_dir);
        let answer = not_writable(&found, Some(&cfg));
        assert!(
            answer.message.contains("can't write to ~/.raft:"),
            "{}",
            answer.message
        );
        assert!(!answer.line().to_lowercase().contains("installer"));
        assert_eq!(
            answer.next_step,
            "Make it writable, then run the same install command again: chmod -R u+w ~/.raft"
        );
    }

    #[test]
    fn a_folder_outside_home_is_named_with_how_to_fix_it() {
        let home = Path::new("/home/u");
        let mut cfg = config(home);
        cfg.state_home = PathBuf::from("/nonexistent-rci/rw");
        cfg.installer_dir = cfg.state_home.join("computer/installer");
        let blocked = Blocked {
            path: PathBuf::from("/nonexistent-rci"),
            read_only: false,
        };
        let answer = not_writable(&blocked, Some(&cfg));
        assert!(
            answer
                .message
                .starts_with("Raft Computer can't write to /nonexistent-rci: permission denied")
        );
        assert_eq!(
            answer.next_step,
            format!(
                "Make /nonexistent-rci writable by {}, then run the same install command again.",
                current_user()
            )
        );
    }
}
