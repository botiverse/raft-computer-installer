use crate::{
    Error, Result, computer,
    config::Config,
    diagnostic, host, operation,
    presence::Interaction,
    report::FailureCode,
    request::{Reply, Request},
    settle::{self, Claim},
    source, supervisor, version,
};
use k_carrier::error::invalid;
use std::{env, sync::Mutex, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

pub fn failure_line() -> &'static str {
    "The installation could not finish. Run the same install command again."
}

/// Report an error that reached main(): the cause, one runnable next step,
/// and with `--json` one reply object on stdout. Returns the exit code. A web
/// upgrade's request also gets that answer as its receipt (settle.rs).
pub fn report_top_level_failure(error: &Error) -> u8 {
    let json = env::args().skip(1).any(|arg| arg == "--json");
    let code = crate::failure::report(error, json);
    settle_top_level_failure(error);
    code
}

/// The request an error reaching main() settles, and how sure this run is
/// that it is its own (`Claim`). None: nothing to settle, or a request that
/// is not this run's (a leaked `remote:` label, a declared CLI).
struct Pending {
    cfg: Config,
    request: Request,
    claim: Claim,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

fn pending(cfg: &Config, request: &Request, claim: Option<Claim>) {
    let mut slot = PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = claim
        .filter(|_| settle::from_web(request))
        .map(|claim| Pending {
            cfg: cfg.clone(),
            request: request.clone(),
            claim,
        });
}

fn pending_claim() -> Option<Claim> {
    PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|pending| pending.claim)
}

fn settle_top_level_failure(error: &Error) {
    let Some(pending) = PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    else {
        return;
    };
    let id = &pending.request.id;
    let blocked = match crate::failure::classify(error) {
        crate::failure::Cause::PermissionDenied | crate::failure::Cause::ReadOnly => {
            crate::failure::blocked(&pending.cfg, &pending.request.command)
        }
        _ => None,
    };
    let answer = crate::failure::answer(error, Some(&pending.cfg), blocked.as_ref());
    let reply = answer
        .reply(id)
        .with_diagnostic(diagnostic::safe_error(error));
    let reply = if reply.validate(id).is_ok() {
        reply
    } else {
        answer.reply(id)
    };
    settle::settle(&pending.cfg, &pending.request, &reply, pending.claim);
}

/// A web upgrade's request when its arguments could not be read: the id and
/// label the service set, the command if one was named.
fn unparsed_request(args: &[String]) -> Option<Request> {
    let request = Request {
        protocol_version: 1,
        id: env::var("RAFT_COMPUTER_OPERATION_ID").ok()?,
        command: args
            .first()
            .filter(|arg| !arg.starts_with('-'))
            .cloned()
            .unwrap_or_else(|| "upgrade".into()),
        version: None,
        channel: None,
        yes: false,
        allow_downgrade: false,
        presence: Interaction::detect().presence,
        approved_by: env::var("RAFT_COMPUTER_APPROVED_BY").ok()?,
        recovery_only: false,
        waiting_caller: None,
        escape_failure: None,
    };
    (request.validate().is_ok() && settle::from_web(&request)).then_some(request)
}

/// Print a reply the way `run` does for an answer of its own.
fn print_reply(reply: &Reply, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(reply)?);
    } else {
        eprintln!("{}", reply.line);
    }
    Ok(())
}

/// The run that started an escaped copy (systemd::escape) stays until the
/// copy exits: this run is in the service's unit, so it is stopped with the
/// unit once the copy stops it, and by then the copy owns the result. If the
/// copy (or systemd-run before it) ends without a receipt, this run writes
/// one, create-only. It holds no lock while it waits.
async fn await_escaped_copy(
    cfg: &Config,
    request: &Request,
    json: bool,
    copy: Box<crate::systemd::Copy>,
) -> Result<u8> {
    let exited = copy.wait(crate::systemd::COPY_WAIT).await;
    if let Ok(Some(receipt)) = crate::report::read(cfg, &request.id) {
        let reply = Reply::receipt(receipt);
        if json {
            println!("{}", serde_json::to_string(&reply)?);
        } else {
            println!("{}", reply.line);
        }
        return Ok(reply.exit_code);
    }
    let (reason, note) = match exited {
        Some((code, stderr)) => (
            "The upgrade stopped before it finished, after it moved out of the Raft Computer service.",
            crate::systemd::escape_note(code, &stderr),
        ),
        None => (
            "The upgrade did not finish in time after it moved out of the Raft Computer service.",
            format!(
                "the copy in its own scope still runs after {} s",
                crate::systemd::COPY_WAIT.as_secs()
            ),
        ),
    };
    let reply = Reply::failure(
        &request.id,
        1,
        format!("{reason} Run the same install command again."),
        FailureCode::EscapeFailed,
        reason,
    )
    .with_diagnostic(crate::systemd::clip(note, 2048));
    // A worker the copy left running still holds the operation gate and
    // writes this request's own receipt.
    let settled = match k_carrier::lock::UpgradeLock::acquire(&cfg.installer_dir.join("gate")) {
        Ok(_gate) => settle::settle(cfg, request, &reply, Claim::Own),
        Err(_) => None,
    };
    let reply = settled.map_or(reply, Reply::receipt);
    print_reply(&reply, json)?;
    Ok(reply.exit_code)
}

const HELP: &str = "Raft Computer installation commands

  install|upgrade [--version V | --channel main|alpha|NAME] [--yes] [--allow-downgrade] [--json]
  repair          [--version V | --channel main|alpha|NAME] [--yes] [--allow-downgrade] [--json]
  status          [--json]
  recover         [--json]

No command means upgrade. Unattended invocations never ask questions.
Repair is allowed only for a broken installation and preserves credentials, configuration and user files.
Exit codes: 0 succeeded, 1 failed or rolled back, 2 held, 3 recovery unresolved.
";

struct Args {
    request: Request,
    json: bool,
}

fn parse(args: &[String]) -> Result<Args> {
    let presence = Interaction::detect().presence;
    let id =
        env::var("RAFT_COMPUTER_OPERATION_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let approved_by = env::var("RAFT_COMPUTER_APPROVED_BY").unwrap_or_else(|_| {
        if presence == crate::presence::Presence::Unattended {
            "unattended invocation"
        } else {
            "terminal operator"
        }
        .into()
    });
    let mut request = Request {
        protocol_version: 1,
        id,
        command: "upgrade".into(),
        version: None,
        channel: None,
        yes: false,
        allow_downgrade: false,
        presence,
        approved_by,
        recovery_only: false,
        waiting_caller: None,
        escape_failure: None,
    };
    let mut json = false;
    let mut position = 0;
    if args.first().is_some_and(|arg| !arg.starts_with('-')) {
        request.command = args[0].clone();
        position = 1;
    }
    while position < args.len() {
        let option = &args[position];
        let (name, inline) = option
            .split_once('=')
            .map_or((option.as_str(), None), |(name, value)| (name, Some(value)));
        match name {
            "--yes" | "-y" if inline.is_none() => request.yes = true,
            "--allow-downgrade" if inline.is_none() => request.allow_downgrade = true,
            "--json" if inline.is_none() => json = true,
            "--version" | "--channel" => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        position += 1;
                        args.get(position)
                            .ok_or_else(|| invalid("option requires a value"))?
                    }
                };
                if name == "--version" {
                    if request.version.is_some() {
                        return Err(invalid("duplicate --version"));
                    }
                    request.version = Some(version::normalize(value)?);
                } else {
                    if request.channel.is_some() {
                        return Err(invalid("duplicate --channel"));
                    }
                    request.channel = Some(source::parse_channel(value)?);
                }
            }
            _ => return Err(invalid("unknown option")),
        }
        position += 1;
    }
    request.validate()?;
    Ok(Args { request, json })
}

async fn worker(cfg: &Config) -> Result<u8> {
    k_carrier::host::isolate_standard_handles()?;
    let mut bytes = Vec::new();
    timeout(
        Duration::from_secs(30),
        tokio::io::stdin().take(16385).read_to_end(&mut bytes),
    )
    .await
    .map_err(|_| invalid("worker request timeout"))??;
    if bytes.len() > 16384 {
        return Err(invalid("worker request too large"));
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    request.validate()?;
    let reply = match operation::execute(cfg, &request).await {
        Ok(reply) => reply,
        Err(Error::Locked(_)) => Reply::failure(
            &request.id,
            2,
            "Another installation is running on this machine.",
            FailureCode::OperationBusy,
            "Another installation is running on this machine.",
        ),
        Err(error) => {
            // Inspect persistent state instead of reporting a write/read failure
            // as an ordinary pre-install failure after effects have begun.
            let unresolved = error.is_uncertain()
                || cfg.installer_dir.join("active.json").exists()
                || matches!(
                    k_carrier::storage::FileStore::new(&cfg.k_state).read_operation(),
                    k_carrier::state::OperationRead::Unreadable { .. }
                        | k_carrier::state::OperationRead::Observed {
                            operation: k_carrier::state::Operation { outcome: None, .. }
                        }
                );
            let diagnostic = diagnostic::safe_error(&error);
            let code = if unresolved {
                FailureCode::RecoveryUnresolved
            } else if crate::failure::classify(&error) == crate::failure::Cause::DiskFull {
                FailureCode::DiskFull
            } else {
                FailureCode::InstallFailed
            };
            let exit_code = if unresolved { 3 } else { 1 };
            // A permission, read-only or full-disk failure fails the same way
            // on every run: name it and its fix instead of "run it again"
            // (for example a lock folder a sudo run left owned by root).
            if crate::failure::classify(&error).retry_helps() {
                Reply::failure(
                    &request.id,
                    exit_code,
                    failure_line(),
                    code,
                    "The installation could not finish.",
                )
            } else {
                let blocked = crate::failure::blocked(cfg, &request.command);
                let answer = crate::failure::answer(&error, Some(cfg), blocked.as_ref());
                Reply::failure(&request.id, exit_code, answer.line(), code, answer.message)
                    .with_next_step(answer.next_step)
            }
            .with_diagnostic(diagnostic)
        }
    };
    // execute() has released the operation gate. Cleanup reacquires it and
    // does nothing if another operation has started or recovery is unresolved.
    // Housekeeping does not change the installation result or require user
    // intervention. Its durable request remains available for a later retry.
    let _ = crate::cleanup::run(cfg).await;
    reply.validate(&request.id)?;
    let mut bytes = serde_json::to_vec(&reply)?;
    bytes.push(b'\n');
    let mut output = tokio::io::stdout();
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(reply.exit_code)
}

/// The answer to a local install, upgrade or repair started from inside the
/// running Computer (see host::product_ancestor).
pub const LEAKED_REMOTE_LINE: &str = "This shell still carries a finished web upgrade's approval, so nothing was changed. Run the command again without it: env -u RAFT_COMPUTER_OPERATION_ID -u RAFT_COMPUTER_APPROVED_BY -u RAFT_COMPUTER_NON_INTERACTIVE raft-computer upgrade";
pub const INSIDE_COMPUTER_LINE: &str = "This upgrade was started from inside Raft Computer (for example by an agent on this computer). Stopping the service would stop the upgrade too, so nothing was changed. Ask the computer's owner to upgrade it from the Computer page on the web, or run the command in your own terminal.";

pub async fn run() -> Result<u8> {
    // Isolate the launcher too: otherwise its capture pipes can pass through
    // the worker as extra inherited handles into a resident Windows service.
    k_carrier::host::isolate_standard_handles()?;
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() == 1 && ["--help", "-h", "help"].contains(&args[0].as_str()) {
        print!("{HELP}");
        return Ok(0);
    }
    if args.len() == 1 && ["--version", "version"].contains(&args[0].as_str()) {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let cfg = Config::load()?;
    if args.len() == 1 {
        match args[0].as_str() {
            "--operation-worker" => return worker(&cfg).await,
            "--host-controller" => return host::serve(&cfg).await,
            "--product-command" => return computer::serve_lifecycle(&cfg).await,
            _ => {}
        }
    }
    let raw_args = args.clone();
    let mut args = match parse(&args) {
        Ok(args) => args,
        Err(error) => {
            // Its caller is unknown yet: Claim::Unproven.
            if let Some(request) = unparsed_request(&raw_args) {
                pending(&cfg, &request, Some(Claim::Unproven));
            }
            return Err(error);
        }
    };
    // An error before the caller is known settles only an id nothing was
    // started for (Claim::Unproven).
    pending(&cfg, &args.request, Some(Claim::Unproven));
    if ["install", "upgrade", "repair"].contains(&args.request.command.as_str()) {
        let marker = env::var("RAFT_COMPUTER_INSTALLER_CALLER").ok();
        if marker.is_some() {
            // A CLI declaring itself never carries a web upgrade: any
            // `remote:` label it passes on leaked (refused below).
            pending(&cfg, &args.request, None);
        }
        if marker.as_deref().is_some_and(|v| v != "waiting-cli-v1") {
            return Err(invalid("invalid caller declaration"));
        }
        // The waiting CLI may run from the installed binary or, on a legacy
        // two-layer install, from the slot artifact its launcher execs into.
        let mut parent = crate::process::immediate_parent(&host::product_executables(&cfg))?;
        if marker.is_some() && parent.is_none() {
            return Err(invalid(
                "waiting caller is not the installed immediate parent",
            ));
        }
        // The copy systemd::escape() started from a web upgrade's run: its
        // parent is that run, not the service. It stands in the service the
        // original was started by, so it gets exactly the original's verdict
        // below, only when the process tree proves it (same executable bytes
        // as its parent, whose parent is a Computer); the variable alone
        // never exempts anything.
        if parent.is_none() && marker.is_none() {
            parent = crate::systemd::escaped_from_service(
                &cfg,
                env::var_os(crate::systemd::ESCAPED_ENV).is_some(),
            )?;
        }
        // A web upgrade's run: started by a Computer process (the service), no
        // CLI declaring itself (a `remote:` label is honored only then).
        let from_service = parent.is_some() && marker.is_none();
        // A command run from inside the Computer (an agent on this computer,
        // through the CLI or by calling this installer directly): stopping the
        // service would kill this installer with it. Refuse before anything is
        // touched. Decided from the process tree, never from the environment.
        // Prevents accidents only: the same OS user can always arrange its own
        // process tree (setsid, a double fork) or download an installer, so
        // this is not an approval check (#proj-raft-computer:#32).
        let inside = match &parent {
            // Through a Computer process (the waiting CLI, or the service for a
            // web upgrade): a Computer above that process.
            Some(parent) => host::product_ancestor(&cfg, parent)?,
            // Called directly: a Computer anywhere above this installer.
            None => match crate::process::observe(std::process::id())? {
                Some(own) => host::product_ancestor(&cfg, &own)?,
                None => None,
            },
        };
        if let Some(product) = inside {
            let reply = Reply::failure(
                &args.request.id,
                2,
                INSIDE_COMPUTER_LINE,
                FailureCode::InstallationHeld,
                "The upgrade was started from inside Raft Computer.",
            )
            .with_diagnostic(format!("computer process {} is an ancestor", product.pid));
            // Started by a Computer process for a web upgrade: that request
            // waits for this answer. Not proven its own: a CLI from before
            // the caller declaration (Computer < 1.0.38) run by an agent
            // with a leaked label looks the same, so only an id nothing was
            // started for is settled (a run still working on that id saved
            // its plan before it started the service that leaked it).
            if from_service {
                settle::settle(&cfg, &args.request, &reply, Claim::Unproven);
            }
            print_reply(&reply, args.json)?;
            return Ok(reply.exit_code);
        }
        // A web upgrade's installer is a direct child of the running service,
        // with no Computer above it and no CLI declaring itself. A `remote:`
        // label anywhere else is a leaked one (an old installer passed it to
        // the service and every agent): refuse, and say how to drop it, rather
        // than treat it as an ordinary local upgrade.
        if args.request.approved_by.starts_with("remote:") && !from_service {
            // Never settled: this id and label belong to another run, which
            // either has its receipt or is still working on it.
            pending(&cfg, &args.request, None);
            let reply = Reply::failure(
                &args.request.id,
                2,
                LEAKED_REMOTE_LINE,
                FailureCode::InstallationHeld,
                "A web upgrade's approval was found outside that upgrade.",
            );
            print_reply(&reply, args.json)?;
            return Ok(reply.exit_code);
        }
        if let Some(parent) = parent {
            if marker.is_some() {
                // The supported CLI declares its role. Terminal presence and a
                // separate status snapshot cannot establish who is waiting. OS
                // parent identity limits the exemption to this invocation.
                args.request.waiting_caller = Some(parent);
            } else if !args.request.approved_by.starts_with("remote:") {
                let reply = Reply::failure(
                    &args.request.id,
                    1,
                    "This command is started by Raft Computer, not run by hand. To install or upgrade, use the install command from the setup page.",
                    FailureCode::CallerNotComputer,
                    "This command is started by Raft Computer, not run by hand.",
                );
                // No `remote:` label: not a web upgrade, nothing to settle.
                print_reply(&reply, args.json)?;
                return Ok(reply.exit_code);
            }
        }
        // Past the caller checks: a `remote:` label here came from the
        // service, so the request is this run's own.
        if from_service {
            pending(&cfg, &args.request, Some(Claim::Own));
        }
    }
    // Every command writes under the state home first (the supervisor's
    // scratch, the operation gate). A directory this user cannot write (for example a
    // ~/.raft created by root) fails identically on every run: say which one
    // and how to fix it before anything is downloaded or stopped. Only
    // metadata is read; nothing is created (#proj-raft-computer:#44).
    // Root in another user's home is refused here too, before it creates
    // folders that user could not write afterwards.
    if let Some(answer) = crate::failure::preflight(&cfg, &args.request.command) {
        let reply = answer.reply(&args.request.id);
        if let Some(claim) = pending_claim() {
            settle::settle(&cfg, &args.request, &reply, claim);
        }
        eprintln!("{}", answer.line());
        if args.json {
            println!("{}", serde_json::to_string(&reply)?);
        }
        return Ok(reply.exit_code);
    }
    // Started inside the systemd unit that runs the service (Computer up to
    // 1.0.47 launches it there): continue from a scope of its own, so stopping
    // the unit does not stop this upgrade. If that fails, the operation
    // refuses before stopping anything (operation::upgrade).
    if ["upgrade", "repair"].contains(&args.request.command.as_str())
        && env::var_os(crate::systemd::ESCAPED_ENV).is_none()
        && let Some(unit) = crate::systemd::product_unit(&cfg).await?
        && crate::systemd::self_inside(&unit)
    {
        match crate::systemd::escape(&unit, &raw_args, &cfg.scratch()).await {
            crate::systemd::Escape::Running(copy) => {
                if !args.json {
                    println!(
                        "Continuing the {} outside systemd unit {}.",
                        args.request.command, unit.name
                    );
                }
                return match copy {
                    Some(copy) => await_escaped_copy(&cfg, &args.request, args.json, copy).await,
                    None => Ok(0),
                };
            }
            crate::systemd::Escape::Exited(Some(0), _) => {
                if !args.json {
                    println!(
                        "Continuing the {} outside systemd unit {}.",
                        args.request.command, unit.name
                    );
                }
                return Ok(0);
            }
            // The copy got as far as settling this request: its receipt is
            // the answer, never a second, contradicting one from here.
            crate::systemd::Escape::Exited(code, stderr) => {
                if let Some(receipt) = crate::report::read(&cfg, &args.request.id)? {
                    let reply = Reply::receipt(receipt);
                    if args.json {
                        println!("{}", serde_json::to_string(&reply)?);
                    } else {
                        println!("{}", reply.line);
                    }
                    return Ok(reply.exit_code);
                }
                args.request.escape_failure = Some(crate::systemd::escape_note(code, &stderr));
            }
            crate::systemd::Escape::Failed(detail) => {
                args.request.escape_failure = Some(crate::systemd::failure_note(&detail));
            }
        }
        // Could not leave the unit: the operation refuses before stopping,
        // with this note as the receipt's detail.diagnostic (or continues
        // where the unit's KillMode spares this process; operation.rs).
    }
    // Windows: the same for the job object of Raft Desktop's logon task. A
    // copy that could not break away is refused in operation.rs before
    // anything is stopped.
    #[cfg(windows)]
    if ["install", "upgrade", "repair"].contains(&args.request.command.as_str())
        && env::var_os(crate::host_supervisor::BROKE_AWAY_ENV).is_none()
        && crate::host_supervisor::confinement(&crate::host_supervisor::resolve(&cfg).await)
            .is_some()
    {
        match crate::host_supervisor::break_away(&raw_args).await {
            crate::systemd::Escape::Running(_) | crate::systemd::Escape::Exited(Some(0), _) => {
                if !args.json {
                    println!(
                        "Continuing the {} outside the Computer service's job.",
                        args.request.command
                    );
                }
                return Ok(0);
            }
            crate::systemd::Escape::Exited(..) => {
                if let Some(receipt) = crate::report::read(&cfg, &args.request.id)? {
                    let reply = Reply::receipt(receipt);
                    if args.json {
                        println!("{}", serde_json::to_string(&reply)?);
                    } else {
                        println!("{}", reply.line);
                    }
                    return Ok(reply.exit_code);
                }
            }
            crate::systemd::Escape::Failed(_) => {}
        }
    }
    let reply = supervisor::run(&cfg, &args.request).await?;
    // An answer no worker wrote down (another installation running, a
    // worker's error, a recovery worker finding nothing started): a web
    // upgrade still gets it as its receipt. The supervisor settles one it
    // gave up on itself.
    if reply.exit_code != 0
        && reply.receipt.is_none()
        && let Some(claim) = pending_claim()
    {
        settle::settle(&cfg, &args.request, &reply, claim);
    }
    if args.json {
        println!("{}", serde_json::to_string(&reply)?);
    } else {
        // Codex order: how to run it (current terminal, future terminals,
        // PATH) first, the success line last.
        if reply.exit_code == 0
            && let Some(steps) = reply
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.detail.get("pathSteps"))
                .and_then(|steps| serde_json::from_str::<Vec<String>>(steps).ok())
        {
            for step in steps {
                println!("{step}");
            }
        }
        println!("{}", reply.line);
    }
    Ok(reply.exit_code)
}

#[cfg(test)]
mod failure_line_tests {
    use super::*;

    #[test]
    fn failure_line_has_one_runnable_next_step() {
        assert_eq!(
            failure_line(),
            "The installation could not finish. Run the same install command again."
        );
    }
}

#[cfg(all(test, unix))]
mod escaped_copy_tests {
    use super::*;
    use crate::{presence::Presence, report::Outcome};
    use std::path::Path;

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

    fn web(id: &str) -> Request {
        Request {
            protocol_version: 1,
            id: id.into(),
            command: "upgrade".into(),
            version: Some("1.25.3".into()),
            channel: None,
            yes: false,
            allow_downgrade: false,
            presence: Presence::Unattended,
            approved_by: format!("remote:{id}"),
            recovery_only: false,
            waiting_caller: None,
            escape_failure: None,
        }
    }

    /// Stands in for systemd-run --scope, which execs the copy in place.
    fn copy(script: &str, home: &Path) -> Box<crate::systemd::Copy> {
        let capture = tempfile::tempfile_in(home).unwrap();
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stderr(capture.try_clone().unwrap())
            .spawn()
            .unwrap();
        Box::new(crate::systemd::Copy::new(child, Some(capture)))
    }

    #[tokio::test]
    async fn a_copy_that_fails_without_a_receipt_is_settled_as_escape_failed() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let request = web("escaped");
        let code = await_escaped_copy(
            &cfg,
            &request,
            true,
            copy(
                "echo 'Failed to start transient scope unit' >&2; exit 3",
                home.path(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
        let receipt = crate::report::read(&cfg, "escaped").unwrap().unwrap();
        assert_eq!(receipt.outcome, Outcome::Failed);
        assert_eq!(receipt.code, Some(FailureCode::EscapeFailed));
        assert_eq!(
            receipt.reason.as_deref(),
            Some(
                "The upgrade stopped before it finished, after it moved out of the Raft Computer service."
            )
        );
        assert_eq!(receipt.target_version.as_deref(), Some("1.25.3"));
        let diagnostic = &receipt.detail["diagnostic"];
        assert!(diagnostic.contains("exited with status 3"), "{diagnostic}");
        assert!(diagnostic.contains("Failed to start transient scope unit"));
    }

    #[tokio::test]
    async fn a_copy_killed_by_a_signal_is_settled_too() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let code = await_escaped_copy(&cfg, &web("killed"), true, copy("kill -9 $$", home.path()))
            .await
            .unwrap();
        assert_eq!(code, 1);
        let receipt = crate::report::read(&cfg, "killed").unwrap().unwrap();
        assert!(receipt.detail["diagnostic"].contains("was killed by a signal"));
    }

    #[tokio::test]
    async fn the_copy_s_own_receipt_is_the_answer_and_is_never_replaced() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let request = web("settled");
        let held = Reply::failure(
            "settled",
            2,
            "Held.",
            FailureCode::InstallationHeld,
            "Held by the copy.",
        );
        let own = settle::settle(&cfg, &request, &held, Claim::Own).unwrap();
        // Exits non-zero after writing it, as a held copy does.
        let code = await_escaped_copy(&cfg, &request, true, copy("exit 2", home.path()))
            .await
            .unwrap();
        assert_eq!(code, 2);
        assert_eq!(crate::report::read(&cfg, "settled").unwrap().unwrap(), own);
    }

    #[tokio::test]
    async fn a_running_operation_on_the_gate_writes_its_own_receipt() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let gate = k_carrier::lock::UpgradeLock::acquire(&cfg.installer_dir.join("gate")).unwrap();
        let code = await_escaped_copy(&cfg, &web("busy"), true, copy("exit 1", home.path()))
            .await
            .unwrap();
        assert_eq!(code, 1);
        assert!(crate::report::read(&cfg, "busy").unwrap().is_none());
        drop(gate);
    }

    #[tokio::test]
    async fn the_wait_is_bounded_and_leaves_the_copy_running() {
        let home = tempfile::tempdir().unwrap();
        let waited = copy("sleep 5", home.path())
            .wait(Duration::from_millis(100))
            .await;
        assert!(waited.is_none());
        let exited = copy("exit 4", home.path())
            .wait(Duration::from_secs(10))
            .await;
        assert_eq!(exited.map(|(code, _)| code), Some(Some(4)));
    }
}
