use crate::{Result, config::Config};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
};

pub async fn ensure(cfg: &Config) -> Result<Option<String>> {
    let directory = cfg.binary.parent().unwrap_or(&cfg.install_dir);
    let on_path = env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|entry| {
            #[cfg(windows)]
            {
                entry
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&directory.to_string_lossy())
            }
            #[cfg(not(windows))]
            {
                entry == directory
            }
        })
    });
    if on_path {
        return Ok(None);
    }
    let hint = format!("Add {} to PATH.", directory.display());
    if directory != cfg.user_home.join(".local/bin")
        || env::var("RAFT_COMPUTER_NO_MODIFY_PATH").as_deref() == Ok("1")
    {
        return Ok(Some(hint));
    }
    #[cfg(unix)]
    {
        let shell = env::var("SHELL").unwrap_or_default();
        let profile = match shell.rsplit('/').next() {
            Some("zsh") => env::var_os("ZDOTDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| cfg.user_home.clone())
                .join(".zshrc"),
            Some("bash") => cfg.user_home.join(".bashrc"),
            _ => return Ok(Some(hint)),
        };
        let line = "export PATH=\"$HOME/.local/bin:$PATH\"";
        let existing = match fs::read_to_string(&profile) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Ok(Some(hint)),
        };
        if !existing.lines().any(|existing| existing.trim() == line) {
            use std::os::unix::fs::OpenOptionsExt;
            // Append through a user's existing profile symlink; never replace
            // their dotfile or change its permissions using an atomic rewrite.
            let updated = (|| -> std::io::Result<()> {
                if let Some(parent) = profile.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .open(&profile)?;
                file.write_all(format!("\n# raft-computer\n{line}\n").as_bytes())?;
                file.sync_all()
            })();
            if updated.is_err() {
                return Ok(Some(hint));
            }
        }
        Ok(Some("Open a new terminal to use raft-computer.".into()))
    }
    #[cfg(windows)]
    {
        // Use a literal script and an environment value: a filesystem path is
        // data, never interpolated PowerShell source.
        let mut environment = cfg.environment();
        environment.insert("RAFT_INSTALL_BIN".into(), directory.as_os_str().to_owned());
        let script = "$ErrorActionPreference='Stop'; $p=[Environment]::GetEnvironmentVariable('Path','User'); $d=$env:RAFT_INSTALL_BIN; if (-not (($p -split ';') -contains $d)) { if ($p) { $p=$p+';'+$d } else { $p=$d }; [Environment]::SetEnvironmentVariable('Path',$p,'User') }";
        let result = crate::computer::run(
            std::path::Path::new("powershell.exe"),
            &["-NoProfile", "-NonInteractive", "-Command", script],
            &environment,
            std::time::Duration::from_secs(30),
        )
        .await;
        Ok(Some(if result.is_ok_and(|r| r.success) {
            "Open a new terminal to use raft-computer.".into()
        } else {
            hint
        }))
    }
}
