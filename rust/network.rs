//! Every HTTP client the installer uses: the release lookup and K's artifact
//! downloads share one TLS and proxy configuration.
use crate::Result;
use k_carrier::artifact::{Downloader, TransferPolicy};
use reqwest::{Client, ClientBuilder};

/// Build a client that trusts the operating system's certificate store as
/// well as the bundled Mozilla roots.
///
/// A company network that inspects HTTPS presents certificates signed by a
/// company root that IT installs in the system store, not in the bundled
/// roots. A missing or empty system store leaves the bundled roots alone;
/// reqwest refuses to build only when every system certificate is unusable,
/// and that store is then skipped so the bundled roots still apply.
///
/// Proxies come from HTTPS_PROXY/HTTP_PROXY/ALL_PROXY/NO_PROXY (either case)
/// and, when those are unset, from the Windows Internet Settings or macOS
/// system proxy (reqwest's `system-proxy` feature).
pub fn client(configure: impl Fn(ClientBuilder) -> ClientBuilder) -> Result<Client> {
    match configure(Client::builder()).build() {
        Ok(client) => Ok(client),
        Err(_) => Ok(configure(Client::builder())
            .tls_built_in_native_certs(false)
            .build()?),
    }
}

/// K's downloader on the shared client configuration.
pub fn downloader() -> Result<Downloader> {
    Ok(Downloader {
        client: client(|builder| builder.redirect(reqwest::redirect::Policy::limited(10)))?,
        policy: TransferPolicy::default(),
    })
}

/// Why a connection failed, as a fixed set. User-facing text never carries
/// library error text; each cause has its own fixed wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    CertificateUntrusted,
    TlsHandshake,
    ProxyUnreachable,
    ProxyRefused,
    Dns,
    Refused,
    Reset,
    TimedOut,
}

impl Cause {
    pub const ALL: [Cause; 8] = [
        Cause::CertificateUntrusted,
        Cause::TlsHandshake,
        Cause::ProxyUnreachable,
        Cause::ProxyRefused,
        Cause::Dns,
        Cause::Refused,
        Cause::Reset,
        Cause::TimedOut,
    ];

    /// Stable machine-readable code, recorded in the result detail.
    pub fn code(self) -> &'static str {
        match self {
            Cause::CertificateUntrusted => "certificate_untrusted",
            Cause::TlsHandshake => "tls_handshake_failed",
            Cause::ProxyUnreachable => "proxy_unreachable",
            Cause::ProxyRefused => "proxy_refused",
            Cause::Dns => "dns_failed",
            Cause::Refused => "connection_refused",
            Cause::Reset => "connection_reset",
            Cause::TimedOut => "timed_out",
        }
    }

    /// Fixed text every message for this cause contains (and no other
    /// cause's message does); it contains spaces, so no URL can carry it.
    fn marker(self) -> &'static str {
        match self {
            Cause::CertificateUntrusted => "the secure connection is not trusted",
            Cause::TlsHandshake => "the secure connection could not be set up",
            Cause::ProxyUnreachable => "the proxy could not be reached",
            Cause::ProxyRefused => "the proxy refused the connection",
            Cause::Dns => "could not be resolved (DNS lookup failed)",
            Cause::Refused => "the connection was refused",
            Cause::Reset => "the connection was closed or reset",
            Cause::TimedOut => "timed out",
        }
    }

    /// One-sentence reason for the result.
    pub fn reason(self) -> &'static str {
        match self {
            Cause::CertificateUntrusted => {
                "The secure connection was not trusted; a company network may be inspecting HTTPS traffic."
            }
            Cause::TlsHandshake => {
                "The secure connection could not be set up; a company network may be inspecting HTTPS traffic."
            }
            Cause::ProxyUnreachable => "The configured proxy could not be reached.",
            Cause::ProxyRefused => "The proxy refused the connection.",
            Cause::Dns => "The server name could not be resolved.",
            Cause::Refused => "The connection was refused.",
            Cause::Reset => "The connection was closed or reset.",
            Cause::TimedOut => "The connection timed out.",
        }
    }
}

/// The cause named in a message built by [`Failure::sentence`] (or a
/// timeout message), if any.
pub fn cause_in(message: &str) -> Option<Cause> {
    Cause::ALL
        .into_iter()
        .find(|cause| message.contains(cause.marker()))
}

/// A classified connection failure.
#[derive(Debug)]
pub struct Failure {
    pub cause: Cause,
    host: String,
    proxy: Option<String>,
}

impl Failure {
    /// Fixed wording for the cause, naming the host (and proxy) involved.
    pub fn sentence(&self) -> String {
        let host = &self.host;
        let proxy = self.proxy.as_deref().unwrap_or("system proxy");
        let marker = self.cause.marker();
        match self.cause {
            Cause::CertificateUntrusted => format!(
                "{marker} (the certificate for {host} is not signed by a root this computer trusts). A company network that inspects HTTPS traffic causes this: ask your network administrator to allow {host}, or to install the company root certificate in the system certificate store"
            ),
            Cause::TlsHandshake => format!(
                "{marker} (TLS handshake failed). A company network that inspects HTTPS traffic can cause this: ask your network administrator to allow {host}"
            ),
            Cause::ProxyUnreachable => format!(
                "{marker} ({proxy}); check the HTTPS_PROXY setting or the system proxy settings"
            ),
            Cause::ProxyRefused => format!(
                "{marker} ({proxy}); ask your network administrator to allow {host} through the proxy"
            ),
            Cause::Dns => format!("the name {host} {marker}; check the network connection"),
            Cause::Refused => format!("{marker} by {host}"),
            Cause::Reset => {
                format!("{marker}; a company firewall or proxy may be blocking {host}")
            }
            Cause::TimedOut => format!("the connection to {host} {marker}"),
        }
    }
}

/// The proxy reqwest uses for `url`, as `host:port` (never credentials).
/// reqwest's own proxy selection is hyper-util's `Matcher::from_system`
/// (environment first, then the Windows/macOS system proxy), so the same
/// matcher answers here.
fn proxy_for(url: &reqwest::Url) -> Option<String> {
    let uri = url.as_str().parse::<http::Uri>().ok()?;
    let intercept = hyper_util::client::proxy::matcher::Matcher::from_system().intercept(&uri)?;
    let proxy = intercept.uri();
    let host = proxy.host()?;
    Some(match proxy.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// What the error chain shows, read through `io::Error` wrappers (whose
/// `source()` skips the wrapped error itself, so the walk descends into it).
#[derive(Default)]
struct Evidence {
    tls: Option<bool>,
    dns: bool,
    kinds: Vec<std::io::ErrorKind>,
}

impl Evidence {
    fn read(error: &(dyn std::error::Error + 'static)) -> Self {
        let mut evidence = Self::default();
        let mut next = Some(error);
        while let Some(node) = next {
            evidence.note(node);
            next = match node.downcast_ref::<std::io::Error>() {
                Some(io) => {
                    evidence.kinds.push(io.kind());
                    // Wrapped errors nest (an io::Error inside an io::Error):
                    // descend into the wrapped error itself.
                    io.get_ref()
                        .map(|inner| inner as &(dyn std::error::Error + 'static))
                }
                None => node.source(),
            };
        }
        evidence
    }

    fn note(&mut self, node: &(dyn std::error::Error + 'static)) {
        if let Some(tls) = node.downcast_ref::<rustls::Error>() {
            self.tls
                .get_or_insert(matches!(tls, rustls::Error::InvalidCertificate(_)));
        }
        // hyper-util's resolver error type is private; its text is fixed.
        if node.to_string() == "dns error" {
            self.dns = true;
        }
    }

    fn has(&self, kinds: &[std::io::ErrorKind]) -> bool {
        self.kinds.iter().any(|kind| kinds.contains(kind))
    }
}

/// Classify a failed request. `None` when no listed cause applies; the
/// caller then keeps its generic wording.
pub fn classify(error: &reqwest::Error, url: &reqwest::Url) -> Option<Failure> {
    use std::io::ErrorKind::*;
    // The failing request may be a redirect target (an object-storage URL).
    let url = error.url().unwrap_or(url);
    let failure = |cause, proxy| Failure {
        cause,
        host: url.host_str().unwrap_or_default().to_owned(),
        proxy,
    };
    let evidence = Evidence::read(error);
    if error.is_timeout() || evidence.has(&[TimedOut]) {
        return Some(failure(Cause::TimedOut, None));
    }
    match evidence.tls {
        Some(true) => return Some(failure(Cause::CertificateUntrusted, None)),
        Some(false) => return Some(failure(Cause::TlsHandshake, None)),
        None => {}
    }
    let reset = [
        ConnectionReset,
        ConnectionAborted,
        UnexpectedEof,
        BrokenPipe,
    ];
    if !error.is_connect() {
        return evidence.has(&reset).then(|| failure(Cause::Reset, None));
    }
    if let Some(proxy) = proxy_for(url) {
        // Every connect failure below is between this computer and the proxy,
        // or the proxy's answer to CONNECT.
        let cause = if evidence.dns
            || evidence.has(&[ConnectionRefused, HostUnreachable, NetworkUnreachable])
        {
            Cause::ProxyUnreachable
        } else if evidence.has(&reset) {
            Cause::Reset
        } else {
            Cause::ProxyRefused
        };
        return Some(failure(cause, Some(proxy)));
    }
    let cause = if evidence.dns {
        Cause::Dns
    } else if evidence.has(&[ConnectionRefused]) {
        Cause::Refused
    } else if evidence.has(&reset) {
        Cause::Reset
    } else {
        return None;
    };
    Some(failure(cause, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
        time::Duration,
    };

    const PROXY_VARS: [&str; 8] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ];

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("rust/testdata")
            .join(name)
    }

    fn read_request(stream: &mut impl Read) {
        let mut seen = Vec::new();
        let mut byte = [0_u8; 1];
        while !seen.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => seen.push(byte[0]),
                _ => return,
            }
        }
    }

    /// An HTTPS server for `localhost` whose certificate is signed by the test
    /// CA in `rust/testdata/ca.pem`, which no shipped root store contains.
    fn tls_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(
                    std::fs::read(testdata("leaf.der")).unwrap(),
                )],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    std::fs::read(testdata("leaf.key.der")).unwrap(),
                )),
            )
            .unwrap(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let config = config.clone();
                std::thread::spawn(move || {
                    let connection = rustls::ServerConnection::new(config).unwrap();
                    let mut tls = rustls::StreamOwned::new(connection, stream);
                    read_request(&mut tls);
                    let _ = tls.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                    let _ = tls.flush();
                });
            }
        });
        port
    }

    /// Run one request in a fresh test process, so the environment it reads
    /// (SSL_CERT_FILE, proxy variables) is exactly the one given here.
    fn fetch_in_child(url: &str, env: &[(&str, &str)]) -> String {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "network::tests::fetch_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("RCI_NETWORK_URL", url)
            .env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR");
        for var in PROXY_VARS {
            command.env_remove(var);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.split_once("RCI_RESULT=").map(|(_, result)| result))
            .unwrap_or_else(|| {
                panic!(
                    "child printed no result:\n{stdout}\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
            .to_owned()
    }

    #[tokio::test]
    #[ignore = "run by fetch_in_child in its own process"]
    async fn fetch_child() {
        let url = reqwest::Url::parse(&std::env::var("RCI_NETWORK_URL").unwrap()).unwrap();
        let timeout =
            std::env::var("RCI_NETWORK_TIMEOUT_MS").map_or(10_000, |ms| ms.parse().unwrap());
        let no_such_host = std::env::var_os("RCI_NETWORK_NO_SUCH_HOST").is_some();
        let built = client(|builder| {
            let builder = builder.timeout(Duration::from_millis(timeout));
            if no_such_host {
                builder.dns_resolver(Arc::new(NoSuchHost))
            } else {
                builder
            }
        });
        let result = match built {
            Err(_) => "build-error".to_owned(),
            Ok(client) => match client.get(url.clone()).send().await {
                Ok(response) => format!("http-{}", response.status().as_u16()),
                Err(error) => match classify(&error, &url) {
                    Some(failure) => {
                        // The fixed wording carries its own marker, so the
                        // cause survives into the result reason.
                        assert_eq!(cause_in(&failure.sentence()), Some(failure.cause));
                        println!("\nRCI_SENTENCE={}", failure.sentence());
                        format!("error:{}", failure.cause.code())
                    }
                    None => "error:unclassified".to_owned(),
                },
            },
        };
        println!("\nRCI_RESULT={result}");
    }

    /// A resolver that answers every lookup with "not found" at once, the
    /// way the OS resolver does for an unknown name, without depending on
    /// how long a platform resolver takes to give up.
    struct NoSuchHost;

    impl reqwest::dns::Resolve for NoSuchHost {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no such host").into())
            })
        }
    }

    fn closed_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A server that runs `serve` on every accepted connection.
    fn server(serve: impl Fn(TcpStream) + Send + Sync + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve = Arc::new(serve);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let serve = serve.clone();
                std::thread::spawn(move || serve(stream));
            }
        });
        port
    }

    /// An HTTP proxy that answers CONNECT with `status`; on 200 it tunnels
    /// to 127.0.0.1:`upstream`, like a company proxy in front of the server.
    fn proxy(status: u16, upstream: u16) -> u16 {
        server(move |mut client| {
            read_request(&mut client);
            if status != 200 {
                let _ = client.write_all(
                    format!("HTTP/1.1 {status} Blocked\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                );
                return;
            }
            let Ok(upstream) = TcpStream::connect(("127.0.0.1", upstream)) else {
                return;
            };
            let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let (mut up_read, mut client_write) =
                (upstream.try_clone().unwrap(), client.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut up_read, &mut client_write);
            });
            let mut upstream = upstream;
            let _ = std::io::copy(&mut client, &mut upstream);
        })
    }

    fn untrusted() -> [(&'static str, &'static str); 1] {
        [("SSL_CERT_FILE", "/nonexistent/ca.pem")]
    }

    #[test]
    fn a_root_in_the_system_store_is_trusted() {
        let url = format!("https://localhost:{}/", tls_server());
        let ca = testdata("ca.pem");
        assert_eq!(
            fetch_in_child(&url, &[("SSL_CERT_FILE", ca.to_str().unwrap())]),
            "http-200"
        );
    }

    #[test]
    fn without_the_root_the_certificate_is_not_trusted() {
        let url = format!("https://localhost:{}/", tls_server());
        assert_eq!(
            fetch_in_child(&url, &untrusted()),
            "error:certificate_untrusted"
        );
    }

    #[test]
    fn an_unreadable_system_store_falls_back_to_the_bundled_roots() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(
            &garbage,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let url = format!("https://localhost:{}/", tls_server());
        assert_eq!(
            fetch_in_child(&url, &[("SSL_CERT_FILE", garbage.to_str().unwrap())]),
            "error:certificate_untrusted"
        );
    }

    #[test]
    fn an_inspecting_proxy_is_reported_as_an_untrusted_certificate() {
        // CONNECT succeeds; the certificate behind the tunnel is not trusted.
        let proxy = proxy(200, tls_server());
        let url = format!("https://localhost:{}/", closed_port());
        let proxy_url = format!("http://127.0.0.1:{proxy}");
        let mut env = untrusted().to_vec();
        env.push(("HTTPS_PROXY", &proxy_url));
        assert_eq!(fetch_in_child(&url, &env), "error:certificate_untrusted");
    }

    #[test]
    fn a_proxy_that_refuses_connect_is_named() {
        let proxy = proxy(403, closed_port());
        let proxy_url = format!("http://127.0.0.1:{proxy}");
        for var in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY"] {
            assert_eq!(
                fetch_in_child("https://localhost:9/", &[(var, &proxy_url)]),
                "error:proxy_refused",
                "{var}"
            );
        }
    }

    #[test]
    fn an_unreachable_proxy_is_named() {
        let proxy_url = format!("http://127.0.0.1:{}", closed_port());
        assert_eq!(
            fetch_in_child("https://localhost:9/", &[("HTTPS_PROXY", &proxy_url)]),
            "error:proxy_unreachable"
        );
    }

    #[test]
    fn no_proxy_bypasses_the_proxy() {
        let proxy_url = format!("http://127.0.0.1:{}", closed_port());
        let url = format!("https://localhost:{}/", closed_port());
        assert_eq!(
            fetch_in_child(
                &url,
                &[("HTTPS_PROXY", &proxy_url), ("NO_PROXY", "localhost")]
            ),
            "error:connection_refused"
        );
    }

    #[test]
    fn a_closed_port_is_refused() {
        let url = format!("https://127.0.0.1:{}/", closed_port());
        assert_eq!(fetch_in_child(&url, &[]), "error:connection_refused");
    }

    #[test]
    fn a_server_that_hangs_up_is_a_reset() {
        let port = server(drop);
        let url = format!("https://localhost:{port}/");
        assert_eq!(fetch_in_child(&url, &[]), "error:connection_reset");
    }

    #[test]
    fn a_name_that_does_not_resolve_is_a_dns_failure() {
        // The injected resolver's error travels reqwest's real resolver path
        // (hyper-util wraps it as its "dns error"); a real `.invalid` lookup
        // can outlast any test timeout on macOS and would then, correctly,
        // be reported as a timeout.
        assert_eq!(
            fetch_in_child(
                "https://raft-computer.invalid/",
                &[("RCI_NETWORK_NO_SUCH_HOST", "1")]
            ),
            "error:dns_failed"
        );
    }

    #[test]
    fn a_tls_server_that_rejects_the_handshake_is_a_handshake_failure() {
        // Garbage instead of a ServerHello: TLS fails without a certificate.
        // The whole ClientHello record is read first and the socket is
        // half-closed, not dropped: closing with unread input sends RST, and
        // Windows then reports the reset before the client reads the alert.
        let port = server(|mut stream| {
            let mut header = [0_u8; 5];
            if stream.read_exact(&mut header).is_err() {
                return;
            }
            let mut body = vec![0_u8; u16::from_be_bytes([header[3], header[4]]) as usize];
            let _ = stream.read_exact(&mut body);
            let _ = stream.write_all(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28]);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
        });
        let url = format!("https://localhost:{port}/");
        assert_eq!(fetch_in_child(&url, &[]), "error:tls_handshake_failed");
    }

    #[test]
    fn a_silent_server_times_out() {
        let port = server(|stream| {
            std::thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let url = format!("https://localhost:{port}/");
        assert_eq!(
            fetch_in_child(&url, &[("RCI_NETWORK_TIMEOUT_MS", "1000")]),
            "error:timed_out"
        );
    }

    #[test]
    fn every_cause_sentence_carries_only_its_own_marker() {
        for cause in Cause::ALL {
            let failure = Failure {
                cause,
                host: "hands.build".into(),
                proxy: Some("proxy.corp:8080".into()),
            };
            let sentence = failure.sentence();
            assert_eq!(cause_in(&sentence), Some(cause), "{sentence}");
            let others = Cause::ALL
                .into_iter()
                .filter(|other| *other != cause && sentence.contains(other.marker()))
                .collect::<Vec<_>>();
            assert!(others.is_empty(), "{sentence}: {others:?}");
        }
        assert_eq!(cause_in("https://hands.build/x returned HTTP 404"), None);
    }
}
