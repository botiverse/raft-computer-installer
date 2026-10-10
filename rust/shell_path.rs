use crate::{Result, config::Config};
use std::env;
#[cfg(not(windows))]
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

/// The lines printed after a successful install, worded after the Codex CLI
/// installer: how to run raft-computer in the current terminal, in future
/// terminals, and where PATH was written.
pub async fn ensure(cfg: &Config) -> Result<Vec<String>> {
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
    #[cfg(unix)]
    let current = format!(
        "Current terminal: export PATH=\"{}:$PATH\" && raft-computer",
        directory.display()
    );
    #[cfg(unix)]
    let future = "Future terminals: open a new terminal and run: raft-computer";
    #[cfg(windows)]
    let current = format!(
        "Current PowerShell session: $env:Path = '{};' + $env:Path; raft-computer",
        // PowerShell single quotes are literal; a quote in the path doubles.
        directory.to_string_lossy().replace('\'', "''")
    );
    #[cfg(windows)]
    let future = "Future PowerShell windows: open a new PowerShell window and run: raft-computer";
    if on_path {
        #[cfg(unix)]
        let current = "Current terminal: raft-computer";
        #[cfg(windows)]
        let current = "Current PowerShell session: raft-computer";
        return Ok(steps(&[
            &format!("{} is already on PATH", directory.display()),
            current,
            future,
        ]));
    }
    let manual = steps(&[
        &current,
        &format!("Future terminals: add {} to PATH", directory.display()),
    ]);
    if directory != crate::config::default_install_dir(&cfg.user_home)
        || env::var("RAFT_COMPUTER_NO_MODIFY_PATH").as_deref() == Ok("1")
    {
        return Ok(manual);
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
            _ => return Ok(manual),
        };
        let line = "export PATH=\"$HOME/.local/bin:$PATH\"";
        let existing = match fs::read_to_string(&profile) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Ok(manual),
        };
        let shown = shell_word(&display_under_home(&profile, &cfg.user_home));
        if existing.lines().any(|existing| existing.trim() == line) {
            return Ok(steps(&[
                &current,
                future,
                &format!("PATH is already configured in {shown}"),
            ]));
        }
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
            return Ok(manual);
        }
        // `curl … | sh` runs in a child shell, so it cannot change the user's
        // current terminal: give the line that works there.
        Ok(steps(&[
            &current,
            future,
            &format!("PATH was added to {shown}"),
        ]))
    }
    #[cfg(windows)]
    {
        // Use a literal script and an environment value: a filesystem path is
        // data, never interpolated PowerShell source.
        let mut environment = cfg.environment();
        environment.insert("RAFT_INSTALL_BIN".into(), directory.as_os_str().to_owned());
        let script = "$ErrorActionPreference='Stop'; $p=[Environment]::GetEnvironmentVariable('Path','User'); $d=$env:RAFT_INSTALL_BIN; $n={param($x) $x.Replace('/','\\').TrimEnd('\\')}; if (-not (($p -split ';') | Where-Object { (& $n $_) -ieq (& $n $d) })) { if ($p) { $p=$p+';'+$d } else { $p=$d }; [Environment]::SetEnvironmentVariable('Path',$p,'User'); 'added' } else { 'present' }";
        let result = crate::computer::run(
            std::path::Path::new("powershell.exe"),
            &["-NoProfile", "-NonInteractive", "-Command", script],
            &environment,
            std::time::Duration::from_secs(30),
        )
        .await;
        let result = match result {
            Ok(result) if result.success => result,
            _ => return Ok(manual),
        };
        let saved = if String::from_utf8_lossy(&result.stdout).trim() == "added" {
            "PATH updated for future PowerShell sessions."
        } else {
            "PATH is already configured for future PowerShell sessions."
        };
        // Pasted as `irm … | iex` the entry script runs in the user's own
        // session and adds the directory to that session's Path itself.
        let current = if env::var("RAFT_COMPUTER_SESSION_PATH").as_deref() == Ok("1") {
            "Current PowerShell session: raft-computer".to_owned()
        } else {
            current
        };
        Ok(steps(&[saved, &current, future]))
    }
}

/// `==> ` step lines, one per entry.
fn steps(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| format!("==> {line}")).collect()
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
