use crate::{
    Error, Result, computer,
    config::Config,
    diagnostic, host, operation,
    presence::Interaction,
    report::FailureCode,
    request::{Reply, Request},
    source, supervisor, version,
};
use k_carrier::error::invalid;
use std::{env, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

pub fn failure_line() -> &'static str {
    "The installation could not finish. Run the same install command again."
}

pub fn preserve_top_level_failure(error: &Error) {
    diagnostic::preserve_top_level_failure(error);
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
            Reply::failure(
                &request.id,
                if unresolved { 3 } else { 1 },
                failure_line(),
                if unresolved {
                    FailureCode::RecoveryUnresolved
                } else {
                    FailureCode::InstallFailed
                },
                "The installation could not finish.",
            )
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
    let mut args = parse(&args)?;
    if ["install", "upgrade", "repair"].contains(&args.request.command.as_str()) {
        let marker = env::var("RAFT_COMPUTER_INSTALLER_CALLER").ok();
        if marker.as_deref().is_some_and(|v| v != "waiting-cli-v1") {
            return Err(invalid("invalid caller declaration"));
        }
        // The waiting CLI may run from the installed binary or, on a legacy
        // two-layer install, from the slot artifact its launcher execs into.
        let parent = crate::process::immediate_parent(&host::product_executables(&cfg))?;
        if marker.is_some() && parent.is_none() {
            return Err(invalid(
                "waiting caller is not the installed immediate parent",
            ));
        }
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
            if args.json {
                println!("{}", serde_json::to_string(&reply)?);
            } else {
                eprintln!("{}", reply.line);
            }
            return Ok(reply.exit_code);
        }
        // A web upgrade's installer is a direct child of the running service,
        // with no Computer above it and no CLI declaring itself. A `remote:`
        // label anywhere else is a leaked one (an old installer passed it to
        // the service and every agent): refuse, and say how to drop it, rather
        // than treat it as an ordinary local upgrade.
        let from_service = parent.is_some() && marker.is_none();
        if args.request.approved_by.starts_with("remote:") && !from_service {
            let reply = Reply::failure(
                &args.request.id,
                2,
                LEAKED_REMOTE_LINE,
                FailureCode::InstallationHeld,
                "A web upgrade's approval was found outside that upgrade.",
            );
            if args.json {
                println!("{}", serde_json::to_string(&reply)?);
            } else {
                eprintln!("{}", reply.line);
            }
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
                if args.json {
                    println!("{}", serde_json::to_string(&reply)?);
                } else {
                    eprintln!("{}", reply.line);
                }
                return Ok(reply.exit_code);
            }
        }
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
        match crate::systemd::escape(&unit, &raw_args).await {
            crate::systemd::Escape::Running | crate::systemd::Escape::Exited(Some(0)) => {
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
            crate::systemd::Escape::Exited(_) => {
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
            crate::systemd::Escape::Failed => {}
        }
        // Could not leave the unit: the operation refuses before stopping.
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
            crate::systemd::Escape::Running | crate::systemd::Escape::Exited(Some(0)) => {
                if !args.json {
                    println!(
                        "Continuing the {} outside the Computer service's job.",
                        args.request.command
                    );
                }
                return Ok(0);
            }
            crate::systemd::Escape::Exited(_) => {
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
            crate::systemd::Escape::Failed => {}
        }
    }
    let reply = supervisor::run(&cfg, &args.request).await?;
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
