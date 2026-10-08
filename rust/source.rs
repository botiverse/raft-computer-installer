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

/// Every release-resolution error below is user-facing: the operation prints
/// it as the reason the release could not be resolved. A network redirect off
/// the Hands host starts with this text so the operation can give network
/// advice instead of a bare retry.
const NETWORK_REDIRECT: &str = "The network redirected ";
/// User-facing text bounds the URLs it names; a reply line holds 2048 bytes.
const SHOWN_URL_LIMIT: usize = 768;

pub fn is_network_redirect(error: &crate::Error) -> bool {
    matches!(error, crate::Error::Invalid(message) if message.starts_with(NETWORK_REDIRECT))
}

/// A URL as shown to the user: credentials are never printed, and a signed
/// object-storage URL (X-Amz-* query) is printed without its signature.
pub fn shown_url(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if url
        .query_pairs()
        .any(|(key, _)| key.to_ascii_lowercase().starts_with("x-amz-"))
    {
        url.set_query(None);
    }
    let mut shown = String::from(url);
    if shown.len() > SHOWN_URL_LIMIT {
        let mut end = SHOWN_URL_LIMIT;
        while !shown.is_char_boundary(end) {
            end -= 1;
        }
        shown.truncate(end);
        shown.push_str("...");
    }
    shown
}

fn network_redirect(origin: &Url, status: u16, target: &Url) -> crate::Error {
    let hands = origin.host_str().unwrap_or_default();
    invalid(format!(
        "{NETWORK_REDIRECT}{hands} to {} (HTTP {status} to {}); a company firewall or proxy may be blocking it. Ask your network administrator to allow {hands} and *.r2.cloudflarestorage.com, then run the same install command again.",
        target.host_str().unwrap_or_default(),
        shown_url(target),
    ))
}

fn unreachable(url: &Url, error: &reqwest::Error) -> crate::Error {
    let cause = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "the connection failed"
    } else if error.is_redirect() {
        "it redirected too many times"
    } else {
        "the request failed"
    };
    invalid(format!("could not reach {}: {cause}", shown_url(url)))
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
        let hands_origin = base_url(&cfg.hands_origin)?;
        let hands_host = hands_origin.host_str().map(str::to_owned);
        Ok(Self {
            // Keep reqwest's environment proxy support for every source request.
            // A redirect off the Hands host is never followed: release
            // selection only comes from Hands, and a network filter that
            // answers for Hands is reported by its exact redirect instead.
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.url().host_str() != hands_host.as_deref() {
                        attempt.stop()
                    } else if attempt.previous().len() > 5 {
                        attempt.error("too many redirects")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()?,
            hands_origin,
            hands_app: cfg.hands_app.clone(),
        })
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        // The deadline covers the complete body, and chunk accounting also
        // bounds responses that omit or lie about Content-Length.
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| unreachable(&url, &error))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        if response.status().is_redirection() {
            let target = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| final_url.join(value).ok());
            return Err(match target {
                Some(target) if target.host_str() != self.hands_origin.host_str() => {
                    network_redirect(&self.hands_origin, status, &target)
                }
                Some(target) => invalid(format!(
                    "{} returned HTTP {status} to {}",
                    shown_url(&final_url),
                    shown_url(&target)
                )),
                None => invalid(format!(
                    "{} returned HTTP {status} without a valid redirect target",
                    shown_url(&final_url)
                )),
            });
        }
        if final_url.host_str() != self.hands_origin.host_str() {
            return Err(network_redirect(&self.hands_origin, status, &final_url));
        }
        if !response.status().is_success() {
            return Err(invalid(format!(
                "{} returned HTTP {status}",
                shown_url(&final_url)
            )));
        }
        let html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("html"));
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            invalid(format!(
                "reading the release document from {} failed",
                shown_url(&final_url)
            ))
        })? {
            if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err(invalid(format!(
                    "{} returned a release document that is too large",
                    shown_url(&final_url)
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            let html = html || bytes.trim_ascii_start().starts_with(b"<");
            invalid(format!(
                "{} returned {} instead of the release document",
                shown_url(&final_url),
                if html {
                    "an HTML page"
                } else {
                    "unreadable data"
                }
            ))
        })
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
            .append_pair("current_version_code", "0")
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

#[cfg(test)]
mod shown_url_tests {
    use super::*;

    #[test]
    fn signed_object_storage_urls_are_shown_without_their_signature() {
        let signed = Url::parse("https://acct.r2.cloudflarestorage.com/bucket/raft-computer?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc").unwrap();
        assert_eq!(
            shown_url(&signed),
            "https://acct.r2.cloudflarestorage.com/bucket/raft-computer"
        );
        let warning = Url::parse(
            "http://114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?ori_url=aHR0cHM6Ly9oYW5kcy5idWlsZA==&uid=0",
        )
        .unwrap();
        assert_eq!(shown_url(&warning), warning.as_str());
        let credentials = Url::parse("https://user:secret@example.com/path").unwrap();
        assert_eq!(shown_url(&credentials), "https://example.com/path");
    }

    #[test]
    fn network_redirect_names_both_hosts_and_the_exact_target() {
        let origin = Url::parse("https://hands.build").unwrap();
        let target = Url::parse(
            "http://114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?ori_url=eA==&uid=0",
        )
        .unwrap();
        let error = network_redirect(&origin, 302, &target);
        assert!(is_network_redirect(&error));
        assert_eq!(
            error.to_string(),
            "The network redirected hands.build to 114.114.114.114 (HTTP 302 to http://114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?ori_url=eA==&uid=0); a company firewall or proxy may be blocking it. Ask your network administrator to allow hands.build and *.r2.cloudflarestorage.com, then run the same install command again."
        );
        assert!(!is_network_redirect(&invalid(
            "release source returned HTTP 404"
        )));
    }
}
