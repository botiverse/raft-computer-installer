use crate::Result;
use k_carrier::error::invalid;
use std::cmp::Ordering;

/// Explicit CLI versions may use a leading v. Manifest identities are exact.
pub fn normalize(value: &str) -> Result<String> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    exact(value)?;
    Ok(value.into())
}

pub fn exact(value: &str) -> Result<semver::Version> {
    if value.len() > 256 {
        return Err(invalid("invalid release version"));
    }
    semver::Version::parse(value).map_err(|_| invalid("invalid release version"))
}

pub fn compare(a: &str, b: &str) -> Result<Ordering> {
    let mut a = exact(a)?;
    let mut b = exact(b)?;
    // Build metadata is identity, but never SemVer precedence.
    a.build = semver::BuildMetadata::EMPTY;
    b.build = semver::BuildMetadata::EMPTY;
    Ok(a.cmp(&b))
}
