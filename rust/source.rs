use crate::{
    Result,
    config::{Config, platform, platform_key},
    version,
};
use async_trait::async_trait;
use k_carrier::{
    artifact::{Release, ReleaseContext, ReleaseSource, Representation},
    error::invalid,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

#[derive(Clone, Debug, Deserialize)]
pub struct FileIdentity {
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

impl FileIdentity {
    fn validate(&mut self) -> Result<()> {
        // Release files are names beneath one immutable version directory.
        // Reject URLs, traversal, encoded separators, queries and fragments.
        if self.file.is_empty()
            || self.file.len() > 255
            || self.file == "."
            || self.file == ".."
            || !self
                .file
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || self.size == 0
            || self.size > 9_007_199_254_740_991
        {
            return Err(invalid("invalid release file identity"));
        }
        self.sha256.make_ascii_lowercase();
        Ok(())
    }
    fn representation(&self, base: &Url) -> Result<Representation> {
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|_| invalid("invalid release base"))?
            .push(&self.file);
        Ok(Representation {
            url: url.into(),
            sha256: self.sha256.clone(),
            size: self.size,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Target {
    #[serde(flatten)]
    raw: FileIdentity,
    gz: Option<FileIdentity>,
}

#[derive(Deserialize)]
struct WireManifest {
    version: String,
    targets: BTreeMap<String, Target>,
    #[serde(rename = "photonWasm")]
    sidecar: Option<FileIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub release: Release,
    pub sidecar: Option<Release>,
}

/// A request carries the already selected identities into the worker. Neither
/// a second channel resolution nor a changed manifest can alter its download.
pub struct FrozenSource(pub Manifest);

#[async_trait]
impl ReleaseSource for FrozenSource {
    async fn check(&self, _context: &ReleaseContext) -> Result<Option<Release>> {
        Ok(None)
    }
    async fn fetch(&self, requested: &str, context: &ReleaseContext) -> Result<Release> {
        if requested != self.0.version
            || self.0.release.version != requested
            || context.platform_key != platform_key()?
        {
            return Err(invalid("frozen release identity mismatch"));
        }
        self.0.release.validate()?;
        Ok(self.0.release.clone())
    }
}

#[derive(Clone)]
pub struct Source {
    client: Client,
    release_base: Url,
    hands_origin: Url,
    hands_app: String,
}

fn base_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| invalid("invalid release source URL"))?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("invalid release source URL"));
    }
    Ok(url)
}

fn append(base: &Url, parts: &[&str]) -> Result<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|_| invalid("invalid release source URL"))?
        .pop_if_empty()
        .extend(parts.iter().copied());
    Ok(url)
}

impl Source {
    pub fn new(cfg: &Config) -> Result<Self> {
        if cfg.hands_app.is_empty()
            || !cfg
                .hands_app
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            return Err(invalid("invalid release authority app"));
        }
        Ok(Self {
            // Keep reqwest's environment proxy support for every source request.
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()?,
            release_base: base_url(&cfg.release_base)?,
            hands_origin: base_url(&cfg.hands_origin)?,
            hands_app: cfg.hands_app.clone(),
        })
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        // The deadline covers the complete body, and chunk accounting also
        // bounds responses that omit or lie about Content-Length.
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| invalid("release source unreachable"))?;
        if !response.status().is_success() {
            return Err(invalid(format!(
                "release source returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| invalid("release source read failed"))?
        {
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err(invalid("release document too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid release document"))
    }

    pub async fn manifest(&self, requested: &str) -> Result<Manifest> {
        version::exact(requested)?;
        let base = append(&self.release_base, &[requested])?;
        let mut wire: WireManifest = self.json(append(&base, &["manifest.json"])?).await?;
        if wire.version != requested {
            return Err(invalid("release manifest version mismatch"));
        }
        let mut target = wire
            .targets
            .remove(&platform_key()?)
            .ok_or_else(|| invalid("release has no binary for this platform"))?;
        target.raw.validate()?;
        let raw = target.raw.representation(&base)?;
        let gzip = match target.gz.as_mut() {
            Some(gzip) => {
                gzip.validate()?;
                Some(gzip.representation(&base)?)
            }
            None => None,
        };
        let sidecar = match wire.sidecar.as_mut() {
            Some(sidecar) => {
                sidecar.validate()?;
                let file = sidecar.representation(&base)?;
                Some(Release {
                    version: requested.into(),
                    url: file.url,
                    sha256: file.sha256,
                    size: file.size,
                    gzip: None,
                })
            }
            None => None,
        };
        Ok(Manifest {
            version: requested.into(),
            release: Release {
                version: requested.into(),
                url: raw.url,
                sha256: raw.sha256,
                size: raw.size,
                gzip,
            },
            sidecar,
        })
    }

    /// Resolve a channel once, then freeze and cross-check the exact manifest.
    /// Callers pass the returned manifest to preparation; they do not resolve again.
    pub async fn resolve(&self, channel: &str) -> Result<Manifest> {
        let channel = parse_channel(channel)?;
        let mut url = append(
            &self.hands_origin,
            &["public", "v2", "apps", &self.hands_app, "latest"],
        )?;
        url.query_pairs_mut()
            .append_pair("channel", &channel)
            .append_pair("product_type", "cli-binary");
        let body: Authority = self.json(url).await?;
        version::exact(&body.build.version)?;
        let (os, arch) = platform()?;
        let assets = body
            .assets
            .iter()
            .filter(|a| {
                a.platform == os && a.arch == arch && a.filetype == "binary" && a.variant.is_null()
            })
            .collect::<Vec<_>>();
        if assets.len() != 1 {
            return Err(invalid(
                "release authority must name exactly one platform binary",
            ));
        }
        let asset = assets[0];
        let manifest = self.manifest(&body.build.version).await?;
        if asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || !asset.sha256.eq_ignore_ascii_case(&manifest.release.sha256)
            || asset.size_bytes != manifest.release.size
        {
            return Err(invalid(
                "release authority and CDN disagree about artifact identity",
            ));
        }
        Ok(manifest)
    }
}

#[derive(Deserialize)]
struct Build {
    version: String,
}
#[derive(Deserialize)]
struct Authority {
    build: Build,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    platform: String,
    arch: String,
    filetype: String,
    variant: serde_json::Value,
    sha256: String,
    size_bytes: u64,
}

pub fn parse_channel(value: &str) -> Result<String> {
    let value = value.trim();
    if ["main", "stable", "latest"].contains(&value) {
        return Ok("main".into());
    }
    if value == "alpha" {
        return Ok(value.into());
    }
    let bytes = value.as_bytes();
    if !(3..=64).contains(&bytes.len())
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        || [
            "rc",
            "release",
            "staging",
            "production",
            "prod",
            "nightly",
            "preview",
            "pinned",
            "default",
        ]
        .contains(&value)
    {
        return Err(invalid("expected main, alpha or a named feature channel"));
    }
    Ok(value.into())
}

#[async_trait]
impl ReleaseSource for Source {
    async fn check(&self, _context: &ReleaseContext) -> Result<Option<Release>> {
        Ok(None)
    }
    async fn fetch(&self, requested: &str, context: &ReleaseContext) -> Result<Release> {
        if context.platform_key != platform_key()? {
            return Err(invalid("runner platform mismatch"));
        }
        Ok(self.manifest(requested).await?.release)
    }
}
