//! Raft Computer packages downloaded ahead of an upgrade (#140). The Computer
//! service fetches them while the machine is idle and executes nothing; an
//! upgrade then finds its package here instead of downloading it.
//!
//! Layout, written by the Computer service:
//! `<installer>/prefetch/<version>/<sha256>`, where `<version>` is an exact
//! release version and `<sha256>` is the lowercase hex digest of the bytes
//! the release transfers (its gzip representation when it has one, else the
//! raw binary). The service writes temporary files inside the version
//! directory and renames them into place, so a file with its final name is
//! complete. The support file is prefetched as the bare file
//! `sidecars/<version>/<SIDECAR_NAME>`, never its `identity.json`:
//! `artifact::acquire_sidecar` reuses the file only when it matches the
//! manifest (otherwise it downloads it) and then writes the record itself, so
//! a bad prefetched file can't fail an upgrade. Cleanup keeps such a newer
//! support file only while `prefetch/<version>/` exists too.
//!
//! Nothing here is trusted: the upgrade checks the cached file against the
//! manifest it just fetched from Hands, and K checks it again (size and
//! sha256, then the decoded binary's) before it uses a byte of it. A file
//! that fails is deleted and the package is downloaded as before.
use crate::{Result, config::Config, source::Manifest, version};
use k_carrier::{
    artifact::{partial_path, sha256},
    storage::{ensure_dir, exists, remove_dir, sync_dir},
};
use std::{
    cmp::Ordering,
    fs, io,
    path::{Path, PathBuf},
};

/// Where an upgrade's package came from (`detail.candidateSource`).
pub const SOURCE_KEY: &str = "candidateSource";
pub const FROM_PREFETCH: &str = "prefetch";
pub const FROM_NETWORK: &str = "network";

pub fn root(cfg: &Config) -> PathBuf {
    cfg.installer_dir.join("prefetch")
}

/// The cached package for `manifest`, if its path is well formed. The
/// version is checked with the same strict parser as `upgrade --version`
/// before it becomes a path.
fn cached_path(cfg: &Config, manifest: &Manifest) -> Result<PathBuf> {
    version::exact(&manifest.version)?;
    let (_, sha, _) = transfer(manifest);
    if sha.len() != 64
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(k_carrier::error::invalid("invalid release digest"));
    }
    Ok(root(cfg).join(&manifest.version).join(sha))
}

/// The representation K transfers: URL, sha256 and size.
fn transfer(manifest: &Manifest) -> (&str, &str, u64) {
    let release = &manifest.release;
    match &release.gzip {
        Some(gzip) => (&gzip.url, &gzip.sha256, gzip.size),
        None => (&release.url, &release.sha256, release.size),
    }
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Links (or, where that fails, copies) the prefetched package for
/// `manifest` to the partial file K resumes
/// from (`<k>/incoming/<sha256(url)>.part`), so K finds it complete and
/// uses it without a request once its own digest check passes. Returns
/// whether it did. A missing file, or one whose type, size or digest does
/// not match the manifest, is not used (a mismatching one is deleted). The
/// digest check here is an early exit; K's is the one that counts. K's
/// incoming directory is written only under K's lock, which the caller
/// holds.
pub fn place(cfg: &Config, manifest: &Manifest) -> Result<bool> {
    let cached = cached_path(cfg, manifest)?;
    let (url, sha, size) = transfer(manifest);
    let meta = match fs::symlink_metadata(&cached) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if meta.is_dir() {
        remove_dir(&cached)?;
        return Ok(false);
    }
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != size {
        remove_file(&cached)?;
        return Ok(false);
    }
    if sha256(&fs::read(&cached)?) != sha {
        remove_file(&cached)?;
        return Ok(false);
    }
    let incoming = cfg.k_state.join("incoming");
    ensure_dir(&incoming)?;
    let part = partial_path(&incoming, url);
    // The cache stays for a retry until cleanup finds it installed. The
    // partial file is replaced in one rename, so a failure here leaves K's
    // own partial file as it was.
    let staging = incoming.join(".prefetch.tmp");
    remove_file(&staging)?;
    if fs::hard_link(&cached, &staging).is_err() {
        fs::copy(&cached, &staging)?;
        fs::File::open(&staging)?.sync_all()?;
    }
    fs::rename(&staging, &part)?;
    sync_dir(&incoming)?;
    Ok(true)
}

/// Deletes every prefetched version that is not newer than `stable` (the
/// installed one), and anything in the prefetch directory that is not a
/// version directory. Caller holds the installer gate, so no upgrade is
/// reading the cache; a package being prefetched for a newer version is
/// kept.
pub fn remove_stale(cfg: &Config, stable: &str) -> Result<()> {
    let root = root(cfg);
    if !exists(&root)? {
        return Ok(());
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let newer = entry
            .file_name()
            .to_str()
            .is_some_and(|name| matches!(version::compare(name, stable), Ok(Ordering::Greater)));
        let is_dir = entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink());
        if newer && is_dir {
            continue;
        }
        if is_dir {
            remove_dir(&entry.path())?;
        } else {
            remove_file(&entry.path())?;
        }
    }
    sync_dir(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k_carrier::artifact::{Release, Representation};

    fn cfg() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::load().unwrap();
        cfg.state_home = dir.path().to_path_buf();
        cfg.k_state = dir.path().join("computer/k");
        cfg.installer_dir = dir.path().join("computer/installer");
        (dir, cfg)
    }

    fn manifest(version: &str, gz: &[u8]) -> Manifest {
        Manifest {
            version: version.into(),
            release: Release {
                version: version.into(),
                url: "https://hands.example/raw".into(),
                sha256: "1".repeat(64),
                size: 10,
                gzip: Some(Representation {
                    url: "https://hands.example/raw.gz".into(),
                    sha256: sha256(gz),
                    size: gz.len() as u64,
                }),
            },
            sidecar: None,
        }
    }

    fn write_cached(cfg: &Config, manifest: &Manifest, bytes: &[u8]) -> PathBuf {
        let path = cached_path(cfg, manifest).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn part(cfg: &Config, manifest: &Manifest) -> PathBuf {
        partial_path(&cfg.k_state.join("incoming"), transfer(manifest).0)
    }

    #[test]
    fn a_matching_package_becomes_ks_complete_partial() {
        let (_dir, cfg) = cfg();
        let m = manifest("1.2.3", b"package-bytes");
        let cached = write_cached(&cfg, &m, b"package-bytes");
        fs::create_dir_all(cfg.k_state.join("incoming")).unwrap();
        fs::write(part(&cfg, &m), b"pack").unwrap();
        assert!(place(&cfg, &m).unwrap());
        assert_eq!(fs::read(part(&cfg, &m)).unwrap(), b"package-bytes");
        assert_eq!(
            fs::read(&cached).unwrap(),
            b"package-bytes",
            "the cache stays for a retry"
        );
        assert!(!cfg.k_state.join("incoming/.prefetch.tmp").exists());
    }

    #[test]
    fn a_missing_package_is_not_placed() {
        let (_dir, cfg) = cfg();
        let m = manifest("1.2.3", b"package-bytes");
        assert!(!place(&cfg, &m).unwrap());
        assert!(!part(&cfg, &m).exists());
    }

    #[test]
    fn a_package_of_the_wrong_size_or_digest_is_deleted_and_not_placed() {
        let (_dir, cfg) = cfg();
        let m = manifest("1.2.3", b"package-bytes");
        for bytes in [&b"package-bytez"[..], b"package-bytes-and-more", b""] {
            let cached = write_cached(&cfg, &m, bytes);
            assert!(!place(&cfg, &m).unwrap());
            assert!(!cached.exists());
            assert!(!part(&cfg, &m).exists());
        }
    }

    #[test]
    fn a_directory_in_the_packages_place_is_removed_and_not_placed() {
        let (_dir, cfg) = cfg();
        let m = manifest("1.2.3", b"package-bytes");
        let cached = cached_path(&cfg, &m).unwrap();
        fs::create_dir_all(cached.join("inner")).unwrap();
        assert!(!place(&cfg, &m).unwrap());
        assert!(!cached.exists());
        assert!(!part(&cfg, &m).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_removed_and_its_target_kept() {
        let (dir, cfg) = cfg();
        let m = manifest("1.2.3", b"package-bytes");
        let target = dir.path().join("elsewhere");
        fs::write(&target, b"package-bytes").unwrap();
        let cached = cached_path(&cfg, &m).unwrap();
        fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &cached).unwrap();
        assert!(!place(&cfg, &m).unwrap());
        assert!(fs::symlink_metadata(&cached).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"package-bytes");
        assert!(!part(&cfg, &m).exists());
    }

    #[test]
    fn a_version_that_is_not_strict_never_becomes_a_path() {
        let (_dir, cfg) = cfg();
        for version in ["../1.2.3", "1.2", "v1.2.3", "1.2.3/..", ""] {
            assert!(place(&cfg, &manifest(version, b"x")).is_err(), "{version}");
        }
    }

    #[test]
    fn stale_versions_and_strays_are_removed_newer_versions_kept() {
        let (_dir, cfg) = cfg();
        let root = root(&cfg);
        for version in ["1.2.2", "1.2.3", "1.2.4", "1.3.0-rc.1", "junk"] {
            fs::create_dir_all(root.join(version)).unwrap();
            fs::write(root.join(version).join("x"), b"x").unwrap();
        }
        fs::write(root.join("stray.tmp"), b"x").unwrap();
        remove_stale(&cfg, "1.2.3").unwrap();
        let mut left: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["1.2.4", "1.3.0-rc.1"]);
    }
}
