use crate::{
    Error, Result, computer,
    config::Config,
    host, operation,
    presence::Interaction,
    request::{Reply, Request},
    source, supervisor, version,
};
use k_carrier::error::invalid;
use std::{env, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

const HELP: &str = "raft-computer-installer

  install|upgrade [--version V | --channel main|alpha|NAME] [--yes] [--allow-downgrade] [--json]
  repair          [--version V | --channel main|alpha|NAME] [--yes] [--allow-downgrade] [--json]
  status          [--json]
  recover         [--json]

No command means upgrade. Unattended invocations never ask questions.
Repair is allowed only for a broken installation and preserves its previous state.
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
            _ => return Err(invalid("unknown installer option")),
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
        Err(Error::Locked(_)) => Reply::plain(
            &request.id,
            2,
            "Another installer is running on this machine.",
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
            eprintln!("Installer: {error}");
            Reply::plain(
                &request.id,
                if unresolved { 3 } else { 1 },
                "The installer could not finish. Run raft-computer-installer status for the current state.",
            )
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
    let args = parse(&args)?;
    let reply = supervisor::run(&cfg, &args.request).await?;
    if args.json {
        println!("{}", serde_json::to_string(&reply)?);
    } else {
        println!("{}", reply.line);
    }
    Ok(reply.exit_code)
}
