use crate::Result;
use k_carrier::error::invalid;
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub const INSTALLER_BIN_NAME: &str = if cfg!(windows) {
    "raft-computer-installer.exe"
} else {
    "raft-computer-installer"
};

pub const BIN_NAME: &str = if cfg!(windows) {
    "raft-computer.exe"
} else {
    "raft-computer"
};
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
    pub hands_origin: String,
    pub hands_app: String,
    pub waiting_caller: Option<crate::process::Identity>,
}

fn absolute(value: &Path, home: &Path) -> Result<PathBuf> {
    let value = if value == Path::new("~") {
        home.to_path_buf()
    } else if let Ok(tail) = value.strip_prefix("~") {
        home.join(tail)
    } else {
        value.to_path_buf()
    };
    Ok(if value.is_absolute() {
        value
    } else {
        env::current_dir()?.join(value)
    })
}

/// `~/.local/bin`, joined per component so Windows gets `\` separators
/// in the PATH entry it persists.
pub fn default_install_dir(home: &Path) -> PathBuf {
    home.join(".local").join("bin")
}

fn setting(name: &str, default: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.into()),
        _ => Err(invalid(format!("invalid {name}"))),
    }
}

/// Entries only a machine actually using `~/.slock` has (agent workspaces,
/// Computer state, daemon machine state, CLI profiles, agent ledgers).
const LEGACY_STATE_ENTRIES: [&str; 5] =
    ["agents", "computer", "machines", "profiles", "agent-state"];

/// The default Raft home, mirroring `@botiverse/raft-shared` `raftHome.ts`
/// (botiverse/slock#9189) so the installer and the Computer it installs pick
/// the same directory: `~/.slock` while it holds Raft state (machines set up
/// before the rename keep it), else `~/.raft`.
fn default_state_home(user_home: &Path) -> PathBuf {
    let legacy = user_home.join(".slock");
    if LEGACY_STATE_ENTRIES
        .iter()
        .any(|entry| legacy.join(entry).exists())
    {
        legacy
    } else {
        user_home.join(".raft")
    }
}

/// `RAFT_HOME`, then `SLOCK_HOME`, then the default under `user_home`, before
/// `~` expansion and absolutization.
fn state_home_setting(var: impl Fn(&str) -> Option<OsString>, user_home: &Path) -> PathBuf {
    var("RAFT_HOME")
        .or_else(|| var("SLOCK_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| default_state_home(user_home))
}

/// The state home another process resolved from its own environment, the way
/// [`Config::load`] does. `None` when that cannot be told from the environment
/// alone (no usable `HOME`, or a path relative to the process's working
/// directory).
pub fn state_home_of(environment: &[(OsString, OsString)]) -> Option<PathBuf> {
    let var = |key: &str| {
        environment
            .iter()
            .rev()
            .find(|(name, _)| name.as_os_str() == key)
            .map(|(_, value)| value.clone())
    };
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let user_home = var(home_key).filter(|p| !p.is_empty()).map(PathBuf::from)?;
    if !user_home.is_absolute() {
        return None;
    }
    let setting = state_home_setting(var, &user_home);
    let expanded = if setting == Path::new("~") {
        user_home
    } else if let Ok(tail) = setting.strip_prefix("~") {
        user_home.join(tail)
    } else {
        setting
    };
    expanded.is_absolute().then_some(expanded)
}

impl Config {
    pub fn load() -> Result<Self> {
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let user_home = env::var_os(home_key)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| invalid("user home is unavailable"))?;
        if !user_home.is_absolute() {
            return Err(invalid("user home must be absolute"));
        }
        let state_home = absolute(
            &state_home_setting(|key| env::var_os(key), &user_home),
            &user_home,
        )?;
        let install_dir = absolute(
            &env::var_os("RAFT_COMPUTER_INSTALL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| default_install_dir(&user_home)),
            &user_home,
        )?;
        let binary = match env::var_os("RAFT_COMPUTER_BINARY") {
            Some(path) => absolute(Path::new(&path), &user_home)?,
            None => install_dir.join(BIN_NAME),
        };
        // A custom binary still needs its sidecar adjacent to that binary.
        let sidecar = binary
            .parent()
            .ok_or_else(|| invalid("invalid binary path"))?
            .join(SIDECAR_NAME);
        Ok(Self {
            waiting_caller: None,
            k_state: state_home.join("computer/k"),
            installer_dir: state_home.join("computer/installer"),
            user_home,
            state_home,
            install_dir,
            binary,
            sidecar,
            hands_origin: setting("RAFT_COMPUTER_HANDS_ORIGIN", "https://hands.build")?,
            hands_app: setting("RAFT_COMPUTER_HANDS_APP", "raft-computer-cli")?,
        })
    }

    /// Variables that describe one installer run: which operation it is, who
    /// asked for it, whether it may prompt, and which caller waits. A Computer
    /// the installer starts must not inherit them, or every runner and agent
    /// would carry this run's id and `remote:` label into later upgrades
    /// (#proj-raft-computer:#32). Configuration such as
    /// RAFT_COMPUTER_INSTALLER_CHANNEL is not listed: it is meant to persist.
    pub const OPERATION_ENV: [&'static str; 4] = [
        "RAFT_COMPUTER_OPERATION_ID",
        "RAFT_COMPUTER_APPROVED_BY",
        "RAFT_COMPUTER_NON_INTERACTIVE",
        "RAFT_COMPUTER_INSTALLER_CALLER",
    ];

    pub fn environment(&self) -> BTreeMap<OsString, OsString> {
        BTreeMap::from([
            ("RAFT_HOME".into(), self.state_home.as_os_str().to_owned()),
            ("SLOCK_HOME".into(), self.state_home.as_os_str().to_owned()),
        ])
    }

    /// One runnable copy of the installer itself, refreshed at every
    /// operation start. Failure hints print this path, so it must survive
    /// both process exit and supervisor scratch cleanup.
    pub fn durable_binary(&self) -> PathBuf {
        self.installer_dir.join("bin").join(INSTALLER_BIN_NAME)
    }

    pub fn scratch(&self) -> PathBuf {
        self.installer_dir.join("scratch")
    }

    pub fn sidecar_dir(&self, version: &str) -> Result<PathBuf> {
        crate::version::exact(version)?;
        Ok(self.installer_dir.join("sidecars").join(version))
    }

    // Request IDs are opaque protocol strings, never filesystem components.
    pub fn receipt_path(&self, id: &str) -> PathBuf {
        self.installer_dir.join("receipts").join(format!(
            "{}.json",
            k_carrier::artifact::sha256(id.as_bytes())
        ))
    }
}

pub fn platform() -> Result<(&'static str, &'static str)> {
    let os = match env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        _ => return Err(invalid("unsupported operating system")),
    };
    let arch = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return Err(invalid("unsupported architecture")),
    };
    if os == "win32" && arch != "x64" {
        return Err(invalid("unsupported Windows architecture"));
    }
    Ok((os, arch))
}

pub fn platform_key() -> Result<String> {
    let (os, arch) = platform()?;
    Ok(format!("{os}-{arch}"))
}

#[cfg(test)]
mod default_state_home_tests {
    use super::default_state_home;
    use std::fs;

    #[test]
    fn new_machine_uses_raft() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(default_state_home(home.path()), home.path().join(".raft"));
    }

    #[test]
    fn existing_slock_with_state_is_kept_even_next_to_raft() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".slock/computer")).unwrap();
        fs::create_dir_all(home.path().join(".raft/computer")).unwrap();
        assert_eq!(default_state_home(home.path()), home.path().join(".slock"));
    }

    #[test]
    fn slock_with_only_a_notice_file_does_not_count() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".slock")).unwrap();
        fs::write(home.path().join(".slock/.cli-rename-notice"), "").unwrap();
        assert_eq!(default_state_home(home.path()), home.path().join(".raft"));
    }
}

#[cfg(all(test, unix))]
mod state_home_of_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn resolves_like_config_load() {
        let user = tempfile::tempdir().unwrap();
        let home = user.path().to_str().unwrap();
        assert_eq!(
            state_home_of(&env(&[("HOME", home), ("RAFT_HOME", "/srv/rw")])),
            Some(PathBuf::from("/srv/rw"))
        );
        assert_eq!(
            state_home_of(&env(&[("HOME", home), ("SLOCK_HOME", "~/rw")])),
            Some(user.path().join("rw"))
        );
        assert_eq!(
            state_home_of(&env(&[("HOME", home)])),
            Some(user.path().join(".raft"))
        );
        std::fs::create_dir_all(user.path().join(".slock/computer")).unwrap();
        assert_eq!(
            state_home_of(&env(&[("HOME", home)])),
            Some(user.path().join(".slock"))
        );
    }

    #[test]
    fn unknown_without_home_or_with_a_relative_path() {
        assert_eq!(state_home_of(&env(&[("RAFT_HOME", "/srv/rw")])), None);
        assert_eq!(
            state_home_of(&env(&[("HOME", "/u"), ("RAFT_HOME", "rw")])),
            None
        );
    }
}
