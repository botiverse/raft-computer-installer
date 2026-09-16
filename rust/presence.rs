use crate::Result;
use serde::{Deserialize, Serialize};
use std::{env, fs::{File, OpenOptions}, io::{Read, Write}};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence { Attended, Unattended }

/// Retain the terminal opened at entry: presence is decided once, even when
/// stdin is a bootstrap pipeline. Questions and setup use this same terminal.
pub struct Interaction {
    pub presence: Presence,
    input: Option<File>,
    output: Option<File>,
}

fn enabled(name: &str) -> bool {
    env::var(name).is_ok_and(|v| !v.is_empty() && !["0", "false", "no"].contains(&v.to_ascii_lowercase().as_str()))
}

impl Interaction {
    /// A supervised worker uses the presence decided by its CLI parent. Losing
    /// the terminal does not silently turn an attended request into consent.
    pub fn for_presence(presence: Presence) -> Result<Self> {
        if presence == Presence::Unattended {
            return Ok(Self { presence, input: None, output: None });
        }
        #[cfg(unix)]
        let input = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        #[cfg(unix)]
        let output = input.try_clone()?;
        #[cfg(windows)]
        let input = OpenOptions::new().read(true).open("CONIN$")?;
        #[cfg(windows)]
        let output = OpenOptions::new().write(true).open("CONOUT$")?;
        Ok(Self { presence, input: Some(input), output: Some(output) })
    }

    pub fn detect() -> Self {
        let unattended = Self { presence: Presence::Unattended, input: None, output: None };
        if enabled("CI") || enabled("RAFT_COMPUTER_NON_INTERACTIVE") { return unattended; }
        #[cfg(unix)]
        let terminal = OpenOptions::new().read(true).write(true).open("/dev/tty")
            .and_then(|input| input.try_clone().map(|output| (input, output)));
        #[cfg(windows)]
        let terminal = OpenOptions::new().read(true).open("CONIN$")
            .and_then(|input| OpenOptions::new().write(true).open("CONOUT$").map(|output| (input, output)));
        match terminal {
            Ok((input, output)) => Self { presence: Presence::Attended, input: Some(input), output: Some(output) },
            Err(_) => unattended,
        }
    }

    pub fn ask(&mut self, question: &str) -> Result<bool> {
        let (Some(input), Some(output)) = (&mut self.input, &mut self.output) else { return Ok(false); };
        write!(output, "{question} [y/N] ")?;
        output.flush()?;
        let mut answer = Vec::new();
        let mut byte = [0u8; 1];
        let mut too_long = false;
        while input.read(&mut byte)? != 0 {
            if byte[0] == b'\n' { break; }
            if answer.len() < 256 { answer.push(byte[0]); } else { too_long = true; }
        }
        let answer = String::from_utf8_lossy(&answer);
        Ok(!too_long && ["y", "yes"].contains(&answer.trim().to_ascii_lowercase().as_str()))
    }

    pub fn setup_stdio(&self) -> Result<Option<(File, File, File)>> {
        match (&self.input, &self.output) {
            (Some(input), Some(output)) => Ok(Some((input.try_clone()?, output.try_clone()?, output.try_clone()?))),
            _ => Ok(None),
        }
    }
}
