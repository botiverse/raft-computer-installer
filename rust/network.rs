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

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
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
        let url = std::env::var("RCI_NETWORK_URL").unwrap();
        let result = match client(|builder| builder) {
            Err(_) => "build-error".to_owned(),
            Ok(client) => match client.get(&url).send().await {
                Ok(response) => format!("http-{}", response.status().as_u16()),
                Err(_) => "error".to_owned(),
            },
        };
        println!("\nRCI_RESULT={result}");
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
    fn without_the_root_the_server_is_not_trusted() {
        let url = format!("https://localhost:{}/", tls_server());
        assert_eq!(
            fetch_in_child(&url, &[("SSL_CERT_FILE", "/nonexistent/ca.pem")]),
            "error"
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
            "error"
        );
    }
}
