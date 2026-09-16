//! Preparation completes before any service is stopped or installed bytes
//! are replaced. Sidecar identity is retained for offline recovery.
use crate::{Result, computer, config::{BIN_NAME, Config, SIDECAR_NAME, platform}, source::Manifest, version};
use k_carrier::{artifact::{Downloader, Release, sha256}, error::invalid, storage::{ensure_dir, write_durable, write_json}};
use serde::{Deserialize, Serialize};
use std::{fs, path::{Path, PathBuf}};

fn u16le(bytes: &[u8], at: usize) -> Option<u16> { Some(u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?)) }
fn u32le(bytes: &[u8], at: usize) -> Option<u32> { Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?)) }
fn u32be(bytes: &[u8], at: usize) -> Option<u32> { Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?)) }

pub fn check_platform(bytes: &[u8]) -> Result<()> {
    let (os, arch) = platform()?;
    let mismatch = || invalid("downloaded executable does not match this platform");
    if bytes.starts_with(b"\x7fELF") {
        let machine = if arch == "x64" { 0x3e } else { 0xb7 };
        if os == "linux" && bytes.len() >= 64 && bytes[4] == 2 && bytes[5] == 1
            && matches!(u16le(bytes, 16), Some(2 | 3)) && u16le(bytes, 18) == Some(machine)
        { return Ok(()); }
        return Err(mismatch());
    }
    if bytes.starts_with(b"MZ") {
        if os != "win32" { return Err(mismatch()); }
        let pe = u32le(bytes, 0x3c).ok_or_else(mismatch)? as usize;
        let header = bytes.get(pe..).ok_or_else(mismatch)?;
        if header.starts_with(b"PE\0\0") && u16le(header, 4) == Some(0x8664)
            && u16le(header, 24) == Some(0x20b) && u16le(header, 22).is_some_and(|flags| flags & 2 != 0)
        { return Ok(()); }
        return Err(mismatch());
    }
    let cpu = if arch == "x64" { 0x01000007 } else { 0x0100000c };
    if u32le(bytes, 0) == Some(0xfeedfacf) {
        if os == "darwin" && bytes.len() >= 32 && u32le(bytes, 4) == Some(cpu) && u32le(bytes, 12) == Some(2) {
            return Ok(());
        }
        return Err(mismatch());
    }
    // A universal executable must actually contain a usable slice for this
    // CPU. Merely recognizing the fat magic does not establish compatibility.
    if matches!(u32be(bytes, 0), Some(0xcafebabe | 0xcafebabf)) {
        if os != "darwin" { return Err(mismatch()); }
        let wide = u32be(bytes, 0) == Some(0xcafebabf);
        let count = u32be(bytes, 4).ok_or_else(mismatch)? as usize;
        let stride = if wide { 32 } else { 20 };
        if count > 64 || bytes.len() < 8 + count * stride { return Err(mismatch()); }
        for i in 0..count {
            let at = 8 + i * stride;
            if u32be(bytes, at) != Some(cpu) { continue; }
            let (offset, size) = if wide {
                let offset = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().map_err(|_| mismatch())?);
                let size = u64::from_be_bytes(bytes[at + 16..at + 24].try_into().map_err(|_| mismatch())?);
                (usize::try_from(offset).map_err(|_| mismatch())?, usize::try_from(size).map_err(|_| mismatch())?)
            } else {
                (u32be(bytes, at + 8).ok_or_else(mismatch)? as usize, u32be(bytes, at + 12).ok_or_else(mismatch)? as usize)
            };
            let end = offset.checked_add(size).ok_or_else(mismatch)?;
            let slice = bytes.get(offset..end).ok_or_else(mismatch)?;
            if slice.len() >= 32 && u32le(slice, 0) == Some(0xfeedfacf)
                && u32le(slice, 4) == Some(cpu) && u32le(slice, 12) == Some(2)
            { return Ok(()); }
        }
    }
    Err(mismatch())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarIdentity { pub sha256: String, pub size: u64 }

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
    if record.format_version != 1 || record.version != version { return Err(invalid("invalid sidecar record")); }
    match record.sidecar {
        Some(identity) => {
            let path = dir.join(SIDECAR_NAME);
            verify(&fs::read(&path)?, &identity)?;
            Ok(Some(path))
        },
        None => Ok(None),
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
        saved_sidecar(cfg, version)?;
        return Ok(());
    }
    let bytes = match fs::symlink_metadata(&cfg.sidecar) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => Some(fs::read(&cfg.sidecar)?),
        Ok(_) => return Err(invalid("installed sidecar is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    ensure_dir(&dir)?;
    let sidecar = match bytes {
        Some(bytes) => {
            let identity = SidecarIdentity { sha256: sha256(&bytes), size: bytes.len() as u64 };
            write_durable(&dir.join(SIDECAR_NAME), &bytes, false)?;
            Some(identity)
        },
        None => None,
    };
    write_json(&record_path, &SidecarRecord { format_version: 1, version: version.into(), sidecar })
}

pub async fn acquire_sidecar(cfg: &Config, manifest: &Manifest) -> Result<Option<PathBuf>> {
    let dir = cfg.sidecar_dir(&manifest.version)?;
    ensure_dir(&dir)?;
    let record = SidecarRecord {
        format_version: 1, version: manifest.version.clone(),
        sidecar: manifest.sidecar.as_ref().map(|r| SidecarIdentity { sha256: r.sha256.clone(), size: r.size }),
    };
    let record_path = dir.join("identity.json");
    match fs::read(&record_path) {
        Ok(bytes) => {
            let old: SidecarRecord = serde_json::from_slice(&bytes)?;
            if old != record { return Err(invalid("release sidecar identity changed for an immutable version")); }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e.into()),
    }
    let path = match (&manifest.sidecar, &record.sidecar) {
        (Some(release), Some(identity)) => {
            let path = dir.join(SIDECAR_NAME);
            if !fs::read(&path).is_ok_and(|bytes| verify(&bytes, identity).is_ok()) {
                let bytes = Downloader::new()?.download(release, Some(&dir), None).await?;
                write_durable(&path, &bytes, false)?;
            }
            Some(path)
        },
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
}

pub async fn acquire(cfg: &Config, manifest: &Manifest) -> Result<PreparedArtifact> {
    version::exact(&manifest.version)?;
    let scratch = cfg.scratch();
    ensure_dir(&scratch)?;
    let sidecar = acquire_sidecar(cfg, manifest).await?;
    let bytes = Downloader::new()?.download(&manifest.release, Some(&scratch), None).await?;
    check_platform(&bytes)?;
    let directory = tempfile::Builder::new().prefix("candidate-").tempdir_in(&scratch)?;
    let path = directory.path().join(BIN_NAME);
    write_durable(&path, &bytes, true)?;
    if let Some(sidecar) = sidecar {
        write_durable(&directory.path().join(SIDECAR_NAME), &fs::read(sidecar)?, false)?;
    }
    let evidence = computer::self_report(&path, cfg).await?;
    if evidence.version != manifest.version { return Err(invalid("candidate self-report version mismatch")); }
    Ok(PreparedArtifact { _directory: directory, path, version: manifest.version.clone() })
}

/// K calls this on its staged incoming binary before it writes handover intent.
pub async fn check_candidate(cfg: &Config, path: &Path, release: &Release) -> Result<()> {
    let bytes = fs::read(path)?;
    check_platform(&bytes)?;
    // Keep product sidecars out of K's incoming/slot layout. Execute an exact
    // copy beside its verified sidecar in installer-owned temporary staging.
    let scratch = cfg.scratch();
    ensure_dir(&scratch)?;
    let directory = tempfile::Builder::new().prefix("candidate-check-").tempdir_in(scratch)?;
    let candidate = directory.path().join(BIN_NAME);
    write_durable(&candidate, &bytes, true)?;
    if let Some(sidecar) = saved_sidecar(cfg, &release.version)? {
        write_durable(&directory.path().join(SIDECAR_NAME), &fs::read(sidecar)?, false)?;
    }
    if computer::self_report(&candidate, cfg).await?.version != release.version {
        return Err(invalid("candidate self-report version mismatch"));
    }
    Ok(())
}
