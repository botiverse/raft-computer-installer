// Windows installers carry `raft-computer-launch.exe` (rust/launcher.rs) and
// publish it next to `raft-computer.exe`, so the launcher a Raft Desktop
// logon task runs always comes from the same release as the installer.
// scripts/build.py builds the launcher first and points this variable at it;
// any other build embeds nothing and publishes no launcher.
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=RAFT_COMPUTER_LAUNCHER_EXE");
    let out =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("raft-computer-launch.bin");
    match env::var_os("RAFT_COMPUTER_LAUNCHER_EXE") {
        Some(path) => {
            println!("cargo:rerun-if-changed={}", PathBuf::from(&path).display());
            fs::copy(&path, &out).expect("RAFT_COMPUTER_LAUNCHER_EXE must name the built launcher");
        }
        None => fs::write(&out, b"").expect("write empty launcher"),
    }
}
