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
    roots_fallback: Option<String>,
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

/// A URL as shown to the user: in full, userinfo, query and fragment
/// included. URLs are printed in full by product decision (artin,
/// 2026-10-08): the exact URL is what identifies the network filter or
/// mirror that answered. Only the length is bounded (a reply line holds 2048
/// bytes).
pub fn shown_url(url: &Url) -> String {
    let mut shown = String::from(url.as_str());
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

fn network_redirect(origin: &Url, requested: &Url, status: u16, target: &Url) -> crate::Error {
    let hands = origin.host_str().unwrap_or_default();
    invalid(format!(
        "{NETWORK_REDIRECT}the request for {} (HTTP {status}) to {}; a company firewall or proxy may be blocking {hands}. Ask your network administrator to allow {hands} and *.r2.cloudflarestorage.com, then run the same install command again.",
        shown_url(requested),
        shown_url(target),
    ))
}

pub(crate) const SYSTEM_ROOTS_UNUSABLE: &str =
    "system certificate store unreadable; using bundled roots";

/// The system's root certificates are trusted as well as the bundled ones
/// (Cargo feature rustls-tls-native-roots): a company proxy that re-signs
/// HTTPS installs its root there. Verification stays on, and release content
/// is still checked by sha256. Turning them off leaves the bundled roots.
pub(crate) fn trusting(builder: reqwest::ClientBuilder, native: bool) -> reqwest::ClientBuilder {
    crate::proxy::configure(builder).tls_built_in_native_certs(native)
}

/// Build with the system's roots first. A system store whose certificates
/// cannot be used fails the build; then build again with the bundled roots
/// only, as before system roots were trusted, and say why.
pub(crate) fn build_with_roots<E: std::fmt::Display>(
    build: impl Fn(bool) -> std::result::Result<Client, E>,
) -> std::result::Result<(Client, Option<String>), E> {
    match build(true) {
        Ok(client) => Ok((client, None)),
        Err(native) => {
            let client = build(false)?;
            Ok((client, Some(format!("{SYSTEM_ROOTS_UNUSABLE} ({native})"))))
        }
    }
}

/// An error and its causes on one line: reqwest's own message is only "builder error".
fn causes(error: &(dyn std::error::Error + 'static)) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(current) = cause {
        line.push_str(": ");
        line.push_str(&current.to_string());
        cause = current.source();
    }
    line
}

/// k-carrier's downloader, with the same roots as release requests.
pub fn downloader() -> Result<k_carrier::artifact::Downloader> {
    let (client, _) = build_with_roots(|native| {
        // As k_carrier::artifact::Downloader::new, apart from the roots.
        trusting(reqwest::Client::builder(), native)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|error| causes(&error))
    })
    .map_err(invalid)?;
    Ok(k_carrier::artifact::Downloader {
        client,
        policy: k_carrier::artifact::TransferPolicy {
            idle_ms: u64::try_from(crate::artifact::STALL_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            ..Default::default()
        },
        report: None,
    })
}

pub(crate) const CERTIFICATE_REJECTED: &str = "its HTTPS certificate is not trusted on this computer; a company proxy may be re-signing HTTPS";

/// TLS rejected the server's certificate. rustls is not a direct dependency,
/// so its error is recognised by message anywhere in the cause chain.
pub(crate) fn certificate_rejected(error: &reqwest::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if current.to_string().contains("invalid peer certificate") {
            return true;
        }
        cause = current.source();
    }
    false
}

/// Prefix of the error for a release lookup that never got an answer from
/// Hands; the operation names Hands as unreachable instead of blaming the
/// release (1.25.7 web upgrades: 4 of 4 "release could not be resolved" were
/// timeouts or failed connections to hands.build).
const UNREACHABLE: &str = "could not reach ";

/// Hands gave no answer at all: a timeout, a refused or failed connection,
/// a rejected certificate.
pub fn is_unreachable(error: &crate::Error) -> bool {
    matches!(error, crate::Error::Invalid(message) if message.starts_with(UNREACHABLE))
}

/// Pauses before the second and the third attempt of the release lookup.
/// The download already retries; the lookup before it was a single attempt,
/// so one dropped connection failed the whole upgrade.
const LOOKUP_RETRY_PAUSES: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];

/// Worth another attempt: no connection, or no answer in time. A rejected
/// certificate is the network's policy and answers the same way again.
fn transient(error: &reqwest::Error) -> bool {
    (error.is_timeout() || error.is_connect()) && !certificate_rejected(error)
}

/// Sends `GET url`, again after each of `pauses` while the failure is
/// transient. The error says how many attempts were made when more than one.
async fn send_with_retry(
    client: &Client,
    url: &Url,
    pauses: &[Duration],
) -> Result<reqwest::Response> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match client.get(url.clone()).send().await {
            Ok(response) => return Ok(response),
            Err(error) => match pauses.get(attempts - 1) {
                Some(pause) if transient(&error) => tokio::time::sleep(*pause).await,
                _ => {
                    let error = unreachable(url, &error);
                    return Err(if attempts > 1 {
                        invalid(format!("{error} ({attempts} attempts)"))
                    } else {
                        error
                    });
                }
            },
        }
    }
}

fn unreachable(url: &Url, error: &reqwest::Error) -> crate::Error {
    let cause = if error.is_timeout() {
        "the request timed out"
    } else if certificate_rejected(error) {
        CERTIFICATE_REJECTED
    } else if error.is_connect() {
        "the connection failed"
    } else if error.is_redirect() {
        "it redirected too many times"
    } else {
        "the request failed"
    };
    invalid(format!("{UNREACHABLE}{}: {cause}", shown_url(url)))
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
        // Proxies as in crate::proxy (environment, then the system proxy).
        // A redirect off the Hands host is never followed: release
        // selection only comes from Hands, and a network filter that
        // answers for Hands is reported by its exact redirect instead.
        let (client, roots_fallback) = build_with_roots(|native| {
            trusting(Client::builder(), native)
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::custom({
                    let hands_host = hands_host.clone();
                    move |attempt| {
                        if attempt.url().host_str() != hands_host.as_deref() {
                            attempt.stop()
                        } else if attempt.previous().len() > 5 {
                            attempt.error("too many redirects")
                        } else {
                            attempt.follow()
                        }
                    }
                }))
                .build()
                .map_err(|error| causes(&error))
        })
        .map_err(invalid)?;
        Ok(Self {
            client,
            hands_origin,
            hands_app: cfg.hands_app.clone(),
            roots_fallback,
        })
    }

    /// Why the system's root certificates are not in use, when they are not.
    pub fn roots_fallback(&self) -> Option<&str> {
        self.roots_fallback.as_deref()
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T> {
        // The deadline covers the complete body, and chunk accounting also
        // bounds responses that omit or lie about Content-Length.
        let mut response = send_with_retry(&self.client, &url, &LOOKUP_RETRY_PAUSES).await?;
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
                    network_redirect(&self.hands_origin, &final_url, status, &target)
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
            return Err(network_redirect(
                &self.hands_origin,
                &url,
                status,
                &final_url,
            ));
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
    fn shown_urls_are_printed_in_full() {
        for raw in [
            "https://acct.r2.cloudflarestorage.com/bucket/raft-computer?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=sig",
            "http://user:pw@114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?token=t&ori_url=aHR0cHM6Ly9oYW5kcy5idWlsZA==&uid=0#frag",
            "https://hands.build/dl/a/releases/x/linux-x64?kind=sha256sums",
        ] {
            assert_eq!(shown_url(&Url::parse(raw).unwrap()), raw);
        }
        let long = Url::parse(&format!("https://h/{}", "a".repeat(2000))).unwrap();
        assert_eq!(shown_url(&long).len(), SHOWN_URL_LIMIT + 3);
    }

    #[test]
    fn network_redirect_names_the_request_and_the_full_target() {
        let origin = Url::parse("https://hands.build").unwrap();
        let requested = Url::parse(
            "https://hands.build/public/v2/apps/raft-computer-cli/updates/check?platform=win32",
        )
        .unwrap();
        let target = Url::parse(
            "http://114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?ori_url=eA==&uid=0",
        )
        .unwrap();
        let error = network_redirect(&origin, &requested, 302, &target);
        assert!(is_network_redirect(&error));
        assert_eq!(
            error.to_string(),
            "The network redirected the request for https://hands.build/public/v2/apps/raft-computer-cli/updates/check?platform=win32 (HTTP 302) to http://114.114.114.114:9421/proxycontrolwarn/httpwarning_3318.html?ori_url=eA==&uid=0; a company firewall or proxy may be blocking hands.build. Ask your network administrator to allow hands.build and *.r2.cloudflarestorage.com, then run the same install command again."
        );
        assert!(!is_network_redirect(&invalid(
            "release source returned HTTP 404"
        )));
    }
}

#[cfg(test)]
mod roots_tests {
    use super::*;

    #[test]
    fn unusable_system_roots_fall_back_to_bundled_roots_and_say_why() {
        let attempts = std::cell::RefCell::new(Vec::new());
        let (_client, fallback) = build_with_roots(|native| {
            attempts.borrow_mut().push(native);
            if native {
                Err("zero valid certificates found in native root store")
            } else {
                Ok(trusting(Client::builder(), false).build().unwrap())
            }
        })
        .unwrap();
        assert_eq!(*attempts.borrow(), [true, false]);
        assert_eq!(
            fallback.as_deref(),
            Some(
                "system certificate store unreadable; using bundled roots (zero valid certificates found in native root store)"
            )
        );
    }

    #[test]
    fn usable_system_roots_are_used_without_a_second_build() {
        let attempts = std::cell::RefCell::new(Vec::new());
        let (_client, fallback) = build_with_roots(|native| {
            attempts.borrow_mut().push(native);
            trusting(Client::builder(), native).build()
        })
        .unwrap();
        assert_eq!(*attempts.borrow(), [true]);
        assert_eq!(fallback, None);
    }

    #[test]
    fn a_failing_bundled_build_still_fails() {
        let result = build_with_roots(|_| Err::<Client, _>("broken"));
        assert!(matches!(result, Err("broken")));
    }
}

#[cfg(test)]
mod lookup_retry_tests {
    use super::*;
    use std::io::{Read, Write};

    /// Bound but not listening: connections are refused, and the port stays
    /// ours, so a test running in parallel cannot start a server on it.
    fn refusing_socket() -> (socket2::Socket, Url) {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket
            .bind(
                &"127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            )
            .unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        let url = Url::parse(&format!("http://127.0.0.1:{port}/updates/check")).unwrap();
        (socket, url)
    }

    fn client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn a_refused_lookup_is_tried_three_times_and_names_hands_unreachable() {
        let (_socket, url) = refusing_socket();
        let pauses = [Duration::ZERO, Duration::ZERO];
        let error = send_with_retry(&client(), &url, &pauses).await.unwrap_err();
        // Linux refuses the connection; macOS lets it time out. Either is
        // "no answer", retried, and named the same way.
        let message = error.to_string();
        assert!(is_unreachable(&error), "{message}");
        assert!(
            message.starts_with(&format!("could not reach {url}: ")),
            "{message}"
        );
        assert!(message.ends_with(" (3 attempts)"), "{message}");
    }

    #[tokio::test]
    async fn a_single_attempt_does_not_count_attempts() {
        let (_socket, url) = refusing_socket();
        let error = send_with_retry(&client(), &url, &[]).await.unwrap_err();
        let message = error.to_string();
        assert!(is_unreachable(&error), "{message}");
        assert!(
            message.starts_with(&format!("could not reach {url}: ")),
            "{message}"
        );
        assert!(!message.contains("attempts"), "{message}");
    }

    #[tokio::test]
    async fn a_lookup_that_is_refused_once_succeeds_on_the_next_attempt() {
        let (socket, url) = refusing_socket();
        // The first attempt is refused; the server starts listening during the
        // pause, so the second attempt gets the answer.
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            socket.listen(1).unwrap();
            let listener: std::net::TcpListener = socket.into();
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let pauses = [Duration::from_millis(500), Duration::from_millis(500)];
        let response = send_with_retry(&client(), &url, &pauses).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        server.join().unwrap();
    }

    #[test]
    fn other_lookup_failures_are_not_called_unreachable() {
        assert!(!is_unreachable(&invalid(
            "release source returned HTTP 404"
        )));
        assert!(is_unreachable(&invalid(
            "could not reach https://hands.build/x: the request timed out"
        )));
    }
}
