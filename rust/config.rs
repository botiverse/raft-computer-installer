use crate::Result;
use k_carrier::error::invalid;
use std::{collections::BTreeMap, env, ffi::OsString, path::{Path, PathBuf}};

pub const BIN_NAME: &str = if cfg!(windows) { "raft-computer.exe" } else { "raft-computer" };
pub const SIDECAR_NAME: &str = "photon_rs_bg.wasm";

#[derive(Clone, Debug)]
pub struct Config {
    pub user_home: PathBuf,
    pub state_home: PathBuf,
    pub k_state: PathBuf,
    pub installer_dir: PathBuf,
    pub install_dir: PathBuf,
    pub binary: PathBuf,
    pub sidecar: PathBuf,
    pub release_base: String,
    pub hands_origin: String,
    pub hands_app: String,
}

fn absolute(value: &Path, home: &Path) -> Result<PathBuf> {
    let value = if value == Path::new("~") {
        home.to_path_buf()
    } else if let Ok(tail) = value.strip_prefix("~") {
        home.join(tail)
    } else {
        value.to_path_buf()
    };
    Ok(if value.is_absolute() { value } else { env::current_dir()?.join(value) })
}

fn setting(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.into()),
        _ => Err(invalid(format!("invalid {name}"))),
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let user_home = env::var_os(home_key).filter(|p| !p.is_empty())
            .map(PathBuf::from).ok_or_else(|| invalid("user home is unavailable"))?;
        if !user_home.is_absolute() { return Err(invalid("user home must be absolute")); }
        let state_home = absolute(&env::var_os("RAFT_HOME").or_else(|| env::var_os("SLOCK_HOME"))
            .map(PathBuf::from).unwrap_or_else(|| user_home.join(".slock")), &user_home)?;
        let install_dir = absolute(&env::var_os("RAFT_COMPUTER_INSTALL_DIR")
            .map(PathBuf::from).unwrap_or_else(|| user_home.join(".local/bin")), &user_home)?;
        let binary = match env::var_os("RAFT_COMPUTER_BINARY") {
            Some(path) => absolute(Path::new(&path), &user_home)?,
            None => install_dir.join(BIN_NAME),
        };
        // A custom binary still needs its sidecar adjacent to that binary.
        let sidecar = binary.parent().ok_or_else(|| invalid("invalid binary path"))?.join(SIDECAR_NAME);
        Ok(Self {
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            user_home, state_home, install_dir, binary, sidecar,
            release_base: setting("RAFT_COMPUTER_RELEASE_BASE", "https://cdn.raft.build/computer")?,
            hands_origin: setting("RAFT_COMPUTER_HANDS_ORIGIN", "https://hands.build")?,
            hands_app: setting("RAFT_COMPUTER_HANDS_APP", "raft-computer-cli")?,
        })
    }

    pub fn environment(&self) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([
            ("RAFT_HOME".into(), self.state_home.as_os_str().to_owned()),
            ("SLOCK_HOME".into(), self.state_home.as_os_str().to_owned()),
        ])
    }

    pub fn scratch(&self) -> PathBuf { self.installer_dir.join("scratch") }

    pub fn sidecar_dir(&self, version: &str) -> Result<PathBuf> {
        crate::version::exact(version)?;
        Ok(self.installer_dir.join("sidecars").join(version))
    }

    // Request IDs are opaque protocol strings, never filesystem components.
    pub fn receipt_path(&self, id: &str) -> PathBuf {
        self.installer_dir.join("receipts")
            .join(format!("{}.json", k_carrier::artifact::sha256(id.as_bytes())))
    }
}

pub fn platform() -> Result<(&'static str, &'static str)> {
    let os = match env::consts::OS {
        "macos" => "darwin", "linux" => "linux", "windows" => "win32",
        _ => return Err(invalid("unsupported operating system")),
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "x64", "aarch64" => "arm64",
        _ => return Err(invalid("unsupported architecture")),
    };
    if os == "win32" && arch != "x64" { return Err(invalid("unsupported Windows architecture")); }
    Ok((os, arch))
}

pub fn platform_key() -> Result<String> {
    let (os, arch) = platform()?;
    Ok(format!("{os}-{arch}"))
}
