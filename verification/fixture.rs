//! Native process fixture. Its service answers over a real loopback socket;
//! stale PID files are never treated as evidence that a service is alive.
use k_carrier::storage::{ensure_dir, write_json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const fn template() -> [u8; 512] {
    let text = b"{\"version\":\"0.0.0\",\"marker\":\"RAFT_NATIVE_FIXTURE_CONFIGURATION_V1\"}";
    let mut bytes = [0u8; 512];
    let mut i = 0;
    while i < text.len() {
        bytes[i] = text[i];
        i += 1;
    }
    bytes
}

#[used]
#[unsafe(no_mangle)]
pub static RAFT_NATIVE_FIXTURE_CONFIG: [u8; 512] = template();

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Behavior {
    version: String,
    reported_version: Option<String>,
    live_version: Option<String>,
    #[serde(default)]
    start_fail: bool,
    #[serde(default)]
    stop_broken: bool,
    #[serde(default)]
    status_unsupported: bool,
}

#[derive(Serialize, Deserialize)]
struct State {
    address: String,
    token: String,
    version: String,
    pid: u32,
    generation: String,
}

fn exchange(home: &Path, action: &str) -> Option<Value> {
    let state: State =
        serde_json::from_slice(&fs::read(home.join("fixture-service.json")).ok()?).ok()?;
    let address = state.address.parse::<std::net::SocketAddr>().ok()?;
    if !address.ip().is_loopback() {
        return None;
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    writeln!(stream, "{}", json!({"action":action,"token":state.token})).ok()?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(4097)
        .read_to_string(&mut line)
        .ok()?;
    let reply: Value = serde_json::from_str(&line).ok()?;
    (reply["generation"] == state.generation).then_some(reply)
}

fn gate(variable: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os(variable) {
        let path = PathBuf::from(path);
        write_json(&path, &json!({"pid":std::process::id()}))?;
        let deadline = Instant::now() + Duration::from_secs(90);
        while !path.with_extension("release").exists() {
            if Instant::now() >= deadline {
                return Err("fixture gate timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

fn service(home: &Path, behavior: &Behavior) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let state = State {
        address: listener.local_addr()?.to_string(),
        token: uuid::Uuid::new_v4().to_string(),
        version: behavior
            .live_version
            .clone()
            .unwrap_or_else(|| behavior.version.clone()),
        pid: std::process::id(),
        generation: uuid::Uuid::new_v4().to_string(),
    };
    write_json(&home.join("fixture-service.json"), &state)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut request = String::new();
        if BufReader::new(stream.try_clone()?)
            .take(4097)
            .read_line(&mut request)
            .is_err()
        {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&request) else {
            continue;
        };
        if request["token"] != state.token {
            continue;
        }
        writeln!(
            stream,
            "{}",
            json!({"generation":state.generation,"pid":state.pid,"version":state.version})
        )?;
        if request["action"] == "upgrade" {
            let child =
                Command::new(env::var_os("RCI_FIXTURE_INSTALLER").ok_or("installer missing")?)
                    .args(["upgrade", "--version", "1.1.0", "--json"])
                    .env_remove("RAFT_COMPUTER_INSTALLER_CALLER")
                    .env("RAFT_COMPUTER_NON_INTERACTIVE", "1")
                    .env("RAFT_COMPUTER_OPERATION_ID", "remote-caller-regression")
                    .env(
                        "RAFT_COMPUTER_APPROVED_BY",
                        "remote:remote-caller-regression",
                    )
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
            let identity = raft_computer_installer::process::observe(child.id())?
                .ok_or("remote installer identity unavailable")?;
            write_json(&home.join("fixture-remote-installer.json"), &identity)?;
        }
        if request["action"] == "stop" {
            break;
        }
    }
    Ok(())
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    k_carrier::host::isolate_standard_handles()?;
    // Volatile access prevents LLVM from replacing configurable data reads with
    // constants. The harness changes this fixed-size blob before ad-hoc signing.
    let bytes = unsafe { std::ptr::read_volatile(&RAFT_NATIVE_FIXTURE_CONFIG) };
    let used = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let behavior: Behavior = serde_json::from_slice(&bytes[..used])?;
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--version") {
        if env::var_os("RCI_FIXTURE_PUBLISHED_PATH")
            .is_some_and(|path| path == env::current_exe().unwrap_or_default())
            && env::var("RCI_FIXTURE_PUBLISHED_VERSION")
                .is_ok_and(|version| version == behavior.version)
        {
            gate("RCI_FIXTURE_PUBLISHED_GATE")?;
        }
        println!(
            "{}",
            behavior
                .reported_version
                .as_ref()
                .unwrap_or(&behavior.version)
        );
        return Ok(0);
    }
    let home = PathBuf::from(
        env::var_os("RAFT_HOME")
            .or_else(|| env::var_os("SLOCK_HOME"))
            .ok_or("fixture home missing")?,
    );
    ensure_dir(&home)?;
    match args.first().map(String::as_str) {
        Some("__wait-remote-installer") => {
            let identity =
                serde_json::from_slice(&fs::read(home.join("fixture-remote-installer.json"))?)?;
            let deadline = Instant::now() + Duration::from_secs(60);
            while raft_computer_installer::process::instance_matches(&identity)? {
                if Instant::now() >= deadline {
                    return Err("remote installer did not exit".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // Exercise the real Computer adapter shape: the installed executable
        // waits for its installer child, while retaining its own image.
        Some("upgrade") => {
            let installer = env::var_os("RCI_FIXTURE_INSTALLER").ok_or("installer missing")?;
            let status = Command::new(installer).args(&args).status()?;
            return Ok(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1));
        }
        Some("__service") => service(&home, &behavior)?,
        Some("login") => {
            fs::write(home.join("fixture-login"), b"configured")?;
        }
        Some("start") => {
            if !home.join("fixture-login").exists() || behavior.start_fail {
                return Ok(1);
            }
            if exchange(&home, "probe").is_some() {
                return Ok(0);
            }
            let mut command = Command::new(env::current_exe()?);
            command
                .arg("__service")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x00000008 | 0x00000200);
            }
            let mut child = command.spawn()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while exchange(&home, "probe").is_none() {
                if child.try_wait()?.is_some() || Instant::now() >= deadline {
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Some("stop") => {
            gate("RCI_FIXTURE_STOP_GATE")?;
            if behavior.stop_broken {
                return Ok(0);
            }
            exchange(&home, "stop");
            let deadline = Instant::now() + Duration::from_secs(5);
            while exchange(&home, "probe").is_some() {
                if Instant::now() >= deadline {
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Some("status") if args.get(1).map(String::as_str) == Some("--json") => {
            if behavior.status_unsupported {
                eprintln!("error: unknown option '--json'");
                return Ok(1);
            }
            let live = exchange(&home, "probe");
            let mut answer = json!({"running":live.is_some(),"nextStep":if home.join("fixture-login").exists() { Value::Null } else { json!("run raft-computer login") }});
            if let Some(live) = live {
                answer["attestation"] = json!({"servicePid":live["pid"],"computerVersion":live["version"],"serviceGeneration":live["generation"]});
            }
            println!("{answer}");
        }
        _ => return Ok(2),
    }
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(i32::from(code)),
        Err(error) => {
            eprintln!("fixture: {error}");
            std::process::exit(1);
        }
    }
}
