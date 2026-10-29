//! `raft-computer-launch.exe`: the program Raft Desktop's Windows logon task
//! (`\Raft\Raft Computer`) runs. GUI subsystem, so no console window appears.
//! It starts `raft-computer.exe start --foreground --host-supervisor
//! windows-task` next to itself and applies the supervised exit policy
//! (`launch_policy`), which Task Scheduler cannot: 75 relaunches at once,
//! 0/77/78 stay down, anything else relaunches with backoff until five
//! consecutive failed starts trip the breaker. Tripping writes the receipt,
//! deletes the task definition, and exits 0.
//!
//! Everything it starts runs in a job object that is killed with the
//! launcher (`/End` never orphans the service) and allows breakaway (the
//! detached installer leaves the job before it stops the service).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    std::process::exit(imp::main());
}

#[cfg(not(windows))]
mod imp {
    pub fn main() -> i32 {
        eprintln!("raft-computer-launch runs only on Windows");
        2
    }
}

#[cfg(windows)]
mod imp {
    use raft_computer_installer::{
        host_supervisor::{self, Registration},
        launch_policy::{self, Decision, State},
    };
    use std::{
        fs,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::GetCurrentProcess,
    };

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DEFAULT_TASK: &str = r"\Raft\Raft Computer";

    fn home_argument() -> Option<PathBuf> {
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--home" {
                return args.next().map(PathBuf::from).filter(|p| p.is_absolute());
            }
        }
        None
    }

    /// Put this process (and so everything it starts) in a job that dies with
    /// it and allows breakaway. Best effort: without it the service still
    /// runs, only `/End` could then leave it behind.
    fn enter_job() {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            AssignProcessToJobObject(job, GetCurrentProcess());
            // The handle is intentionally kept open until the process exits.
        }
    }

    fn relative(home: &Path, contract: &str) -> PathBuf {
        contract
            .split('/')
            .fold(home.to_path_buf(), |path, part| path.join(part))
    }

    fn task_name(home: &Path) -> String {
        let record =
            fs::read(relative(home, "computer/host-lifecycle-owner.json")).unwrap_or_default();
        match host_supervisor::parse(&record) {
            Registration::Registered(s) if s.kind == host_supervisor::Kind::WindowsTask => s.name,
            _ => DEFAULT_TASK.into(),
        }
    }

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    fn trip(home: &Path, receipt: &str, failed_starts: u32, last_exit: Option<i32>) {
        let path = relative(home, receipt);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = serde_json::json!({
            "formatVersion": 1,
            "kind": "windows-task",
            "by": "launcher",
            "trippedAtMs": now_ms() as u64,
            "failedStarts": failed_starts,
            "lastExitCode": last_exit,
        });
        // Receipt first, then the definition; never the reverse.
        let _ = fs::write(&path, body.to_string());
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", &task_name(home), "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }

    pub fn main() -> i32 {
        let Some(home) = home_argument() else {
            return 78;
        };
        let Some(binary) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("raft-computer.exe")))
        else {
            return 78;
        };
        enter_job();
        let policy = launch_policy::policy();
        let ready_file = relative(&home, &policy.launcher.ready_file);
        let mut state = State::default();
        loop {
            let _ = fs::remove_file(&ready_file);
            let spawned = Command::new(&binary)
                .args(["start", "--foreground", "--host-supervisor", "windows-task"])
                .env("RAFT_HOME", &home)
                .env("RAFT_COMPUTER_HOST_SUPERVISOR", "windows-task")
                .env("RAFT_COMPUTER_LAUNCHER_PID", std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            let (code, pid) = match spawned {
                Ok(mut child) => {
                    let pid = child.id();
                    (
                        child.wait().ok().and_then(|status| status.code()),
                        Some(pid),
                    )
                }
                Err(_) => (None, None),
            };
            let reached_ready = pid.is_some_and(|pid| {
                fs::read_to_string(&ready_file).is_ok_and(|text| text.trim() == pid.to_string())
            });
            match state.on_exit(&policy, code, reached_ready) {
                Decision::Relaunch { delay_ms } => {
                    std::thread::sleep(Duration::from_millis(delay_ms))
                }
                Decision::StayDown => return 0,
                Decision::Trip => {
                    trip(
                        &home,
                        &policy.launcher.breaker_receipt,
                        state.failed_starts,
                        code,
                    );
                    return 0;
                }
            }
        }
    }
}
