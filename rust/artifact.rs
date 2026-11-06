//! Preparation completes before any service is stopped or installed bytes
//! are replaced. Sidecar identity is retained for offline recovery.
use crate::{
    Result, computer,
    config::{BIN_NAME, Config, SIDECAR_NAME, platform},
    source::Manifest,
    version,
};
use k_carrier::{
    artifact::{Downloader, Progress, Release, TransferPolicy, sha256},
    error::invalid,
    storage::{ensure_dir, write_durable, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const PROGRESS_BAR_WIDTH: u64 = 30;
/// A download failure is user-facing and starts with this text, so the
/// operation can print why the download failed instead of a bare retry.
const DOWNLOAD_FAILURE: &str = "Could not download ";

pub fn is_download_failure(error: &crate::Error) -> bool {
    matches!(error, crate::Error::Invalid(message) if message.starts_with(DOWNLOAD_FAILURE))
}

/// Name the failed download, its URL and the cause in user language. Errors
/// that are not transfer failures pass through unchanged.
fn download_failure(label: &str, release: &Release, error: crate::Error) -> crate::Error {
    let url = release.gzip.as_ref().map_or(&release.url, |gzip| &gzip.url);
    let Ok(url) = reqwest::Url::parse(url) else {
        return error;
    };
    let host = url.host_str().unwrap_or_default().to_owned();
    let cause = match &error {
        crate::Error::Http(error) if error.is_timeout() => "the download timed out".into(),
        crate::Error::Http(error) if crate::source::certificate_rejected(error) => {
            crate::source::CERTIFICATE_REJECTED.into()
        }
        crate::Error::Http(error) if error.is_connect() => "the connection failed".into(),
        crate::Error::Http(error) if error.is_redirect() => "it redirected too many times".into(),
        crate::Error::Http(_) => "the download failed".into(),
        crate::Error::Invalid(message) => {
            if message.starts_with("SHA256_MISMATCH") || message.starts_with("SIZE_MISMATCH") {
                format!(
                    "the downloaded file does not match the published release; a company firewall or proxy may have replaced it. Ask your network administrator to allow {host} and *.r2.cloudflarestorage.com"
                )
            } else if let Some(detail) = message.strip_prefix("DOWNLOAD_FAILED: ") {
                if detail.ends_with("timeout") {
                    "the download timed out".into()
                } else if let Some(status) = detail.strip_prefix("HTTP ") {
                    format!("the server returned HTTP {status}")
                } else {
                    format!("the download failed ({detail})")
                }
            } else {
                return error;
            }
        }
        _ => return error,
    };
    invalid(format!(
        "{DOWNLOAD_FAILURE}{label} from {}: {cause}",
        crate::source::shown_url(&url)
    ))
}

/// Pauses before the second and the third attempt of one download. Each
/// attempt resumes from the partial file the previous one left behind.
const DOWNLOAD_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// The least overall time any one attempt gets. K budgets a transfer at its
/// size over 64 KiB/s plus the 30 s first-response allowance, so a small file
/// on a slow link (#proj-raft-computer:#49: the support file over a slow
/// Windows connection) failed after well under a minute, every time.
const MINIMUM_ATTEMPT_MS: u64 = 120_000;

/// K's policy exposes no minimum overall budget, so the floor is reached by
/// lowering the required rate for a small transfer until its size takes at
/// least the floor minus the first-response allowance. The first-response and
/// idle timeouts stay as K sets them: a stall there is retried instead. Large
/// transfers, whose own budget exceeds the floor, keep K's policy unchanged.
fn with_minimum_attempt(policy: TransferPolicy, size: u64) -> TransferPolicy {
    let body_ms = MINIMUM_ATTEMPT_MS.saturating_sub(policy.response_ms);
    if body_ms == 0 {
        return policy;
    }
    // Rounded down, so size / rate is never shorter than body_ms.
    let rate = u64::try_from(u128::from(size) * 1000 / u128::from(body_ms))
        .unwrap_or(u64::MAX)
        .max(1);
    TransferPolicy {
        minimum_bytes_per_second: policy.minimum_bytes_per_second.min(rate),
        maximum_overall_ms: policy.maximum_overall_ms.max(MINIMUM_ATTEMPT_MS),
        ..policy
    }
}

/// A failure the same request may not repeat: a timeout, a dropped or refused
/// connection, or a server error (5xx). A refusal (4xx), bytes that do not
/// match the release, a rejected certificate, a redirect loop and local I/O
/// errors are final.
fn transient(error: &crate::Error) -> bool {
    match error {
        crate::Error::Http(error) => {
            !crate::source::certificate_rejected(error)
                && (error.is_timeout()
                    || error.is_connect()
                    || error.is_request()
                    || error.is_body()
                    || error.is_decode())
        }
        crate::Error::Invalid(message) => {
            message
                .strip_prefix("DOWNLOAD_FAILED: ")
                .is_some_and(|detail| {
                    detail.ends_with(" timeout")
                        || detail
                            .strip_prefix("HTTP ")
                            .is_some_and(|status| status.starts_with('5'))
                })
        }
        _ => false,
    }
}

async fn download(
    downloader: &Downloader,
    release: &Release,
    resume_dir: &Path,
    label: &'static str,
) -> Result<Vec<u8>> {
    download_with_retries(
        downloader,
        release,
        resume_dir,
        label,
        &DOWNLOAD_RETRY_DELAYS,
    )
    .await
}

async fn download_with_retries(
    downloader: &Downloader,
    release: &Release,
    resume_dir: &Path,
    label: &'static str,
    delays: &[Duration],
) -> Result<Vec<u8>> {
    let size = release.gzip.as_ref().map_or(release.size, |gzip| gzip.size);
    let downloader = Downloader {
        client: downloader.client.clone(),
        policy: with_minimum_attempt(downloader.policy, size),
    };
    let mut delays = delays.iter();
    loop {
        let error = match downloader
            .download(release, Some(resume_dir), Some(download_progress(label)))
            .await
        {
            Ok(bytes) => return Ok(bytes),
            Err(error) => error,
        };
        match delays.next() {
            Some(delay) if transient(&error) => {
                eprintln!("The {label} download was interrupted; trying again.");
                tokio::time::sleep(*delay).await;
            }
            _ => return Err(download_failure(label, release, error)),
        }
    }
}

/// One redrawn terminal line: `Downloading Raft Computer [#######·····]  42%`.
fn progress_line(label: &str, percent: u64) -> String {
    let percent = percent.min(100);
    let filled = (percent * PROGRESS_BAR_WIDTH / 100) as usize;
    let empty = PROGRESS_BAR_WIDTH as usize - filled;
    format!(
        "Downloading {label} [{}{}] {percent:>3}%",
        "#".repeat(filled),
        "-".repeat(empty)
    )
}

/// Ends an unfinished redrawn line when the download stops early (error or
/// abort), so the next message starts on its own line.
struct TerminalLine {
    open: Mutex<bool>,
}

impl Drop for TerminalLine {
    fn drop(&mut self) {
        if self.open.lock().map(|open| *open).unwrap_or(false) {
            eprintln!();
        }
    }
}

fn download_progress(label: &'static str) -> Progress {
    use std::io::{IsTerminal, Write};
    // A terminal gets one line redrawn in place; logs, CI and redirected
    // output keep one plain line per 10% so they stay readable.
    if std::io::stderr().is_terminal() {
        eprint!("\r{}", progress_line(label, 0));
        let _ = std::io::stderr().flush();
        let line = Arc::new(TerminalLine {
            open: Mutex::new(true),
        });
        let last_percent = Arc::new(Mutex::new(0_u64));
        return Arc::new(move |received, total| {
            if total == 0 {
                return;
            }
            let percent = received
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100);
            let Ok(mut last) = last_percent.lock() else {
                return;
            };
            if percent <= *last {
                return;
            }
            *last = percent;
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r{}", progress_line(label, percent));
            if percent == 100 {
                let _ = writeln!(stderr);
                if let Ok(mut open) = line.open.lock() {
                    *open = false;
                }
            }
            let _ = stderr.flush();
        });
    }
    eprintln!("Downloading {label}: 0%");
    let last_bucket = Arc::new(Mutex::new(0_u64));
    Arc::new(move |received, total| {
        if total == 0 {
            return;
        }
        let percent = received
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0)
            .min(100);
        let bucket = if percent == 100 {
            100
        } else {
            percent / 10 * 10
        };
        let Ok(mut last) = last_bucket.lock() else {
            return;
        };
        if bucket > *last {
            *last = bucket;
            eprintln!("Downloading {label}: {bucket}%");
        }
    })
}

fn u16le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u32be(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub fn check_platform(bytes: &[u8]) -> Result<()> {
    let (os, arch) = platform()?;
    let mismatch = || invalid("downloaded executable does not match this platform");
    if bytes.starts_with(b"\x7fELF") {
        let machine = if arch == "x64" { 0x3e } else { 0xb7 };
        if os == "linux"
            && bytes.len() >= 64
            && bytes[4] == 2
            && bytes[5] == 1
            && matches!(u16le(bytes, 16), Some(2 | 3))
            && u16le(bytes, 18) == Some(machine)
        {
            return Ok(());
        }
        return Err(mismatch());
    }
    if bytes.starts_with(b"MZ") {
        if os != "win32" {
            return Err(mismatch());
        }
        let pe = u32le(bytes, 0x3c).ok_or_else(mismatch)? as usize;
        let header = bytes.get(pe..).ok_or_else(mismatch)?;
        if header.starts_with(b"PE\0\0")
            && u16le(header, 4) == Some(0x8664)
            && u16le(header, 24) == Some(0x20b)
            && u16le(header, 22).is_some_and(|flags| flags & 2 != 0)
        {
            return Ok(());
        }
        return Err(mismatch());
    }
    let cpu = if arch == "x64" {
        0x01000007
    } else {
        0x0100000c
    };
    if u32le(bytes, 0) == Some(0xfeedfacf) {
        if os == "darwin"
            && bytes.len() >= 32
            && u32le(bytes, 4) == Some(cpu)
            && u32le(bytes, 12) == Some(2)
        {
            return Ok(());
        }
        return Err(mismatch());
    }
    // A universal executable must actually contain a usable slice for this
    // CPU. Merely recognizing the fat magic does not establish compatibility.
    if matches!(u32be(bytes, 0), Some(0xcafebabe | 0xcafebabf)) {
        if os != "darwin" {
            return Err(mismatch());
        }
        let wide = u32be(bytes, 0) == Some(0xcafebabf);
        let count = u32be(bytes, 4).ok_or_else(mismatch)? as usize;
        let stride = if wide { 32 } else { 20 };
        if count > 64 || bytes.len() < 8 + count * stride {
            return Err(mismatch());
        }
        for i in 0..count {
            let at = 8 + i * stride;
            if u32be(bytes, at) != Some(cpu) {
                continue;
            }
            let (offset, size) = if wide {
                let offset =
                    u64::from_be_bytes(bytes[at + 8..at + 16].try_into().map_err(|_| mismatch())?);
                let size =
                    u64::from_be_bytes(bytes[at + 16..at + 24].try_into().map_err(|_| mismatch())?);
                (
                    usize::try_from(offset).map_err(|_| mismatch())?,
                    usize::try_from(size).map_err(|_| mismatch())?,
                )
            } else {
                (
                    u32be(bytes, at + 8).ok_or_else(mismatch)? as usize,
                    u32be(bytes, at + 12).ok_or_else(mismatch)? as usize,
                )
            };
            let end = offset.checked_add(size).ok_or_else(mismatch)?;
            let slice = bytes.get(offset..end).ok_or_else(mismatch)?;
            if slice.len() >= 32
                && u32le(slice, 0) == Some(0xfeedfacf)
                && u32le(slice, 4) == Some(cpu)
                && u32le(slice, 12) == Some(2)
            {
                return Ok(());
            }
        }
    }
    Err(mismatch())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarIdentity {
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarRecord {
    pub format_version: u32,
    pub version: String,
    /// None is explicit absence, not a missing or unreadable record.
    pub sidecar: Option<SidecarIdentity>,
}

fn verify(bytes: &[u8], identity: &SidecarIdentity) -> Result<()> {
    if bytes.len() as u64 != identity.size || sha256(bytes) != identity.sha256 {
        return Err(invalid("cached sidecar identity mismatch"));
    }
    Ok(())
}

pub fn saved_sidecar(cfg: &Config, version: &str) -> Result<Option<PathBuf>> {
    let dir = cfg.sidecar_dir(version)?;
    let record: SidecarRecord = serde_json::from_slice(&fs::read(dir.join("identity.json"))?)?;
    if record.format_version != 1 || record.version != version {
        return Err(invalid("invalid sidecar record"));
    }
    match record.sidecar {
        Some(identity) => {
            let path = dir.join(SIDECAR_NAME);
            verify(&fs::read(&path)?, &identity)?;
            Ok(Some(path))
        }
        None => Ok(None),
    }
}

/// Observe without adopting or rewriting anything. A missing record is allowed
/// for installations made before the native installer; once recorded, explicit
/// absence and the exact saved bytes are part of the managed installation.
pub fn check_installed_sidecar(cfg: &Config, version: &str) -> Result<()> {
    let installed = match fs::symlink_metadata(&cfg.sidecar) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            Some(fs::read(&cfg.sidecar)?)
        }
        Ok(_) => return Err(invalid("installed sidecar is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let dir = cfg.sidecar_dir(version)?;
    if !k_carrier::storage::exists(&dir)? {
        return Ok(());
    }
    // An existing cache directory with no identity is interrupted/corrupt
    // native state, not evidence of a legacy installation without a sidecar.
    match (saved_sidecar(cfg, version)?, installed) {
        (None, None) => Ok(()),
        (Some(saved), Some(installed)) if fs::read(&saved)? == installed => Ok(()),
        _ => Err(invalid("installed sidecar differs from its saved identity")),
    }
}

/// Adopt the sidecar belonging to an existing, self-checked installation before
/// its first native transaction. Existing records retain their original identity.
/// Caller holds both product and K gates, and has proved installed == stable
/// for a pre-existing managed installation (or is bootstrapping trusted bytes).
pub fn adopt_installed_sidecar(cfg: &Config, version: &str) -> Result<()> {
    let dir = cfg.sidecar_dir(version)?;
    let record_path = dir.join("identity.json");
    if k_carrier::storage::exists(&record_path)? {
        check_installed_sidecar(cfg, version)?;
        return Ok(());
    }
    let bytes = match fs::symlink_metadata(&cfg.sidecar) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            Some(fs::read(&cfg.sidecar)?)
        }
        Ok(_) => return Err(invalid("installed sidecar is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    ensure_dir(&dir)?;
    let sidecar = match bytes {
        Some(bytes) => {
            let identity = SidecarIdentity {
                sha256: sha256(&bytes),
                size: bytes.len() as u64,
            };
            write_durable(&dir.join(SIDECAR_NAME), &bytes, false)?;
            Some(identity)
        }
        None => None,
    };
    write_json(
        &record_path,
        &SidecarRecord {
            format_version: 1,
            version: version.into(),
            sidecar,
        },
    )
}

pub async fn acquire_sidecar(cfg: &Config, manifest: &Manifest) -> Result<Option<PathBuf>> {
    let dir = cfg.sidecar_dir(&manifest.version)?;
    ensure_dir(&dir)?;
    let record = SidecarRecord {
        format_version: 1,
        version: manifest.version.clone(),
        sidecar: manifest.sidecar.as_ref().map(|r| SidecarIdentity {
            sha256: r.sha256.clone(),
            size: r.size,
        }),
    };
    let record_path = dir.join("identity.json");
    match fs::read(&record_path) {
        Ok(bytes) => {
            let old: SidecarRecord = serde_json::from_slice(&bytes)?;
            if old != record {
                return Err(invalid(
                    "release sidecar identity changed for an immutable version",
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let path = match (&manifest.sidecar, &record.sidecar) {
        (Some(release), Some(identity)) => {
            let path = dir.join(SIDECAR_NAME);
            if !fs::read(&path).is_ok_and(|bytes| verify(&bytes, identity).is_ok()) {
                let bytes = download(
                    &crate::source::downloader()?,
                    release,
                    &dir,
                    "Raft Computer support file",
                )
                .await?;
                write_durable(&path, &bytes, false)?;
            }
            Some(path)
        }
        (None, None) => None,
        _ => return Err(invalid("invalid sidecar identity")),
    };
    write_json(&record_path, &record)?;
    Ok(path)
}

pub struct PreparedArtifact {
    // Own the staging directory until bootstrap has consumed its bytes.
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    pub version: String,
    pub sidecar_quarantine: Option<PathBuf>,
}

pub async fn acquire(cfg: &Config, manifest: &Manifest) -> Result<PreparedArtifact> {
    version::exact(&manifest.version)?;
    let scratch = cfg.scratch();
    ensure_dir(&scratch)?;
    let sidecar = acquire_sidecar(cfg, manifest).await?;
    let bytes = download(
        &crate::source::downloader()?,
        &manifest.release,
        &scratch,
        "Raft Computer",
    )
    .await?;
    check_platform(&bytes)?;
    let directory = tempfile::Builder::new()
        .prefix("candidate-")
        .tempdir_in(&scratch)?;
    let path = directory.path().join(BIN_NAME);
    write_durable(&path, &bytes, true)?;
    if let Some(sidecar) = sidecar {
        write_durable(
            &directory.path().join(SIDECAR_NAME),
            &fs::read(sidecar)?,
            false,
        )?;
    }
    let evidence = computer::self_report(&path, cfg).await?;
    if evidence.version != manifest.version {
        return Err(invalid("candidate self-report version mismatch"));
    }
    Ok(PreparedArtifact {
        _directory: directory,
        path,
        version: manifest.version.clone(),
        sidecar_quarantine: None,
    })
}

/// Repair prepares a complete candidate independently of a damaged sidecar
/// cache. Only after download and self-check succeed may the old cache move
/// aside; product data and the installed binary remain untouched here.
pub async fn acquire_repair(cfg: &Config, manifest: &Manifest) -> Result<PreparedArtifact> {
    version::exact(&manifest.version)?;
    ensure_dir(&cfg.scratch())?;
    let downloader = crate::source::downloader()?;
    let bytes = download(
        &downloader,
        &manifest.release,
        &cfg.scratch(),
        "Raft Computer",
    )
    .await?;
    check_platform(&bytes)?;
    let directory = tempfile::Builder::new()
        .prefix("repair-candidate-")
        .tempdir_in(cfg.scratch())?;
    let path = directory.path().join(BIN_NAME);
    write_durable(&path, &bytes, true)?;
    let sidecar = match &manifest.sidecar {
        Some(release) => {
            let bytes = download(
                &downloader,
                release,
                &cfg.scratch(),
                "Raft Computer support file",
            )
            .await?;
            write_durable(&directory.path().join(SIDECAR_NAME), &bytes, false)?;
            Some(bytes)
        }
        None => None,
    };
    if computer::self_report(&path, cfg).await?.version != manifest.version {
        return Err(invalid("candidate self-report version mismatch"));
    }
    let record = SidecarRecord {
        format_version: 1,
        version: manifest.version.clone(),
        sidecar: manifest.sidecar.as_ref().map(|release| SidecarIdentity {
            sha256: release.sha256.clone(),
            size: release.size,
        }),
    };
    let cache = cfg.sidecar_dir(&manifest.version)?;
    if let Ok(bytes) = fs::read(cache.join("identity.json"))
        && let Ok(old) = serde_json::from_slice::<SidecarRecord>(&bytes)
        && old.format_version == 1
        && old.version == manifest.version
        && old != record
    {
        return Err(invalid(
            "release sidecar identity changed for an immutable version",
        ));
    }
    // Stage the replacement identity and bytes together before either rename.
    let replacement = directory.path().join("sidecar-cache");
    ensure_dir(&replacement)?;
    if let Some(bytes) = &sidecar {
        write_durable(&replacement.join(SIDECAR_NAME), bytes, false)?;
    }
    write_json(&replacement.join("identity.json"), &record)?;
    let sidecar_quarantine = if k_carrier::storage::exists(&cache)? {
        let meta = fs::symlink_metadata(&cache)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(invalid("sidecar cache is not an owned directory"));
        }
        let destination = cfg.installer_dir.join("quarantine").join(format!(
            "sidecar-{}-{}",
            manifest.version,
            uuid::Uuid::new_v4()
        ));
        ensure_dir(
            destination
                .parent()
                .ok_or_else(|| invalid("sidecar quarantine directory missing"))?,
        )?;
        k_carrier::storage::rename_durable(&cache, &destination)?;
        Some(destination)
    } else {
        None
    };
    ensure_dir(
        cache
            .parent()
            .ok_or_else(|| invalid("sidecar cache directory missing"))?,
    )?;
    k_carrier::storage::rename_durable(&replacement, &cache)?;
    Ok(PreparedArtifact {
        _directory: directory,
        path,
        version: manifest.version.clone(),
        sidecar_quarantine,
    })
}

/// K calls this on its staged incoming binary before it writes handover intent.
pub async fn check_candidate(cfg: &Config, path: &Path, release: &Release) -> Result<()> {
    let bytes = fs::read(path)?;
    check_platform(&bytes)?;
    // Keep product sidecars out of K's incoming/slot layout. Execute an exact
    // copy beside its verified sidecar in installer-owned temporary staging.
    let scratch = cfg.scratch();
    ensure_dir(&scratch)?;
    let directory = tempfile::Builder::new()
        .prefix("candidate-check-")
        .tempdir_in(scratch)?;
    let candidate = directory.path().join(BIN_NAME);
    write_durable(&candidate, &bytes, true)?;
    if let Some(sidecar) = saved_sidecar(cfg, &release.version)? {
        write_durable(
            &directory.path().join(SIDECAR_NAME),
            &fs::read(sidecar)?,
            false,
        )?;
    }
    if computer::self_report(&candidate, cfg).await?.version != release.version {
        return Err(invalid("candidate self-report version mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod progress_tests {
    use super::progress_line;

    #[test]
    fn progress_line_draws_a_fixed_width_bar() {
        assert_eq!(
            progress_line("Raft Computer", 0),
            "Downloading Raft Computer [------------------------------]   0%"
        );
        assert_eq!(
            progress_line("Raft Computer", 42),
            "Downloading Raft Computer [############------------------]  42%"
        );
        assert_eq!(
            progress_line("Raft Computer", 100),
            "Downloading Raft Computer [##############################] 100%"
        );
        assert_eq!(progress_line("x", 250), progress_line("x", 100));
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    /// What the test server does with one connection.
    #[derive(Clone, Copy)]
    enum Reply {
        /// Read the request and never answer.
        Stall,
        /// Answer 200 with the full length but send only the first `n` bytes, then close.
        Drop(usize),
        /// Answer with this status and no body.
        Status(u16),
        /// Answer the request in full, honouring a Range header.
        Serve,
    }

    struct Server {
        url: String,
        requests: mpsc::Receiver<String>,
    }

    impl Server {
        fn start(body: Vec<u8>, script: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/file", listener.local_addr().unwrap());
            let (sender, requests) = mpsc::channel();
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for reply in script {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let request = read_request(&mut stream);
                    let _ = sender.send(request.clone());
                    match reply {
                        Reply::Stall => held.push(stream),
                        Reply::Drop(n) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(&body[..n]);
                            let _ = stream.flush();
                        }
                        Reply::Status(status) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {status} Status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            );
                        }
                        Reply::Serve => serve(&mut stream, &request, &body),
                    }
                }
            });
            Self { url, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.try_iter().collect()
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
            request.push(byte[0]);
        }
        String::from_utf8_lossy(&request).to_ascii_lowercase()
    }

    fn serve(stream: &mut TcpStream, request: &str, body: &[u8]) {
        let offset = request
            .lines()
            .find_map(|line| line.strip_prefix("range: bytes="))
            .and_then(|range| range.trim().strip_suffix('-'))
            .and_then(|first| first.parse::<usize>().ok());
        let _ = match offset {
            Some(first) => write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {first}-{}/{}\r\nConnection: close\r\n\r\n",
                body.len() - first,
                body.len() - 1,
                body.len()
            )
            .and_then(|()| stream.write_all(&body[first..])),
            None => write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .and_then(|()| stream.write_all(body)),
        };
        let _ = stream.flush();
    }

    fn body() -> Vec<u8> {
        (0..64 * 1024).map(|i| (i % 251) as u8).collect()
    }

    fn release(url: &str, body: &[u8]) -> Release {
        Release {
            version: "1.0.0".into(),
            url: url.into(),
            sha256: sha256(body),
            size: body.len() as u64,
            gzip: None,
        }
    }

    /// A client that ignores any proxy in the test environment, and a short
    /// first-response allowance so a stall costs the test little time.
    fn downloader() -> Downloader {
        Downloader {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            policy: TransferPolicy {
                response_ms: 500,
                idle_ms: 500,
                ..TransferPolicy::default()
            },
        }
    }

    const NO_WAIT: [Duration; 2] = [Duration::from_millis(10), Duration::from_millis(10)];

    async fn fetch(server: &Server, body: &[u8], dir: &Path) -> Result<Vec<u8>> {
        download_with_retries(
            &downloader(),
            &release(&server.url, body),
            dir,
            "Raft Computer support file",
            &NO_WAIT,
        )
        .await
    }

    #[tokio::test]
    async fn a_dropped_connection_is_retried_from_the_partial_file() {
        let body = body();
        let server = Server::start(body.clone(), vec![Reply::Drop(20_000), Reply::Serve]);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fetch(&server, &body, dir.path()).await.unwrap(), body);
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(!requests[0].contains("range:"), "{}", requests[0]);
        assert!(
            requests[1].contains("range: bytes=20000-"),
            "the retry resumes: {}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn a_stalled_response_and_a_server_error_are_retried() {
        let body = body();
        let server = Server::start(
            body.clone(),
            vec![Reply::Stall, Reply::Status(503), Reply::Serve],
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fetch(&server, &body, dir.path()).await.unwrap(), body);
        assert_eq!(server.requests().len(), 3);
    }

    #[tokio::test]
    async fn three_transient_failures_fail_the_download() {
        let body = body();
        let server = Server::start(
            body.clone(),
            vec![
                Reply::Status(502),
                Reply::Status(503),
                Reply::Status(504),
                Reply::Serve,
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let error = fetch(&server, &body, dir.path()).await.unwrap_err();
        assert!(is_download_failure(&error), "{error}");
        assert!(error.to_string().contains("HTTP 504"), "{error}");
        assert_eq!(server.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_refusal_is_not_retried() {
        let body = body();
        let server = Server::start(body.clone(), vec![Reply::Status(404), Reply::Serve]);
        let dir = tempfile::tempdir().unwrap();
        let error = fetch(&server, &body, dir.path()).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 404"), "{error}");
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn bytes_that_do_not_match_the_release_are_not_retried() {
        let body = body();
        let mut other = body.clone();
        other[0] ^= 1;
        let server = Server::start(other, vec![Reply::Serve, Reply::Serve]);
        let dir = tempfile::tempdir().unwrap();
        let error = fetch(&server, &body, dir.path()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match the published release"),
            "{error}"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn transient_failures_are_told_from_final_ones() {
        for message in [
            "DOWNLOAD_FAILED: response timeout",
            "DOWNLOAD_FAILED: idle timeout",
            "DOWNLOAD_FAILED: overall timeout",
            "DOWNLOAD_FAILED: HTTP 500 Internal Server Error",
            "DOWNLOAD_FAILED: HTTP 503 Service Unavailable",
        ] {
            assert!(transient(&invalid(message)), "{message}");
        }
        for message in [
            "DOWNLOAD_FAILED: HTTP 404 Not Found",
            "DOWNLOAD_FAILED: HTTP 403 Forbidden",
            "DOWNLOAD_FAILED: mismatched Content-Range",
            "SHA256_MISMATCH",
            "SIZE_MISMATCH: response exceeds declared size",
            "MANIFEST_INVALID",
        ] {
            assert!(!transient(&invalid(message)), "{message}");
        }
        assert!(!transient(&crate::Error::Io(std::io::Error::other("disk"))));
    }

    #[tokio::test]
    async fn a_refused_connection_is_transient() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert!(transient(&crate::Error::Http(error)));
    }

    #[test]
    fn a_small_transfer_gets_at_least_two_minutes() {
        let policy = TransferPolicy::default();
        for size in [100_u64, 64 * 1024, 1024 * 1024, 5 * 1024 * 1024] {
            let budgets = with_minimum_attempt(policy, size).budgets(size).unwrap();
            assert!(
                budgets.overall_ms >= MINIMUM_ATTEMPT_MS,
                "{size}: {budgets:?}"
            );
            assert_eq!(budgets.response_ms, policy.response_ms);
            assert_eq!(budgets.idle_ms, policy.idle_ms);
        }
        // K's own budget for this size is already wider: unchanged.
        let large = 100 * 1024 * 1024;
        assert_eq!(
            with_minimum_attempt(policy, large)
                .budgets(large)
                .unwrap()
                .overall_ms,
            policy.budgets(large).unwrap().overall_ms
        );
    }
}
