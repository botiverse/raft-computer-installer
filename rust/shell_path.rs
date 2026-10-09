use crate::{Result, config::Config};
use std::env;
#[cfg(not(windows))]
use std::{
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
            // macOS terminals start login shells, which read .bash_profile
            // (not .bashrc); Linux terminals start interactive non-login ones.
            Some("bash") if cfg!(target_os = "macos") => cfg.user_home.join(".bash_profile"),
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
        // `curl … | sh` runs in a child shell, so it cannot change the user's
        // current terminal: name the profile it just updated, ready to source.
        Ok(Some(format!(
            "New terminals will find raft-computer. To use it in this terminal now, run: source {}",
            shell_word(&display_under_home(&profile, &cfg.user_home))
        )))
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
            // Pasted as `irm … | iex` the entry script runs in the user's own
            // session and adds the directory to that session's Path itself.
            if env::var("RAFT_COMPUTER_SESSION_PATH").as_deref() == Ok("1") {
                return Ok(Some(
                    "raft-computer is ready in this PowerShell and in new windows.".into(),
                ));
            }
            // PowerShell single quotes are literal; a quote in the path doubles.
            let quoted = directory.to_string_lossy().replace('\'', "''");
            format!(
                "New terminals will find raft-computer. To use it in this PowerShell now, run: $env:Path = '{quoted};' + $env:Path"
            )
        } else {
            hint
        }))
    }
}

/// `~/…` for a path under the user's home, as people type it.
#[cfg(unix)]
fn display_under_home(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A shell word that pastes safely: plain paths stay as they are; anything
/// else is single-quoted (a leading `~/` stays outside so it still expands).
#[cfg(unix)]
fn shell_word(text: &str) -> String {
    let plain = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+~".contains(c))
    };
    if plain(text) {
        return text.to_owned();
    }
    let (prefix, rest) = match text.strip_prefix("~/") {
        Some(rest) => ("~/", rest),
        None => ("", text),
    };
    format!("{prefix}'{}'", rest.replace('\'', "'\\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn profile_is_shown_under_home_and_quoted_only_when_needed() {
        let home = Path::new("/home/u");
        assert_eq!(
            shell_word(&display_under_home(Path::new("/home/u/.bashrc"), home)),
            "~/.bashrc"
        );
        assert_eq!(
            shell_word(&display_under_home(Path::new("/home/u/cfg/.zshrc"), home)),
            "~/cfg/.zshrc"
        );
        assert_eq!(
            shell_word(&display_under_home(Path::new("/etc/zsh/.zshrc"), home)),
            "/etc/zsh/.zshrc"
        );
        assert_eq!(
            shell_word(&display_under_home(
                Path::new("/home/u/my dir/.zshrc"),
                home
            )),
            "~/'my dir/.zshrc'"
        );
        assert_eq!(shell_word("/o'b/.zshrc"), "'/o'\\''b/.zshrc'");
    }
}
