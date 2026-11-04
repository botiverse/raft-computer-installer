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

/// The directories an operation writes to first. Checking them only reads
/// metadata: nothing is created, so `status` stays read-only here.
fn targets(cfg: &Config, installs: bool) -> Vec<PathBuf> {
    let mut targets = vec![
        cfg.scratch().join("supervisors"),
        cfg.installer_dir.join("receipts"),
        cfg.installer_dir.join("diagnostics"),
        cfg.installer_dir.join("bin"),
        cfg.k_state.clone(),
    ];
    if installs {
        targets.push(cfg.install_dir.clone());
    }
    targets
}

/// The first state directory (or its nearest existing ancestor) this user
/// cannot write, if any.
pub fn blocked(cfg: &Config, installs: bool) -> Option<Blocked> {
    targets(cfg, installs)
        .iter()
        .find_map(|target| blocked_on_the_way(target))
}

/// The nearest existing ancestor of `target` (inclusive) decides whether it
/// can be created or written; an unsearchable ancestor shows up as that
/// ancestor's own missing search permission.
#[cfg(unix)]
pub fn blocked_on_the_way(target: &Path) -> Option<Blocked> {
    use std::os::unix::ffi::OsStrExt;
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
    let path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a valid NUL-terminated string for the call's duration.
    if unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0 {
        return None;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => Some(Blocked {
            path: existing.to_path_buf(),
            read_only: false,
        }),
        Some(libc::EROFS) => Some(Blocked {
            path: existing.to_path_buf(),
            read_only: true,
        }),
        _ => None,
    }
}

#[cfg(not(unix))]
pub fn blocked_on_the_way(_target: &Path) -> Option<Blocked> {
    None
}

/// The directory to give back to the user: the topmost one between the home
/// (exclusive) and `blocked` (inclusive) that the user does not own. Never the
/// home itself or anything outside it; `None` when every one is owned.
pub fn chown_target(blocked: &Path, home: &Path, owned: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let relative = blocked.strip_prefix(home).ok()?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if !owned(&current) {
            return Some(current);
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
/// internal folders are not something the user needs to know about.
fn shown(path: &Path, cfg: Option<&Config>) -> PathBuf {
    match cfg {
        Some(cfg) if path.starts_with(&cfg.installer_dir) => cfg.state_home.clone(),
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

/// The answer for a directory this user cannot write.
pub fn not_writable(blocked: &Blocked, cfg: Option<&Config>) -> Answer {
    let path = shown(&blocked.path, cfg);
    let user = current_user();
    if blocked.read_only {
        return Answer {
            message: format!(
                "Raft Computer can't write to {}: the file system is read-only.",
                display(&path, cfg)
            ),
            next_step: "Make it writable, or set RAFT_HOME to a writable folder, then run the same install command again.".into(),
            code: FailureCode::InstallationDirNotWritable,
            retry_helps: false,
        };
    }
    #[cfg(unix)]
    {
        let uid = current_uid();
        let why = match owner_of(&blocked.path) {
            Some(owner) if owner != uid => format!("it belongs to {}", account(owner)),
            _ => format!("it is not writable by {user}"),
        };
        let message = format!(
            "Raft Computer can't write to {}: permission denied ({why}).",
            display(&path, cfg)
        );
        if let Some(cfg) = cfg
            && path.starts_with(&cfg.user_home)
            && path != cfg.user_home
        {
            let owned = |p: &Path| owner_of(p) == Some(uid);
            let next_step = match chown_target(&blocked.path, &cfg.user_home, owned) {
                Some(target) => format!(
                    "Fix the owner, then run the same install command again: sudo chown -R {} {}",
                    crate::shell_path::shell_word(&user),
                    word(&shown(&target, Some(cfg)), Some(cfg))
                ),
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
    #[cfg(not(unix))]
    Answer {
        message: format!(
            "Raft Computer can't write to {}: permission denied.",
            display(&path, cfg)
        ),
        next_step: format!(
            "Make {} writable by {user}, then run the same install command again.",
            display(&path, cfg)
        ),
        code: FailureCode::InstallationDirNotWritable,
        retry_helps: false,
    }
}

/// The answer for an error that reached main(). `blocked` is the state
/// directory found unwritable, if any: an I/O error does not carry its path.
pub fn answer(error: &Error, cfg: Option<&Config>, blocked: Option<&Blocked>) -> Answer {
    let cause = classify(error);
    let answer = match (cause, blocked) {
        (Cause::PermissionDenied | Cause::ReadOnly, Some(blocked)) => not_writable(blocked, cfg),
        (Cause::PermissionDenied, None) => Answer {
            message: "Raft Computer can't write to a folder it needs: permission denied.".into(),
            next_step: match cfg {
                Some(cfg) => format!(
                    "Make {} writable by {}, then run the same install command again.",
                    display(&cfg.state_home, Some(cfg)),
                    current_user()
                ),
                None => format!(
                    "Make your home folder writable by {}, then run the same install command again.",
                    current_user()
                ),
            },
            code: FailureCode::InstallationDirNotWritable,
            retry_helps: false,
        },
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
        (Cause::PermissionDenied | Cause::ReadOnly, Some(cfg)) => blocked(cfg, true),
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
        let foreign = [
            Path::new("/home/u/.raft"),
            Path::new("/home/u/.raft/computer"),
        ];
        let owned = |p: &Path| !foreign.contains(&p);
        assert_eq!(
            chown_target(blocked, home, owned),
            Some(PathBuf::from("/home/u/.raft"))
        );
        // Only a deeper directory is foreign.
        let owned = |p: &Path| p != Path::new("/home/u/.raft/computer");
        assert_eq!(
            chown_target(blocked, home, owned),
            Some(PathBuf::from("/home/u/.raft/computer"))
        );
        // Everything owned: nothing to chown (a mode problem).
        assert_eq!(chown_target(blocked, home, |_| true), None);
        // Never the home itself, nor anything outside it.
        assert_eq!(chown_target(home, home, |_| false), None);
        assert_eq!(chown_target(Path::new("/srv/rw"), home, |_| false), None);
        assert_eq!(chown_target(Path::new("/"), home, |_| false), None);
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
        let found = blocked(&cfg, false);
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
        assert_eq!(blocked(&cfg, true), None);
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
        let found = blocked(&cfg, false);
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
