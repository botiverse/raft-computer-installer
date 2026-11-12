//! A product service run by a systemd unit (system or `--user`) is that unit's
//! to stop and start. Computer up to 1.0.47 does not know about systemd: it
//! launches this installer inside the unit's control group and stops itself
//! with `raft-computer stop`. Under the default `KillMode=control-group` the
//! unit's stop then kills the installer too, and `Restart=always` brings the
//! old version back while the swap is in flight (or `Restart=on-failure`
//! leaves it stopped). So, on Linux: leave the unit's control group first
//! (`systemd-run --scope`), and stop/start the product through `systemctl`.
//! When the installer cannot leave, it refuses before stopping anything.
use crate::{Result, config::Config};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Set on the copy re-executed in its own scope, so it never tries again. To
/// the inside-Computer refusal it is only a hint to check the process tree
/// (`escaped_from_service`), never an exemption by itself.
pub const ESCAPED_ENV: &str = "RAFT_COMPUTER_INSTALLER_SCOPE";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceUnit {
    /// Unit of the per-user manager (`systemctl --user`).
    pub user: bool,
    pub name: String,
    /// The unit's control-group path; processes below it belong to the unit.
    pub cgroup: String,
}

/// The systemd control-group path in `/proc/<pid>/cgroup` (v2 `0::`, or the v1
/// `name=systemd` hierarchy).
pub fn cgroup_path(content: &str) -> Option<String> {
    let mut v1 = None;
    for line in content.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if id == "0" && controllers.is_empty() {
            return Some(path.to_string());
        }
        if controllers == "name=systemd" {
            v1 = Some(path.to_string());
        }
    }
    v1
}

/// The innermost `*.service` unit a control-group path belongs to, other than
/// a per-user manager (`user@UID.service`) itself.
pub fn unit_of_cgroup(path: &str) -> Option<ServiceUnit> {
    let parts: Vec<&str> = path.split('/').collect();
    let user_manager = parts
        .iter()
        .position(|part| part.starts_with("user@") && part.ends_with(".service"));
    let index = parts.iter().rposition(|part| {
        part.ends_with(".service") && !(part.starts_with("user@") && part.len() > ".service".len())
    })?;
    if user_manager.is_some_and(|manager| index <= manager) {
        return None;
    }
    let name = parts[index];
    if name == ".service" || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    Some(ServiceUnit {
        user: user_manager.is_some(),
        name: name.to_string(),
        cgroup: parts[..=index].join("/"),
    })
}

fn inside(path: &str, unit: &ServiceUnit) -> bool {
    path == unit.cgroup || path.starts_with(&format!("{}/", unit.cgroup))
}

#[cfg(target_os = "linux")]
fn cgroup_of(pid: u32) -> Option<String> {
    cgroup_path(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
}

#[cfg(not(target_os = "linux"))]
fn cgroup_of(_pid: u32) -> Option<String> {
    None
}

/// The parent pid from `/proc/<pid>/stat` (the field after the command name,
/// which may itself contain spaces and parentheses).
pub fn parent_from_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Shell interpreters a unit's `ExecStart=` script runs under.
pub fn is_shell_name(name: &str) -> bool {
    ["sh", "bash", "dash", "zsh", "ash", "ksh", "mksh", "busybox"].contains(&name)
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    parent_from_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "linux")]
fn is_shell(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .is_some_and(|name| is_shell_name(&name))
}

#[cfg(not(target_os = "linux"))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(target_os = "linux"))]
fn is_shell(_pid: u32) -> bool {
    false
}

/// The unit running the installed product's service, if any: a unit whose
/// main process is the product itself, or a shell script that started the
/// product as its direct child (`ExecStart=` a wrapper without `exec`). A
/// product started from inside some other service (a CI runner, a desktop
/// app's unit) is not that unit's to stop or start.
pub async fn product_unit(cfg: &Config) -> Result<Option<ServiceUnit>> {
    let processes = crate::host::installed_product_processes(cfg)?;
    for process in &processes {
        let Some(unit) = cgroup_of(process.pid).as_deref().and_then(unit_of_cgroup) else {
            continue;
        };
        let Some(main) = main_pid(&unit).await else {
            continue;
        };
        if processes.iter().any(|product| product.pid == main)
            || (parent_of(process.pid) == Some(main) && is_shell(main))
        {
            return Ok(Some(unit));
        }
    }
    Ok(None)
}

/// `systemctl [--user] show -p MainPID --value -- <unit>`; None when unknown.
async fn main_pid(unit: &ServiceUnit) -> Option<u32> {
    show(unit, "MainPID")
        .await?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

/// `systemctl [--user] show -p <property> --value -- <unit>`; None when unknown.
async fn show(unit: &ServiceUnit, property: &str) -> Option<String> {
    let mut command = command("systemctl", unit);
    command
        .args(["show", "-p", property, "--value", "--", unit.name.as_str()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The unit's actual `KillMode=`, read from its manager; None when unknown.
pub async fn kill_mode(unit: &ServiceUnit) -> Option<String> {
    show(unit, "KillMode").await.filter(|mode| !mode.is_empty())
}

/// Whether stopping a unit with this `KillMode=` leaves its other processes
/// (this one) running. `process` and `none` signal only the main process.
/// `mixed` does not qualify: SIGTERM goes to the main process, but once it
/// has exited (or the stop times out) every process left in the control
/// group gets SIGKILL, so the upgrade would still die with the service.
/// `control-group` (the default) and anything unknown kill everything.
pub fn kill_mode_spares_others(mode: &str) -> bool {
    matches!(mode.trim(), "process" | "none")
}

/// Whether this process would be stopped together with the unit.
pub fn self_inside(unit: &ServiceUnit) -> bool {
    cgroup_of(std::process::id()).is_some_and(|path| inside(&path, unit))
}

fn manager_args(unit: &ServiceUnit) -> Vec<String> {
    if unit.user {
        vec!["--user".into()]
    } else {
        vec![]
    }
}

fn command(program: &str, unit: &ServiceUnit) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.args(manager_args(unit));
    // A user manager is found through the runtime directory; a service it
    // started has one, a recovery run from elsewhere may not.
    #[cfg(unix)]
    if unit.user && std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        command.env(
            "XDG_RUNTIME_DIR",
            format!("/run/user/{}", unsafe { libc::getuid() }),
        );
    }
    command
}

/// `systemctl [--user] stop|start <unit>`, bounded.
pub async fn systemctl(unit: &ServiceUnit, action: &str) -> Result<()> {
    let mut command = command("systemctl", unit);
    command
        .args([action, "--", unit.name.as_str()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .map_err(|_| {
            k_carrier::error::invalid(format!("systemctl {action} {} timed out", unit.name))
        })??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr.lines().next().unwrap_or("").trim();
        return Err(k_carrier::error::invalid(format!(
            "systemctl {action} {} failed: {line}",
            unit.name
        )));
    }
    Ok(())
}

pub enum Escape {
    /// The copy is running in its own scope and carries the operation.
    Running,
    /// `systemd-run` (or the copy) exited within the first seconds, with what
    /// it wrote on stderr (sanitized and bounded; empty when not captured).
    Exited(Option<i32>, String),
    /// Nothing could be started, and why.
    Failed(String),
}

/// The longest escape note carried to the operation (see `Request`).
pub const ESCAPE_NOTE_LIMIT: usize = 4096;

/// Re-run this invocation in a transient scope outside the unit. A system
/// unit that runs the service as another user than root cannot create a
/// scope on the system manager (polkit asks for an administrator, and no
/// agent answers): that user's own manager is tried once instead, when it
/// is reachable. `scratch` holds the (unlinked) capture of stderr.
pub async fn escape(unit: &ServiceUnit, args: &[String], scratch: &std::path::Path) -> Escape {
    let system = match escape_with(unit.user, args, scratch).await {
        Escape::Exited(Some(code), stderr)
            if code != 0 && !unit.user && scope_denied(&stderr) && !is_root() =>
        {
            if !user_manager_reachable().await {
                return Escape::Exited(Some(code), stderr);
            }
            stderr
        }
        other => return other,
    };
    // Bounded by escape_note()/failure_note() in cli.rs.
    let both = |user: String| format!("{system} Then with --user: {user}");
    match escape_with(true, args, scratch).await {
        Escape::Exited(code, user) if code != Some(0) => Escape::Exited(code, both(user)),
        Escape::Failed(user) => Escape::Failed(both(user)),
        other => other,
    }
}

async fn escape_with(user: bool, args: &[String], scratch: &std::path::Path) -> Escape {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return Escape::Failed(format!("own executable unknown: {error}")),
    };
    let manager = ServiceUnit {
        user,
        name: String::new(),
        cgroup: String::new(),
    };
    let mut command = command("systemd-run", &manager);
    command
        .args(["--scope", "--quiet", "--collect"])
        .arg("--")
        .arg(executable)
        .args(args)
        .env(ESCAPED_ENV, "1")
        .stdin(std::process::Stdio::null());
    // systemd-run execs the copy in place, so this is the copy's stderr too.
    // A file, not a pipe: the copy outlives this process, and a pipe without
    // a reader would fail its writes. It is unlinked, so nothing is left.
    let capture = k_carrier::storage::ensure_dir(scratch)
        .ok()
        .and_then(|()| tempfile::tempfile_in(scratch).ok());
    if let Some(stderr) = capture.as_ref().and_then(|file| file.try_clone().ok()) {
        command.stderr(stderr);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Escape::Failed(format!("systemd-run could not be started: {error}")),
    };
    // systemd-run fails fast when it cannot create the scope (no manager,
    // not permitted); a copy still running after that is in its scope. A
    // copy that already finished may have written this request's receipt.
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => Escape::Exited(status.code(), capture.map(replay).unwrap_or_default()),
        Ok(Err(error)) => Escape::Failed(format!("systemd-run could not be awaited: {error}")),
        Err(_) => Escape::Running,
    }
}

/// What the exited systemd-run (or copy) wrote on stderr: passed on to this
/// process's own stderr as it was, and returned sanitized for the receipt.
fn replay(mut file: std::fs::File) -> String {
    use std::io::{Read, Seek, Write};
    let mut bytes = Vec::new();
    if file.seek(std::io::SeekFrom::Start(0)).is_err()
        || file.take(65536).read_to_end(&mut bytes).is_err()
    {
        return String::new();
    }
    let _ = std::io::stderr().write_all(&bytes);
    crate::computer::diagnostic_stderr(&bytes).0
}

/// systemd-run's own refusal to create the scope for lack of permission
/// (polkit `auth_admin` without an agent, or a denied D-Bus call). Only its
/// "Failed to start transient scope unit" line counts, never anything the
/// copy itself may have printed.
pub fn scope_denied(stderr: &str) -> bool {
    stderr
        .split("Failed to start transient scope unit:")
        .skip(1)
        .any(|rest| {
            let rest = rest.to_ascii_lowercase();
            [
                "authentication required",
                "access denied",
                "permission denied",
                "not authorized",
            ]
            .iter()
            .any(|denial| rest.contains(denial))
        })
}

/// Whether this user's own manager answers (`systemctl --user`).
async fn user_manager_reachable() -> bool {
    let manager = ServiceUnit {
        user: true,
        name: String::new(),
        cgroup: String::new(),
    };
    let mut command = command("systemctl", &manager);
    command
        .args(["show", "-p", "Version", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    matches!(
        tokio::time::timeout(Duration::from_secs(15), command.output()).await,
        Ok(Ok(output)) if output.status.success() && !output.stdout.trim_ascii().is_empty()
    )
}

#[cfg(unix)]
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// At most `limit` bytes of `text`, cut at a character boundary.
pub fn clip(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The note on a failed escape, for the held receipt's reason.
pub fn escape_note(code: Option<i32>, stderr: &str) -> String {
    let status = code.map_or_else(
        || "was killed by a signal".to_string(),
        |code| format!("exited with status {code}"),
    );
    let stderr = stderr.trim();
    let note = if stderr.is_empty() {
        format!("systemd-run --scope {status}.")
    } else {
        format!("systemd-run --scope {status}: {stderr}")
    };
    clip(note, ESCAPE_NOTE_LIMIT)
}

/// The note when systemd-run could not even be started, sanitized as its
/// stderr would be.
pub fn failure_note(detail: &str) -> String {
    clip(
        crate::computer::diagnostic_stderr(detail.as_bytes()).0,
        ESCAPE_NOTE_LIMIT,
    )
}

/// The Computer process that started the run this copy was escaped from.
///
/// `escape()` re-runs the installer under `systemd-run --scope`, which execs
/// the copy in place: the copy's parent is the original run, and that run's
/// parent the service. The copy then sees a Computer above it and no Computer
/// as its immediate parent, and would refuse an upgrade the original accepted.
/// It gets the original's verdict instead, by standing in the original's own
/// parent, only when the tree proves what it is: its parent runs the very
/// same executable bytes (sha256 of `/proc/<pid>/exe`), and that parent's
/// parent runs a product executable. `ESCAPED_ENV` (`hint`) only says to
/// look; anyone can set it, so it never decides.
pub fn escaped_from_service(cfg: &Config, hint: bool) -> Result<Option<crate::process::Identity>> {
    escaped_original_parent(
        hint,
        std::process::id(),
        &crate::host::product_executables(cfg),
        crate::process::parent_of,
        crate::process::observe,
        executable_digest,
    )
}

#[cfg(target_os = "linux")]
fn executable_digest(pid: u32) -> Option<String> {
    let path = if pid == std::process::id() {
        "/proc/self/exe".to_string()
    } else {
        format!("/proc/{pid}/exe")
    };
    std::fs::read(path)
        .ok()
        .map(|bytes| k_carrier::artifact::sha256(&bytes))
}

/// The scope escape exists on Linux only.
#[cfg(not(target_os = "linux"))]
fn executable_digest(_pid: u32) -> Option<String> {
    None
}

/// The walk behind `escaped_from_service`, over injectable process queries.
pub(crate) fn escaped_original_parent<P, O, D>(
    hint: bool,
    own: u32,
    products: &[std::path::PathBuf],
    mut parent_of: P,
    mut observe: O,
    mut digest: D,
) -> Result<Option<crate::process::Identity>>
where
    P: FnMut(u32) -> Result<Option<u32>>,
    O: FnMut(u32) -> Result<Option<crate::process::Identity>>,
    D: FnMut(u32) -> Option<String>,
{
    use crate::process::{created_after, same_path};
    if !hint {
        return Ok(None);
    }
    let Some(copy) = observe(own)? else {
        return Ok(None);
    };
    let Some(original_pid) = parent_of(own)? else {
        return Ok(None);
    };
    let Some(original) = observe(original_pid)? else {
        return Ok(None);
    };
    // A "parent" created after its child is a reused PID, not the parent.
    if created_after(&original, &copy) == Some(true) {
        return Ok(None);
    }
    let (Some(mine), Some(theirs)) = (digest(own), digest(original_pid)) else {
        return Ok(None);
    };
    if mine != theirs {
        return Ok(None);
    }
    let Some(service_pid) = parent_of(original_pid)? else {
        return Ok(None);
    };
    let Some(service) = observe(service_pid)? else {
        return Ok(None);
    };
    if created_after(&service, &original) == Some(true)
        || !products
            .iter()
            .any(|product| same_path(&service.executable, product))
    {
        return Ok(None);
    }
    // The tree must not have changed while it was read.
    if parent_of(own)? != Some(original_pid)
        || parent_of(original_pid)? != Some(service_pid)
        || observe(original_pid)?.as_ref() != Some(&original)
    {
        return Ok(None);
    }
    Ok(Some(service))
}

/// The refusal when the product's unit would stop this installer with it.
pub fn confinement_reason(unit: &ServiceUnit) -> String {
    let manager = if unit.user {
        "systemctl --user"
    } else {
        "systemctl"
    };
    format!(
        "The Computer service runs in systemd unit {name}, which would stop the upgrade together with the service, and the upgrade could not move out of it. Upgrade from the host: `{manager} stop {name}`, run the install command, then `{manager} start {name}`.",
        name = unit.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_and_user_units_are_found_and_the_user_manager_is_not_a_unit() {
        assert_eq!(
            unit_of_cgroup("/system.slice/raft-computer.service"),
            Some(ServiceUnit {
                user: false,
                name: "raft-computer.service".into(),
                cgroup: "/system.slice/raft-computer.service".into(),
            })
        );
        assert_eq!(
            unit_of_cgroup("/user.slice/user-1001.slice/user@1001.service/app.slice/raft.service"),
            Some(ServiceUnit {
                user: true,
                name: "raft.service".into(),
                cgroup: "/user.slice/user-1001.slice/user@1001.service/app.slice/raft.service"
                    .into(),
            })
        );
        assert_eq!(
            unit_of_cgroup(
                "/user.slice/user-1001.slice/user@1001.service/app.slice/app-gnome-term.scope"
            ),
            None,
            "a terminal session"
        );
        assert_eq!(
            unit_of_cgroup("/user.slice/user-1001.slice/user@1001.service/init.scope"),
            None
        );
        assert_eq!(unit_of_cgroup("/init.scope"), None);
        assert_eq!(
            unit_of_cgroup("/user.slice/user-1001.slice/session-3.scope"),
            None
        );
    }

    #[test]
    fn processes_below_a_unit_belong_to_it() {
        let unit = unit_of_cgroup("/system.slice/raft-computer.service").unwrap();
        assert!(inside("/system.slice/raft-computer.service", &unit));
        assert!(inside("/system.slice/raft-computer.service/sub", &unit));
        assert!(!inside("/system.slice/raft-computer.service2", &unit));
        assert!(!inside("/system.slice/run-r1.scope", &unit));
    }

    #[test]
    fn cgroup_files_of_both_hierarchies_are_read() {
        assert_eq!(
            cgroup_path("0::/system.slice/raft-computer.service\n").as_deref(),
            Some("/system.slice/raft-computer.service")
        );
        assert_eq!(
            cgroup_path("12:cpu,cpuacct:/x\n1:name=systemd:/system.slice/a.service\n").as_deref(),
            Some("/system.slice/a.service")
        );
        assert_eq!(cgroup_path(""), None);
    }

    #[test]
    fn a_wrapper_shell_is_recognized_by_its_parent_pid_and_name() {
        assert_eq!(
            parent_from_stat("4242 (raft-computer) S 4100 4242 4242 0 -1"),
            Some(4100)
        );
        assert_eq!(
            parent_from_stat("4242 (a b) c)) S 77 1 1"),
            Some(77),
            "a command name with spaces and parentheses"
        );
        assert_eq!(parent_from_stat("garbage"), None);
        assert!(is_shell_name("bash") && is_shell_name("sh") && is_shell_name("dash"));
        assert!(
            !is_shell_name("node")
                && !is_shell_name("Runner.Listener")
                && !is_shell_name("raft-desktop")
        );
    }

    #[test]
    fn only_kill_modes_that_spare_other_processes_let_the_upgrade_stay() {
        assert!(kill_mode_spares_others("process"));
        assert!(kill_mode_spares_others("none\n"));
        // mixed: SIGKILL reaches the rest of the control group after the
        // main process; control-group kills everything.
        assert!(!kill_mode_spares_others("mixed"));
        assert!(!kill_mode_spares_others("control-group"));
        assert!(!kill_mode_spares_others(""));
        assert!(!kill_mode_spares_others("Process"));
    }

    #[test]
    fn only_systemd_runs_own_permission_refusal_counts_as_denied() {
        assert!(scope_denied(
            "Failed to start transient scope unit: Interactive authentication required."
        ));
        assert!(scope_denied("Failed to start transient scope unit: Access denied"));
        assert!(!scope_denied(
            "Failed to start transient scope unit: Unit run-r1.scope already exists."
        ));
        assert!(
            !scope_denied("open /home/raft/x: permission denied"),
            "the copy's own output"
        );
        assert!(!scope_denied(""));
    }

    #[test]
    fn an_escape_note_is_bounded_and_says_how_systemd_run_ended() {
        assert_eq!(
            escape_note(Some(1), "  Failed to start transient scope unit: Access denied "),
            "systemd-run --scope exited with status 1: Failed to start transient scope unit: Access denied"
        );
        assert_eq!(
            escape_note(None, ""),
            "systemd-run --scope was killed by a signal."
        );
        let long = escape_note(Some(2), &"é".repeat(ESCAPE_NOTE_LIMIT));
        assert!(long.len() <= ESCAPE_NOTE_LIMIT);
        assert_eq!(clip("abc".into(), 2), "ab");
        assert_eq!(clip("é".into(), 1), "");
    }

    mod escaped_copy {
        use super::super::escaped_original_parent;
        use crate::process::{Identity, test_creation};
        use std::{collections::BTreeMap, path::PathBuf};

        const COPY: u32 = 900;
        const ORIGINAL: u32 = 800;
        const SERVICE: u32 = 708;
        const PRODUCT: &str = "/home/raft/.local/bin/raft-computer";

        struct Tree {
            /// pid -> (parent pid, executable, creation instant, digest)
            processes: BTreeMap<u32, (u32, &'static str, u64, &'static str)>,
        }

        impl Tree {
            /// copy -> original -> service, as escape() leaves it.
            fn escaped() -> Self {
                Self {
                    processes: BTreeMap::from([
                        (COPY, (ORIGINAL, "/tmp/rci", 300, "same")),
                        (ORIGINAL, (SERVICE, "/tmp/rci", 200, "same")),
                        (SERVICE, (1, PRODUCT, 100, "product")),
                    ]),
                }
            }

            fn set(
                mut self,
                pid: u32,
                parent: u32,
                exe: &'static str,
                digest: &'static str,
            ) -> Self {
                let created = self.processes.get(&pid).map_or(150, |p| p.2);
                self.processes.insert(pid, (parent, exe, created, digest));
                self
            }

            fn resolve(&self, hint: bool) -> Option<u32> {
                escaped_original_parent(
                    hint,
                    COPY,
                    &[PathBuf::from(PRODUCT)],
                    |pid| Ok(self.processes.get(&pid).map(|p| p.0)),
                    |pid| {
                        Ok(self.processes.get(&pid).map(|p| Identity {
                            pid,
                            executable: PathBuf::from(p.1),
                            created: test_creation(p.2),
                        }))
                    },
                    |pid| self.processes.get(&pid).map(|p| p.3.to_string()),
                )
                .unwrap()
                .map(|service| service.pid)
            }
        }

        #[test]
        fn a_same_digest_parent_started_by_the_service_stands_in_the_service() {
            assert_eq!(Tree::escaped().resolve(true), Some(SERVICE));
        }

        #[test]
        fn without_the_hint_nothing_changes() {
            assert_eq!(Tree::escaped().resolve(false), None);
        }

        #[test]
        fn a_parent_with_other_bytes_is_not_the_original_even_with_the_hint() {
            // An agent's own installer (another download) or a renamed copy.
            let tree = Tree::escaped().set(ORIGINAL, SERVICE, "/tmp/rci", "other");
            assert_eq!(tree.resolve(true), None);
        }

        #[test]
        fn an_agents_shell_is_not_the_original_even_with_the_hint() {
            // copy -> agent shell -> Computer: the #32 refusal must hold.
            let tree = Tree::escaped().set(ORIGINAL, SERVICE, "/bin/sh", "shell");
            assert_eq!(tree.resolve(true), None);
        }

        #[test]
        fn a_same_digest_parent_not_started_by_a_computer_is_not_exempt() {
            // copy -> same bytes -> agent shell -> Computer: an agent running
            // the installer that runs itself again with the hint.
            let tree = Tree::escaped()
                .set(ORIGINAL, 40, "/tmp/rci", "same")
                .set(40, SERVICE, "/bin/sh", "shell");
            assert_eq!(tree.resolve(true), None);
        }

        #[test]
        fn unreadable_bytes_are_not_the_same_bytes() {
            let tree = Tree::escaped();
            let unreadable = escaped_original_parent(
                true,
                COPY,
                &[PathBuf::from(PRODUCT)],
                |pid| Ok(tree.processes.get(&pid).map(|p| p.0)),
                |pid| {
                    Ok(tree.processes.get(&pid).map(|p| Identity {
                        pid,
                        executable: PathBuf::from(p.1),
                        created: test_creation(p.2),
                    }))
                },
                |pid| (pid == COPY).then(|| "same".to_string()),
            )
            .unwrap();
            assert_eq!(unreadable, None);
        }

        #[test]
        fn a_reused_parent_pid_newer_than_the_copy_is_not_the_original() {
            let mut tree = Tree::escaped();
            tree.processes.get_mut(&ORIGINAL).unwrap().2 = 400;
            assert_eq!(tree.resolve(true), None);
        }
    }

    #[test]
    fn the_refusal_names_the_unit_and_the_manager() {
        let user =
            unit_of_cgroup("/user.slice/user-1.slice/user@1.service/app.slice/rc.service").unwrap();
        assert!(confinement_reason(&user).contains("systemctl --user stop rc.service"));
        assert!(!confinement_reason(&user).chars().any(char::is_control));
    }
}
