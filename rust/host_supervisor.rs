//! A Computer service registered with the per-user OS supervisor by Raft
//! Desktop: a launchd agent, a systemd `--user` unit, or a Windows logon task
//! running `raft-computer-launch.exe`. The registration is format 2 of
//! `<state home>/computer/host-lifecycle-owner.json`, a cross-repository
//! contract (slock `docs/contracts/computer-os-manager-retirement.md`, fixture
//! `contract/host-lifecycle-registration.v2.json`).
//!
//! While a registration exists the service is that supervisor's to stop and
//! start. Stopping it with `raft-computer stop` beside the supervisor would
//! let the supervisor relaunch the old version in the middle of the swap; a
//! start beside it would leave a second service the supervisor does not
//! track. A registration this installer cannot read is therefore never "not
//! registered": the operation is held before anything is touched.
use crate::{Result, config::Config, version};
use k_carrier::error::invalid;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cmp::Ordering, ffi::OsString, fs, path::PathBuf, time::Duration};

/// Relative to the state home (`RAFT_HOME`).
pub const REGISTRATION_FILE: [&str; 2] = ["computer", "host-lifecycle-owner.json"];
const MAX_REGISTRATION_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    LaunchdAgent,
    SystemdUser,
    WindowsTask,
}

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "launchd-agent" => Some(Self::LaunchdAgent),
            "systemd-user" => Some(Self::SystemdUser),
            "windows-task" => Some(Self::WindowsTask),
            _ => None,
        }
    }

    /// The only kind each platform registers.
    fn native() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::LaunchdAgent)
        } else if cfg!(target_os = "linux") {
            Some(Self::SystemdUser)
        } else if cfg!(windows) {
            Some(Self::WindowsTask)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostSupervisor {
    pub kind: Kind,
    /// launchd label, systemd unit, or Task Scheduler path.
    pub name: String,
    pub definition_path: Option<PathBuf>,
    pub binary_path: PathBuf,
    pub launcher_path: Option<PathBuf>,
    pub min_computer_version: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Registration {
    /// No owner record, one that is not a JSON object with a format version
    /// (today's CLI treats it the same), a format 1 record, format 2 without
    /// a supervisor, or a registration whose definition is gone (stale; the
    /// next `raft-computer start` clears it).
    Absent,
    Registered(HostSupervisor),
    /// A valid record this installer does not understand (another format
    /// version, a malformed format 2 `supervisor`), an unreadable file, or a
    /// service manager that could not be asked about the definition. Treated
    /// as registered: the operation is held.
    Unknown(String),
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn plain(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

/// Absolute in either path syntax, whatever this host is: the record names
/// paths of the platform that wrote it, and must parse the same everywhere.
fn absolute(value: Option<String>) -> Option<PathBuf> {
    let value = value.filter(|v| plain(v))?;
    let bytes = value.as_bytes();
    let posix = bytes.first() == Some(&b'/');
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = value.starts_with("\\\\");
    (posix || drive || unc).then(|| PathBuf::from(value))
}

/// Parse the owner record. Unknown top-level and supervisor fields are
/// ignored (later additive fields); every field this installer acts on must
/// be present and valid, otherwise the record is `Unknown`.
pub fn parse(bytes: &[u8]) -> Registration {
    // Not a JSON object with a format version: not a record any Computer
    // reads as a registration (contract: corrupt is unregistered).
    let Ok(record) = serde_json::from_slice::<Value>(bytes) else {
        return Registration::Absent;
    };
    // A non-numeric version is a corrupt record; a number this installer
    // does not know (3, 2.5) is a valid record of another version: held.
    let Some(number) = record.get("formatVersion").filter(|v| v.is_number()) else {
        return Registration::Absent;
    };
    let Some(format) = number.as_u64() else {
        return Registration::Unknown(format!("owner record format {number}"));
    };
    match format {
        1 => return Registration::Absent,
        2 => {}
        other => return Registration::Unknown(format!("owner record format {other}")),
    }
    let supervisor = match record.get("supervisor") {
        None | Some(Value::Null) => return Registration::Absent,
        Some(value) if value.is_object() => value,
        Some(_) => return Registration::Unknown("the supervisor entry is not an object".into()),
    };
    let Some(kind) = text(supervisor, "kind").as_deref().and_then(Kind::parse) else {
        return Registration::Unknown("unknown supervisor kind".into());
    };
    if text(supervisor, "registeredBy").as_deref() != Some("desktop") {
        return Registration::Unknown("the supervisor was not registered by Raft Desktop".into());
    }
    let Some(name) = text(supervisor, "name").filter(|v| plain(v)) else {
        return Registration::Unknown("the supervisor has no name".into());
    };
    let Some(binary_path) = absolute(text(supervisor, "binaryPath")) else {
        return Registration::Unknown("the supervisor has no absolute binary path".into());
    };
    let definition_path = absolute(text(supervisor, "definitionPath"));
    if kind != Kind::WindowsTask && definition_path.is_none() {
        return Registration::Unknown("the supervisor has no definition path".into());
    }
    let launcher_path = absolute(text(supervisor, "launcherPath"));
    if kind == Kind::WindowsTask && launcher_path.is_none() {
        return Registration::Unknown("the scheduled task has no launcher path".into());
    }
    let Some(min_computer_version) =
        text(supervisor, "minComputerVersion").filter(|v| version::exact(v).is_ok())
    else {
        return Registration::Unknown(
            "the supervisor has no valid minimum Computer version".into(),
        );
    };
    Registration::Registered(HostSupervisor {
        kind,
        name,
        definition_path,
        binary_path,
        launcher_path,
        min_computer_version,
    })
}

pub fn registration_path(cfg: &Config) -> PathBuf {
    REGISTRATION_FILE
        .iter()
        .fold(cfg.state_home.clone(), |path, part| path.join(part))
}

/// Same path, whatever the spelling: canonical when it exists, otherwise
/// lexical; Windows verbatim prefix and case ignored.
pub fn normalized_path(path: &std::path::Path) -> String {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();
    if cfg!(windows) || text.contains('\\') || text.get(1..2) == Some(":") {
        text.replace('/', "\\").to_ascii_lowercase()
    } else {
        text
    }
}

/// The registration, with its definition confirmed in the OS manager before
/// anything is stopped (slock `supervisorDefinitionPresent`, same rules):
/// - the plist or unit file exists, or `schtasks /Query` finds the task:
///   registered;
/// - nothing at all (no file, no loaded job): a stale registration, Absent,
///   exactly as slock's `raft-computer start` treats it (it clears the record
///   and starts self-managed);
/// - the file is gone but launchd / systemd still has the job loaded: held
///   (Unknown). It is not self-managed (the manager would relaunch it), and
///   once stopped it could not be started again without its definition;
/// - a manager that cannot be asked: held (Unknown).
pub async fn resolve(cfg: &Config) -> Registration {
    let registration = read(cfg);
    let Registration::Registered(supervisor) = &registration else {
        return registration;
    };
    match definition_state(supervisor).await {
        Some(DefinitionState::Present) => registration,
        Some(DefinitionState::Absent) => Registration::Absent,
        Some(DefinitionState::LoadedWithoutFile) => Registration::Unknown(
            "the service manager still runs the registered job, but its definition file is gone"
                .into(),
        ),
        None => Registration::Unknown(
            "the service manager could not be asked about the registered definition".into(),
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinitionState {
    Present,
    /// No file, but the manager has the job loaded (launchd / systemd).
    LoadedWithoutFile,
    Absent,
}

/// Where the registered definition stands; None when the manager cannot be asked.
pub async fn definition_state(supervisor: &HostSupervisor) -> Option<DefinitionState> {
    match supervisor.kind {
        Kind::WindowsTask => {
            let (found, _) =
                query("schtasks", &["/Query", "/TN", supervisor.name.as_str()]).await?;
            Some(if found {
                DefinitionState::Present
            } else {
                DefinitionState::Absent
            })
        }
        Kind::LaunchdAgent | Kind::SystemdUser => {
            let file = supervisor.definition_path.as_ref()?;
            if fs::metadata(file).is_ok_and(|meta| meta.is_file()) {
                return Some(DefinitionState::Present);
            }
            let loaded = if supervisor.kind == Kind::LaunchdAgent {
                query(
                    "launchctl",
                    &["print", &launchd_target(supervisor, current_uid())],
                )
                .await?
                .0
            } else {
                let (ok, out) = query(
                    "systemctl",
                    &[
                        "--user",
                        "show",
                        "-p",
                        "LoadState",
                        "--value",
                        "--",
                        &supervisor.name,
                    ],
                )
                .await?;
                ok && out.trim() == "loaded"
            };
            Some(if loaded {
                DefinitionState::LoadedWithoutFile
            } else {
                DefinitionState::Absent
            })
        }
    }
}

/// Whether the supervisor is enabled (it will start the service at the next
/// login). None when not applicable (systemd: the installer only stops a
/// unit, it never disables it) or when the manager cannot be asked.
pub async fn supervisor_enabled(supervisor: &HostSupervisor) -> Option<bool> {
    match supervisor.kind {
        Kind::SystemdUser => None,
        Kind::LaunchdAgent => {
            let (ok, out) = query(
                "launchctl",
                &["print-disabled", &format!("gui/{}", current_uid())],
            )
            .await?;
            ok.then(|| !launchd_disabled(&out, &supervisor.name))
        }
        Kind::WindowsTask => {
            let (ok, out) = query(
                "schtasks",
                &[
                    "/Query",
                    "/TN",
                    supervisor.name.as_str(),
                    "/FO",
                    "CSV",
                    "/NH",
                ],
            )
            .await?;
            ok.then(|| !out.contains("\"Disabled\""))
        }
    }
}

/// `launchctl print-disabled` lists `"label" => disabled` (macOS 13+) or
/// `"label" => true` (older); a label it does not list is enabled.
pub fn launchd_disabled(output: &str, label: &str) -> bool {
    let needle = format!("\"{label}\" =>");
    output.lines().any(|line| {
        let line = line.trim();
        line.starts_with(&needle)
            && matches!(
                line[needle.len()..].trim().trim_end_matches(','),
                "disabled" | "true"
            )
    })
}

/// One bounded query of the service manager: Some((succeeded, stdout)), or
/// None when it could not be run at all.
pub async fn query(program: &str, args: &[&str]) -> Option<(bool, String)> {
    #[cfg(test)]
    if let Some(answer) = test_manager::answer(program, args) {
        return answer;
    }
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    #[cfg(target_os = "linux")]
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", current_uid()));
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .ok()?
        .ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// Test seam: answers manager queries and records every supervisor command,
/// so a test can prove what was (not) stopped. Per thread (tokio tests run on
/// the current thread).
#[cfg(test)]
pub mod test_manager {
    use std::cell::RefCell;
    type Answer = Box<dyn Fn(&str, &[&str]) -> Option<(bool, String)>>;
    thread_local! {
        static ANSWER: RefCell<Option<Answer>> = const { RefCell::new(None) };
        static COMMANDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }
    pub fn install(answer: impl Fn(&str, &[&str]) -> Option<(bool, String)> + 'static) {
        ANSWER.with(|cell| *cell.borrow_mut() = Some(Box::new(answer)));
        COMMANDS.with(|cell| cell.borrow_mut().clear());
    }
    pub fn commands() -> Vec<String> {
        COMMANDS.with(|cell| cell.borrow().clone())
    }
    pub(super) fn answer(program: &str, args: &[&str]) -> Option<Option<(bool, String)>> {
        ANSWER.with(|cell| cell.borrow().as_ref().map(|answer| answer(program, args)))
    }
    thread_local! {
        static FAIL_COMMANDS: RefCell<bool> = const { RefCell::new(false) };
    }
    /// Make every recorded supervisor command fail.
    pub fn fail_commands(fail: bool) {
        FAIL_COMMANDS.with(|cell| *cell.borrow_mut() = fail);
    }
    /// Some(result) when the seam is installed (the command is recorded, not run).
    pub(super) fn record(line: String) -> Option<bool> {
        let installed = ANSWER.with(|cell| cell.borrow().is_some());
        if !installed {
            return None;
        }
        COMMANDS.with(|cell| cell.borrow_mut().push(line));
        Some(!FAIL_COMMANDS.with(|cell| *cell.borrow()))
    }
}

pub fn read(cfg: &Config) -> Registration {
    let path = registration_path(cfg);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Registration::Absent,
        Err(error) => {
            return Registration::Unknown(format!("the owner record is unreadable: {error}"));
        }
        Ok(meta) if !meta.is_file() || meta.len() > MAX_REGISTRATION_BYTES => {
            return Registration::Unknown("the owner record is not a small regular file".into());
        }
        Ok(_) => {}
    }
    match fs::read(&path) {
        Ok(bytes) => parse(&bytes),
        Err(error) => Registration::Unknown(format!("the owner record is unreadable: {error}")),
    }
}

/// The reason to hold an install, upgrade or repair to `target` before
/// anything is touched; None when it may proceed.
pub fn refusal(cfg: &Config, registration: &Registration, target: &str) -> Result<Option<String>> {
    Ok(refusal_with_code(cfg, registration, target)?.map(|(_, reason)| reason))
}

/// `refusal` with its failure code: a binary path mismatch has its own
/// (`installation_path_mismatch`); every other refusal is `installation_held`.
pub fn refusal_with_code(
    cfg: &Config,
    registration: &Registration,
    target: &str,
) -> Result<Option<(crate::report::FailureCode, String)>> {
    use crate::report::FailureCode;
    let held = |reason: String| Some((FailureCode::InstallationHeld, reason));
    let supervisor = match registration {
        Registration::Absent => return Ok(None),
        Registration::Unknown(detail) => {
            return Ok(held(format!(
                "Raft Computer is registered with this computer's service manager by Raft Desktop, in a form this version does not understand ({detail}). Nothing was changed. Update Computer from Raft Desktop, or run the install command from the setup page again."
            )));
        }
        Registration::Registered(supervisor) => supervisor,
    };
    if Kind::native() != Some(supervisor.kind) {
        return Ok(held(
            "Raft Computer is registered with a service manager this platform does not have. Nothing was changed. Remove the Computer service in Raft Desktop, then retry.".into(),
        ));
    }
    if normalized_path(&supervisor.binary_path) != normalized_path(&cfg.binary) {
        return Ok(Some((
            FailureCode::InstallationPathMismatch,
            format!(
                "Raft Desktop runs Raft Computer from {}, but this installation would write {}. Nothing was changed. Run the install command without RAFT_COMPUTER_INSTALL_DIR or RAFT_COMPUTER_BINARY.",
                supervisor.binary_path.display(),
                cfg.binary.display()
            ),
        )));
    }
    if version::compare(target, &supervisor.min_computer_version)? == Ordering::Less {
        return Ok(held(format!(
            "Raft Desktop keeps Raft Computer running with the service manager, which needs Computer {} or newer; {target} would start beside it. Nothing was changed.",
            supervisor.min_computer_version
        )));
    }
    Ok(None)
}

/// Set on the copy that broke away from the Windows job, so it never tries again.
pub const BROKE_AWAY_ENV: &str = "RAFT_COMPUTER_INSTALLER_BROKE_AWAY";

/// Windows: the service runs in the launcher's job object (and Task
/// Scheduler's), which stopping it kills. An installer still inside one
/// would die halfway through the swap.
#[cfg(windows)]
pub fn in_job() -> bool {
    use windows_sys::Win32::System::{JobObjects::IsProcessInJob, Threading::GetCurrentProcess};
    let mut result = 0;
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut result) };
    // Unknown counts as inside: refusing is recoverable, a killed swap is not.
    ok == 0 || result != 0
}

#[cfg(not(windows))]
pub fn in_job() -> bool {
    false
}

/// The refusal when a Windows task-supervised service would stop this
/// installer with it (the breakaway in cli.rs did not happen).
pub fn confinement(registration: &Registration) -> Option<String> {
    let Registration::Registered(supervisor) = registration else {
        return None;
    };
    (supervisor.kind == Kind::WindowsTask && in_job()).then(|| {
        "The Computer service runs in a Windows job that would stop this upgrade together with the service, and the upgrade could not leave it. Nothing was changed. Update Computer from Raft Desktop, or run the install command in your own terminal.".to_string()
    })
}

/// Re-run this invocation outside the job (`CREATE_BREAKAWAY_FROM_JOB`; the
/// launcher's job allows it). Same outcomes as the systemd scope escape.
#[cfg(windows)]
pub async fn break_away(args: &[String]) -> crate::systemd::Escape {
    use crate::systemd::Escape;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(executable) = std::env::current_exe() else {
        return Escape::Failed;
    };
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .env(BROKE_AWAY_ENV, "1")
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW);
    let Ok(mut child) = command.spawn() else {
        return Escape::Failed;
    };
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => Escape::Exited(status.code()),
        Ok(Err(_)) => Escape::Failed,
        Err(_) => Escape::Running,
    }
}

/// One supervisor command. A tolerated failure is checked by what follows
/// (the stop is proven by the process wait; the start by readiness).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Run {
        program: &'static str,
        args: Vec<OsString>,
        tolerate_failure: bool,
    },
    /// `raft-computer stop`, which a registered Computer delegates to the
    /// supervisor itself (a graceful IPC shutdown on Windows).
    ProductStop,
}

fn run(program: &'static str, args: &[&str], tolerate_failure: bool) -> Step {
    Step::Run {
        program,
        args: args.iter().map(OsString::from).collect(),
        tolerate_failure,
    }
}

fn launchd_target(supervisor: &HostSupervisor, uid: u32) -> String {
    format!("gui/{uid}/{}", supervisor.name)
}

/// Stop for an upgrade, repair or rollback. Disable first so nothing can
/// relaunch the service while its bytes are replaced.
pub fn stop_steps(supervisor: &HostSupervisor, uid: u32) -> Vec<Step> {
    match supervisor.kind {
        Kind::LaunchdAgent => {
            let target = launchd_target(supervisor, uid);
            vec![
                run("launchctl", &["disable", &target], false),
                // Not loaded (stopped before the operation) is fine.
                run("launchctl", &["bootout", &target], true),
            ]
        }
        Kind::SystemdUser => vec![run(
            "systemctl",
            &["--user", "stop", "--", &supervisor.name],
            false,
        )],
        Kind::WindowsTask => vec![
            run(
                "schtasks",
                &["/Change", "/TN", &supervisor.name, "/DISABLE"],
                false,
            ),
            Step::ProductStop,
            run("schtasks", &["/End", "/TN", &supervisor.name], true),
        ],
    }
}

pub fn start_steps(supervisor: &HostSupervisor, uid: u32) -> Vec<Step> {
    match supervisor.kind {
        Kind::LaunchdAgent => {
            let target = launchd_target(supervisor, uid);
            let definition = supervisor
                .definition_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            vec![
                run("launchctl", &["enable", &target], false),
                // Already loaded is fine; kickstart then starts it.
                run(
                    "launchctl",
                    &["bootstrap", &format!("gui/{uid}"), &definition],
                    true,
                ),
                run("launchctl", &["kickstart", &target], false),
            ]
        }
        Kind::SystemdUser => vec![run(
            "systemctl",
            &["--user", "start", "--", &supervisor.name],
            false,
        )],
        Kind::WindowsTask => vec![
            run(
                "schtasks",
                &["/Change", "/TN", &supervisor.name, "/ENABLE"],
                false,
            ),
            run("schtasks", &["/Run", "/TN", &supervisor.name], false),
        ],
    }
}

#[cfg(unix)]
pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
pub fn current_uid() -> u32 {
    0
}

async fn execute(program: &'static str, args: &[OsString]) -> Result<()> {
    #[cfg(test)]
    if let Some(ok) = test_manager::record(format!(
        "{program} {}",
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    )) {
        return if ok {
            Ok(())
        } else {
            Err(invalid(format!("{program} failed")))
        };
    }
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    #[cfg(target_os = "linux")]
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", current_uid()));
    }
    let label = || {
        let mut text = program.to_string();
        for arg in args {
            text.push(' ');
            text.push_str(&arg.to_string_lossy());
        }
        text
    };
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .map_err(|_| invalid(format!("{} timed out", label())))??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr.lines().next().unwrap_or("").trim();
        return Err(invalid(format!("{} failed: {line}", label())));
    }
    Ok(())
}

pub async fn apply(cfg: &Config, steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Run {
                program,
                args,
                tolerate_failure,
            } => {
                if let Err(error) = execute(program, args).await
                    && !tolerate_failure
                {
                    return Err(error);
                }
            }
            Step::ProductStop => {
                let _ = crate::computer::lifecycle(cfg, "stop").await;
            }
        }
    }
    Ok(())
}

/// Re-enable without starting (a repair whose own stop disabled an enabled
/// supervisor: the previous state is restored, never more).
pub fn enable_steps(supervisor: &HostSupervisor, uid: u32) -> Vec<Step> {
    match supervisor.kind {
        Kind::LaunchdAgent => vec![run(
            "launchctl",
            &["enable", &launchd_target(supervisor, uid)],
            false,
        )],
        // The installer only stops a unit (`systemctl --user stop`), it never
        // disables it: nothing to undo.
        Kind::SystemdUser => vec![],
        Kind::WindowsTask => vec![run(
            "schtasks",
            &["/Change", "/TN", &supervisor.name, "/ENABLE"],
            false,
        )],
    }
}

pub async fn enable(cfg: &Config, supervisor: &HostSupervisor) -> Result<()> {
    apply(cfg, &enable_steps(supervisor, current_uid())).await
}

/// Before a stop that disables (launchd, the task): was the supervisor
/// enabled? Only then does `restore_after_stop` re-enable it. Unknown counts
/// as not enabled: a supervisor the user stopped is never turned back on.
pub async fn stop_will_disable(supervisor: &HostSupervisor) -> bool {
    supervisor_enabled(supervisor).await == Some(true)
}

/// After a run that stopped but did not start the Computer: restore the
/// supervisor's previous state. Returns a warning when re-enabling failed
/// (the installed bytes stand either way).
pub async fn restore_after_stop(
    cfg: &Config,
    supervisor: &HostSupervisor,
    disabled_by_run: bool,
) -> Option<String> {
    if !disabled_by_run {
        return None;
    }
    enable(cfg, supervisor).await.err().map(|error| {
        format!(
            "The service manager could not re-enable Raft Computer ({error}); run `raft-computer start` to bring it back."
        )
    })
}

pub async fn stop(cfg: &Config, supervisor: &HostSupervisor) -> Result<()> {
    apply(cfg, &stop_steps(supervisor, current_uid())).await
}

pub async fn start(cfg: &Config, supervisor: &HostSupervisor) -> Result<()> {
    apply(cfg, &start_steps(supervisor, current_uid())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../contract/host-lifecycle-registration.v2.json");

    fn registered(kind: &str, extra: &str) -> Vec<u8> {
        format!(
            r#"{{"formatVersion":2,"owner":"app","supervisor":{{"kind":"{kind}","name":"n","definitionPath":"/d/n.plist","binaryPath":"/h/.local/bin/raft-computer","launcherPath":"/h/.local/bin/raft-computer-launch.exe","registeredBy":"desktop","minComputerVersion":"1.0.58"{extra}}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn the_shared_fixture_is_a_launchd_registration() {
        let Registration::Registered(supervisor) = parse(FIXTURE) else {
            panic!("fixture must parse as registered");
        };
        assert_eq!(supervisor.kind, Kind::LaunchdAgent);
        assert_eq!(supervisor.name, "build.raft.desktop.computer");
        assert_eq!(
            supervisor.binary_path,
            PathBuf::from("/Users/u/.local/bin/raft-computer")
        );
        assert_eq!(supervisor.min_computer_version, "1.0.58");
    }

    #[test]
    fn no_record_format_one_and_no_supervisor_are_absent() {
        assert_eq!(
            parse(br#"{"formatVersion":1,"owner":"cli","enabled":true}"#),
            Registration::Absent
        );
        assert_eq!(
            parse(br#"{"formatVersion":2,"owner":"app","supervisor":null}"#),
            Registration::Absent
        );
        assert_eq!(
            parse(br#"{"formatVersion":2,"owner":"cli"}"#),
            Registration::Absent
        );
    }

    #[test]
    fn anything_not_understood_is_registered_but_unknown_never_absent() {
        // Corrupt or format-less: unregistered, as the CLI treats it today.
        assert_eq!(parse(b"not json"), Registration::Absent);
        assert_eq!(parse(br#"{"owner":"app"}"#), Registration::Absent);
        for bytes in [
            &br#"{"formatVersion":3,"supervisor":null}"#[..],
            br#"{"formatVersion":2,"supervisor":"launchd"}"#,
        ] {
            assert!(
                matches!(parse(bytes), Registration::Unknown(_)),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert!(matches!(
            parse(&registered("upstart", "")),
            Registration::Unknown(_)
        ));
        let foreign = String::from_utf8(registered("launchd-agent", ""))
            .unwrap()
            .replace("\"desktop\"", "\"cli\"");
        assert!(matches!(
            parse(foreign.as_bytes()),
            Registration::Unknown(_)
        ));
        let relative = String::from_utf8(registered("launchd-agent", ""))
            .unwrap()
            .replace("/h/.local", "h/.local");
        assert!(matches!(
            parse(relative.as_bytes()),
            Registration::Unknown(_)
        ));
        let bad_floor = String::from_utf8(registered("launchd-agent", ""))
            .unwrap()
            .replace("1.0.58", "latest");
        assert!(matches!(
            parse(bad_floor.as_bytes()),
            Registration::Unknown(_)
        ));
        let windows = String::from_utf8(registered("windows-task", ""))
            .unwrap()
            .replace(
                "/h/.local/bin/raft-computer-launch.exe",
                r"C:\\Users\\u\\.local\\bin\\raft-computer-launch.exe",
            );
        assert!(
            matches!(parse(windows.as_bytes()), Registration::Registered(_)),
            "Windows paths parse on every host"
        );
        // Later additive fields are tolerated.
        assert!(matches!(
            parse(&registered("launchd-agent", r#","futureField":1"#)),
            Registration::Registered(_)
        ));
    }

    fn supervisor(kind: Kind, name: &str) -> HostSupervisor {
        HostSupervisor {
            kind,
            name: name.into(),
            definition_path: Some(PathBuf::from(
                "/Users/u/Library/LaunchAgents/build.raft.desktop.computer.plist",
            )),
            binary_path: PathBuf::from("/Users/u/.local/bin/raft-computer"),
            launcher_path: None,
            min_computer_version: "1.0.58".into(),
        }
    }

    fn commands(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                Step::Run {
                    program,
                    args,
                    tolerate_failure,
                } => format!(
                    "{program} {}{}",
                    args.iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                    if *tolerate_failure {
                        " (tolerated)"
                    } else {
                        ""
                    }
                ),
                Step::ProductStop => "raft-computer stop (tolerated)".into(),
            })
            .collect()
    }

    #[test]
    fn launchd_stop_disables_before_bootout_and_start_enables_before_bootstrap() {
        let s = supervisor(Kind::LaunchdAgent, "build.raft.desktop.computer");
        assert_eq!(
            commands(&stop_steps(&s, 501)),
            [
                "launchctl disable gui/501/build.raft.desktop.computer",
                "launchctl bootout gui/501/build.raft.desktop.computer (tolerated)",
            ]
        );
        assert_eq!(
            commands(&start_steps(&s, 501)),
            [
                "launchctl enable gui/501/build.raft.desktop.computer",
                "launchctl bootstrap gui/501 /Users/u/Library/LaunchAgents/build.raft.desktop.computer.plist (tolerated)",
                "launchctl kickstart gui/501/build.raft.desktop.computer",
            ]
        );
    }

    #[test]
    fn windows_stop_disables_then_stops_gracefully_then_ends_the_task() {
        let s = supervisor(Kind::WindowsTask, r"\Raft\Raft Computer");
        assert_eq!(
            commands(&stop_steps(&s, 0)),
            [
                r"schtasks /Change /TN \Raft\Raft Computer /DISABLE",
                "raft-computer stop (tolerated)",
                r"schtasks /End /TN \Raft\Raft Computer (tolerated)",
            ]
        );
        assert_eq!(
            commands(&start_steps(&s, 0)),
            [
                r"schtasks /Change /TN \Raft\Raft Computer /ENABLE",
                r"schtasks /Run /TN \Raft\Raft Computer",
            ]
        );
    }

    #[test]
    fn systemd_user_units_are_stopped_and_started_by_their_manager() {
        let s = supervisor(Kind::SystemdUser, "raft-desktop-computer.service");
        assert_eq!(
            commands(&stop_steps(&s, 0)),
            ["systemctl --user stop -- raft-desktop-computer.service"]
        );
        assert_eq!(
            commands(&start_steps(&s, 0)),
            ["systemctl --user start -- raft-desktop-computer.service"]
        );
    }

    fn cfg_with_binary(binary: &str) -> Config {
        let mut cfg = Config::load().unwrap();
        cfg.binary = PathBuf::from(binary);
        cfg
    }

    #[test]
    fn refusals_hold_path_mismatches_old_targets_and_unknown_records() {
        let native = Kind::native().unwrap();
        let mut s = supervisor(native, "n");
        s.binary_path = PathBuf::from("/Users/u/.local/bin/raft-computer");
        let cfg = cfg_with_binary("/Users/u/.local/bin/raft-computer");
        let registered = Registration::Registered(s.clone());
        assert_eq!(refusal(&cfg, &registered, "1.0.58").unwrap(), None);
        assert_eq!(refusal(&cfg, &registered, "1.1.0").unwrap(), None);
        assert!(
            refusal(&cfg, &registered, "1.0.57")
                .unwrap()
                .unwrap()
                .contains("1.0.58 or newer")
        );
        let moved = cfg_with_binary("/opt/elsewhere/raft-computer");
        assert!(
            refusal(&moved, &registered, "1.0.60")
                .unwrap()
                .unwrap()
                .contains("RAFT_COMPUTER_INSTALL_DIR")
        );
        let unknown = Registration::Unknown("owner record format 3".into());
        assert!(
            refusal(&cfg, &unknown, "1.0.60")
                .unwrap()
                .unwrap()
                .contains("format 3")
        );
        assert_eq!(refusal(&cfg, &Registration::Absent, "0.0.1").unwrap(), None);
        let other = if native == Kind::LaunchdAgent {
            Kind::SystemdUser
        } else {
            Kind::LaunchdAgent
        };
        s.kind = other;
        assert!(
            refusal(&cfg, &Registration::Registered(s), "1.0.60")
                .unwrap()
                .is_some()
        );
    }

    fn temp_cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.state_home = dir.path().to_path_buf();
        cfg.binary = dir.path().join("bin").join("raft-computer");
        (dir, cfg)
    }

    fn write_registration(cfg: &Config, definition: &std::path::Path) {
        let record = serde_json::json!({
            "formatVersion": 2, "owner": "app",
            "supervisor": {
                "kind": "launchd-agent", "name": "build.raft.desktop.computer",
                "definitionPath": definition, "binaryPath": cfg.binary,
                "launcherPath": null, "registeredBy": "desktop",
                "registeredAt": "t", "minComputerVersion": "1.0.58"
            }
        });
        let path = registration_path(cfg);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, record.to_string()).unwrap();
    }

    /// archer's #60 blocker: the registration exists, the plist was deleted,
    /// and Computer runs self-managed. Before, stop disabled the label, the
    /// bootout failed, the process was killed and bootstrap/kickstart failed:
    /// Computer stayed down. Now the definition is confirmed before anything
    /// stops: a missing one is a stale registration, handled self-managed.
    #[tokio::test]
    async fn a_registration_whose_definition_is_gone_is_stale_before_anything_stops() {
        let (dir, cfg) = temp_cfg();
        let plist = dir.path().join("build.raft.desktop.computer.plist");
        write_registration(&cfg, &plist);
        // Never the host's real launchctl: the job is not loaded.
        test_manager::install(|_, _| Some((false, String::new())));
        assert!(
            matches!(read(&cfg), Registration::Registered(_)),
            "the file alone says registered"
        );
        assert_eq!(
            resolve(&cfg).await,
            Registration::Absent,
            "no definition: stale, not supervised"
        );
        assert_eq!(refusal(&cfg, &resolve(&cfg).await, "1.0.58").unwrap(), None);
        fs::write(&plist, "<plist/>").unwrap();
        assert!(matches!(resolve(&cfg).await, Registration::Registered(_)));
    }

    #[test]
    fn paths_compare_the_same_whatever_their_spelling() {
        assert_eq!(
            normalized_path(std::path::Path::new(
                r"\\?\C:\Users\U\.local\bin\raft-computer.exe"
            )),
            normalized_path(std::path::Path::new(
                r"C:\Users\u\.local\bin\raft-computer.exe"
            )),
            "a verbatim prefix and case never make the same deleted binary differ"
        );
        assert_eq!(
            normalized_path(std::path::Path::new("C:/Users/u/x.exe")),
            normalized_path(std::path::Path::new(r"C:\Users\u\x.exe"))
        );
        // On a POSIX host a POSIX path keeps its case (Windows compares
        // every path case-insensitively, as its file system does).
        #[cfg(not(windows))]
        assert_ne!(
            normalized_path(std::path::Path::new("/Users/u/.local/bin/raft-computer")),
            normalized_path(std::path::Path::new("/Users/U/.local/bin/raft-computer")),
            "POSIX paths stay case-sensitive"
        );
    }

    #[test]
    fn a_path_mismatch_has_its_own_failure_code() {
        let (_dir, mut cfg) = temp_cfg();
        let mut s = supervisor(Kind::native().unwrap(), "n");
        s.binary_path = PathBuf::from("/Users/u/.local/bin/raft-computer");
        cfg.binary = PathBuf::from("/opt/elsewhere/raft-computer");
        let (code, _) = refusal_with_code(&cfg, &Registration::Registered(s), "1.0.60")
            .unwrap()
            .unwrap();
        assert_eq!(code, crate::report::FailureCode::InstallationPathMismatch);
        assert_eq!(code.as_str(), "installation_path_mismatch");
    }

    #[test]
    fn enabling_a_stopped_registration_never_starts_it() {
        // (Only re-enables; systemd units are never disabled by the installer.)
        let s = supervisor(Kind::LaunchdAgent, "build.raft.desktop.computer");
        assert_eq!(
            commands(&enable_steps(&s, 501)),
            ["launchctl enable gui/501/build.raft.desktop.computer"]
        );
        let t = supervisor(Kind::WindowsTask, r"\Raft\Raft Computer");
        assert_eq!(
            commands(&enable_steps(&t, 0)),
            [r"schtasks /Change /TN \Raft\Raft Computer /ENABLE"]
        );
    }

    #[test]
    fn every_shared_parsing_case_is_classified_as_slock_classifies_it() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../contract/host-lifecycle-registration.cases.json"
        ))
        .unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap();
            let state = match parse(raw.as_bytes()) {
                Registration::Absent => "absent",
                Registration::Registered(_) => "registered",
                Registration::Unknown(_) => "unknown",
            };
            assert_eq!(state, case["state"].as_str().unwrap(), "{}", case["name"]);
        }
    }

    #[test]
    fn launchctl_print_disabled_is_read_for_the_label_only() {
        let out = "disabled services = {\n\t\"com.other\" => disabled\n\t\"build.raft.desktop.computer\" => disabled\n}\n";
        assert!(launchd_disabled(out, "build.raft.desktop.computer"));
        assert!(!launchd_disabled(
            "\t\"build.raft.desktop.computer\" => enabled\n",
            "build.raft.desktop.computer"
        ));
        assert!(
            launchd_disabled(
                "\t\"build.raft.desktop.computer\" => true\n",
                "build.raft.desktop.computer"
            ),
            "older macOS"
        );
        assert!(!launchd_disabled(
            "\t\"build.raft.desktop.computer.x\" => disabled\n",
            "build.raft.desktop.computer"
        ));
        assert!(
            !launchd_disabled("", "build.raft.desktop.computer"),
            "not listed: enabled"
        );
    }

    /// archer #60 re-review, follow-ups 1 and 2: the registration is present
    /// and the plist was deleted while the Computer runs.
    /// - launchd still has the job loaded: held, so nothing is stopped (once
    ///   booted out it could not be started again without its definition);
    /// - nothing is loaded: the Computer is self-managed; the supervisor is
    ///   never touched.
    /// upgrade, install and repair all pass this one gate before any stop
    /// (`the_supervisor_gate_comes_before_every_stop`).
    #[tokio::test]
    async fn a_deleted_plist_with_the_job_still_loaded_is_held_and_nothing_is_stopped() {
        let (dir, cfg) = temp_cfg();
        let plist = dir.path().join("build.raft.desktop.computer.plist");
        write_registration(&cfg, &plist);
        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print")) => {
                Some((true, "\tstate = running\n\tpid = 4242\n".into()))
            }
            _ => Some((false, String::new())),
        });
        let registration = resolve(&cfg).await;
        assert!(
            matches!(&registration, Registration::Unknown(detail) if detail.contains("definition file is gone")),
            "{registration:?}"
        );
        for target in ["1.0.58", "1.0.60"] {
            let (code, _) = refusal_with_code(&cfg, &registration, target)
                .unwrap()
                .expect("held before anything stops");
            assert_eq!(code, crate::report::FailureCode::InstallationHeld);
        }
        assert_eq!(
            test_manager::commands(),
            Vec::<String>::new(),
            "nothing disabled, booted out or stopped"
        );

        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print")) => Some((false, String::new())),
            _ => Some((false, String::new())),
        });
        assert_eq!(
            resolve(&cfg).await,
            Registration::Absent,
            "nothing loaded: self-managed"
        );
        assert_eq!(
            refusal(&cfg, &Registration::Absent, "1.0.60").unwrap(),
            None
        );
        assert_eq!(
            test_manager::commands(),
            Vec::<String>::new(),
            "the supervisor is never touched"
        );

        test_manager::install(|_, _| None);
        assert!(
            matches!(resolve(&cfg).await, Registration::Unknown(_)),
            "a manager that cannot be asked holds"
        );
    }

    #[tokio::test]
    async fn a_systemd_unit_whose_file_is_gone_but_still_loaded_is_held() {
        let (dir, cfg) = temp_cfg();
        let unit = dir.path().join("raft-desktop-computer.service");
        let record = serde_json::json!({ "formatVersion": 2, "supervisor": {
            "kind": "systemd-user", "name": "raft-desktop-computer.service", "definitionPath": unit,
            "binaryPath": cfg.binary, "launcherPath": null, "registeredBy": "desktop", "registeredAt": "t", "minComputerVersion": "1.0.58" } });
        fs::create_dir_all(registration_path(&cfg).parent().unwrap()).unwrap();
        fs::write(registration_path(&cfg), record.to_string()).unwrap();
        test_manager::install(|program, args| match program {
            "systemctl" if args.contains(&"LoadState") => Some((true, "loaded\n".into())),
            _ => Some((false, String::new())),
        });
        assert!(matches!(resolve(&cfg).await, Registration::Unknown(_)));
        test_manager::install(|program, args| match program {
            "systemctl" if args.contains(&"LoadState") => Some((true, "not-found\n".into())),
            _ => Some((false, String::new())),
        });
        assert_eq!(resolve(&cfg).await, Registration::Absent);
    }

    /// archer #60 re-review blocker: restore the previous state, never more.
    #[tokio::test]
    async fn a_run_re_enables_only_what_its_own_stop_disabled() {
        let (_dir, cfg) = temp_cfg();
        let s = supervisor(Kind::LaunchdAgent, "build.raft.desktop.computer");
        // (a) The user ran `raft-computer stop` (the label is disabled): the
        // install or repair leaves it disabled.
        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print-disabled")) => Some((
                true,
                "\t\"build.raft.desktop.computer\" => disabled\n".into(),
            )),
            _ => Some((true, String::new())),
        });
        let disabled_by_run = stop_will_disable(&s).await;
        assert!(!disabled_by_run);
        assert_eq!(restore_after_stop(&cfg, &s, disabled_by_run).await, None);
        assert_eq!(
            test_manager::commands(),
            Vec::<String>::new(),
            "never re-enabled"
        );
        // (b) It was enabled and this run's stop disabled it: re-enabled.
        test_manager::install(|program, args| match (program, args.first().copied()) {
            ("launchctl", Some("print-disabled")) => {
                Some((true, "\t\"com.other\" => disabled\n".into()))
            }
            _ => Some((true, String::new())),
        });
        let disabled_by_run = stop_will_disable(&s).await;
        assert!(disabled_by_run);
        assert_eq!(restore_after_stop(&cfg, &s, disabled_by_run).await, None);
        assert_eq!(
            test_manager::commands(),
            ["launchctl enable gui/".to_string()
                + &current_uid().to_string()
                + "/build.raft.desktop.computer"]
        );
        // A failed re-enable is a warning, never a failed install.
        test_manager::fail_commands(true);
        let warning = restore_after_stop(&cfg, &s, true).await;
        test_manager::fail_commands(false);
        assert!(warning.is_some_and(|w| w.contains("raft-computer start")));
        // Unknown state (the manager did not answer) never re-enables.
        test_manager::install(|_, _| None);
        assert!(!stop_will_disable(&s).await);
        // systemd: the installer never disables a unit, so nothing to undo.
        let unit = supervisor(Kind::SystemdUser, "raft-desktop-computer.service");
        assert!(!stop_will_disable(&unit).await);
        assert!(enable_steps(&unit, 0).is_empty());
    }

    #[test]
    fn the_supervisor_gate_comes_before_every_stop() {
        let source = include_str!("operation.rs");
        for function in ["async fn upgrade(", "async fn install("] {
            let body = &source[source.find(function).unwrap()..];
            let body = &body[..body[1..]
                .find("\nasync fn ")
                .map_or(body.len(), |end| end + 1)];
            let gate = body
                .find("supervisor_refusal(cfg, plan).await?")
                .unwrap_or_else(|| panic!("{function} has the gate"));
            for stop in ["stop_product(", "Phase::Stopping", "Phase::Upgrading"] {
                if let Some(at) = body.find(stop) {
                    assert!(gate < at, "{function}: the supervisor gate precedes {stop}");
                }
            }
        }
    }

    #[test]
    fn the_shared_fixtures_match_the_digests_slock_publishes() {
        let list = include_str!("../contract/CONTRACT_FIXTURES");
        let mut checked = 0;
        for line in list
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        {
            let (digest, name) = line.split_once("  ").unwrap();
            let bytes = fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("contract")
                    .join(name),
            )
            .unwrap();
            assert_eq!(
                k_carrier::artifact::sha256(&bytes),
                digest,
                "{name} differs from slock's copy"
            );
            checked += 1;
        }
        assert_eq!(checked, 3);
    }
}
