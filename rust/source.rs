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
use std::time::Duration;

#[derive(Deserialize)]
struct FileIdentity {
    download_url: String,
    sha256: String,
    size_bytes: u64,
}

impl FileIdentity {
    fn representation(
        &self,
        origin: &Url,
        expected_path: &str,
        expected_query: Option<&str>,
    ) -> Result<Representation> {
        let url =
            Url::parse(&self.download_url).map_err(|_| invalid("invalid Hands artifact URL"))?;
        if url.origin() != origin.origin()
            || url.path() != expected_path
            || url.query() != expected_query
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || self.size_bytes == 0
            || self.size_bytes > 9_007_199_254_740_991
        {
            return Err(invalid("invalid Hands release-bound artifact identity"));
        }
        Ok(Representation {
            url: url.into(),
            sha256: self.sha256.to_ascii_lowercase(),
            size: self.size_bytes,
        })
    }
}

#[derive(Deserialize)]
struct AuthorityRelease {
    id: String,
    version: String,
}

#[derive(Deserialize)]
struct AuthorityArtifact {
    #[serde(flatten)]
    raw: FileIdentity,
    platform: String,
    arch: String,
    gzip: Option<FileIdentity>,
    photon_wasm: FileIdentity,
}

#[derive(Deserialize)]
struct Authority {
    update_available: bool,
    release: AuthorityRelease,
    artifact: AuthorityArtifact,
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
        self.select(None, Some(requested)).await
    }

    /// Resolve exactly once. All representations must belong to the selected
    /// immutable Hands release; preparation never follows a mutable channel again.
    pub async fn resolve(&self, channel: &str) -> Result<Manifest> {
        let channel = parse_channel(channel)?;
        self.select(Some(&channel), None).await
    }

    async fn select(&self, channel: Option<&str>, requested: Option<&str>) -> Result<Manifest> {
        let (os, arch) = platform()?;
        let mut url = append(
            &self.hands_origin,
            &["public", "v2", "apps", &self.hands_app, "updates", "check"],
        )?;
        url.query_pairs_mut()
            .append_pair("product_type", "cli-binary")
            .append_pair("current_version", "0.0.0")
            .append_pair("platform", os)
            .append_pair("arch", arch);
        if let Some(channel) = channel {
            url.query_pairs_mut().append_pair("channel", channel);
        }
        if let Some(requested) = requested {
            url.query_pairs_mut().append_pair("version", requested);
        }
        let body: Authority = self.json(url).await?;
        version::exact(&body.release.version)?;
        if !body.update_available
            || requested.is_some_and(|v| v != body.release.version)
            || body.artifact.platform != os
            || body.artifact.arch != arch
            || body.release.id.is_empty()
            || !body
                .release
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(invalid("Hands release selection identity mismatch"));
        }
        let release_path = format!(
            "/dl/{}/releases/{}/{}",
            self.hands_app,
            body.release.id,
            platform_key()?
        );
        let raw = body
            .artifact
            .raw
            .representation(&self.hands_origin, &release_path, None)?;
        let gzip = body
            .artifact
            .gzip
            .as_ref()
            .map(|file| {
                file.representation(&self.hands_origin, &format!("{release_path}.gz"), None)
            })
            .transpose()?;
        let wasm = body.artifact.photon_wasm.representation(
            &self.hands_origin,
            &release_path,
            Some("kind=photon-wasm"),
        )?;
        let version = body.release.version;
        Ok(Manifest {
            version: version.clone(),
            release: Release {
                version: version.clone(),
                url: raw.url,
                sha256: raw.sha256,
                size: raw.size,
                gzip,
            },
            sidecar: Some(Release {
                version,
                url: wasm.url,
                sha256: wasm.sha256,
                size: wasm.size,
                gzip: None,
            }),
        })
    }
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
