//! TLS and mutual TLS (mTLS) server configuration, client-cert inspection, and HTTPS listener adapter.

use crate::logging::{LogEvent, Severity};
use anyhow::{anyhow, bail, Context, Result};
use axum::serve::Listener;
use sha2::{Digest, Sha256};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{RootCertStore, ServerConfig, ServerConnection};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_QUEUE_CAPACITY: usize = 256;

/// Client certificate verification policy for TLS listeners (IRC TLS and HTTPS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientAuthMode {
    /// Do not request or verify client certificates (standard server-auth TLS).
    #[default]
    None,
    /// Request a client certificate; allow connections without one, verify if presented.
    Optional,
    /// Require every client to present a valid certificate signed by `tls_client_ca`.
    Required,
}

impl ClientAuthMode {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "disabled" => Ok(Self::None),
            "optional" | "want" => Ok(Self::Optional),
            "required" | "require" => Ok(Self::Required),
            other => bail!(
                "tls_client_auth must be one of 'none', 'optional', or 'required', got '{other}'"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

/// Complete server-side TLS + optional mTLS specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsServerConfigSpec {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub client_ca_path: Option<PathBuf>,
    pub client_auth: ClientAuthMode,
}

/// Metadata extracted from a verified peer client certificate chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCertInfo {
    pub sha256: String,
    pub chain_len: usize,
}

fn load_certs(path: &Path, label: &str) -> Result<Vec<CertificateDer<'static>>> {
    let data =
        std::fs::read(path).with_context(|| format!("read {label}: {}", path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut data.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parse {label} certificates from {}", path.display()))?;
    if certs.is_empty() {
        bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path)
        .with_context(|| format!("read TLS key: {}", path.display()))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .with_context(|| format!("parse TLS private key from {}", path.display()))?
        .ok_or_else(|| anyhow!("no private key found in {}", path.display()))
}

/// Ensure a process-wide rustls CryptoProvider is installed before constructing
/// any `ServerConfig` or `ClientConfig`.
pub(crate) fn ensure_crypto_provider() {
    if tokio_rustls::rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Build a `tokio_rustls::TlsAcceptor` configured for server TLS and optional mTLS.
pub fn build_tls_acceptor(spec: &TlsServerConfigSpec) -> Result<TlsAcceptor> {
    ensure_crypto_provider();
    let certs = load_certs(&spec.cert_path, "TLS cert")?;
    let key = load_private_key(&spec.key_path)?;

    let builder = ServerConfig::builder();
    let config = match spec.client_auth {
        ClientAuthMode::None => builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("build TLS server config")?,
        ClientAuthMode::Optional | ClientAuthMode::Required => {
            let ca_path = spec.client_ca_path.as_deref().ok_or_else(|| {
                anyhow!("tls_client_ca must be configured when tls_client_auth is enabled")
            })?;
            let ca_certs = load_certs(ca_path, "TLS client CA")?;
            let mut roots = RootCertStore::empty();
            for cert in ca_certs {
                roots
                    .add(cert)
                    .with_context(|| format!("add CA certificate from {}", ca_path.display()))?;
            }
            let mut verifier_builder = WebPkiClientVerifier::builder(Arc::new(roots));
            if spec.client_auth == ClientAuthMode::Optional {
                verifier_builder = verifier_builder.allow_unauthenticated();
            }
            let verifier = verifier_builder
                .build()
                .context("build TLS client certificate verifier")?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .context("build mTLS server config")?
        }
    };

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Inspect the negotiated rustls session for a peer client certificate chain.
pub fn extract_client_cert_info(conn: &ServerConnection) -> Option<ClientCertInfo> {
    let certs = conn.peer_certificates()?;
    let leaf = certs.first()?;
    let digest = Sha256::digest(leaf.as_ref());
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(&mut sha256, "{byte:02x}");
    }
    Some(ClientCertInfo {
        sha256,
        chain_len: certs.len(),
    })
}

/// Axum-compatible TLS/mTLS listener that performs TLS handshakes concurrently.
pub struct TlsListener {
    local_addr: SocketAddr,
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    accept_task: JoinHandle<()>,
}

impl TlsListener {
    pub fn from_tcp(
        listener: TcpListener,
        acceptor: TlsAcceptor,
        logger: crate::logging::EventForwarder,
    ) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        let (tx, rx) = mpsc::channel(ACCEPT_QUEUE_CAPACITY);
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, peer_addr) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(err) => {
                        logger.log(
                            LogEvent::server_error(
                                "http.accept_error",
                                format!("HTTPS accept failed: {err}"),
                            )
                            .with_target(local_addr.to_string()),
                        );
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let logger = logger.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;
                    let tls_stream = match accepted {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(error)) => {
                            logger.log(
                                LogEvent::server_error(
                                    "http.tls_handshake_failed",
                                    format!("HTTPS TLS handshake failed from {peer_addr}: {error:#}"),
                                )
                                .with_target(peer_addr.to_string()),
                            );
                            return;
                        }
                        Err(_) => {
                            logger.log(
                                LogEvent::server_error(
                                    "http.tls_handshake_failed",
                                    format!("HTTPS TLS handshake timed out from {peer_addr}"),
                                )
                                .with_target(peer_addr.to_string()),
                            );
                            return;
                        }
                    };
                    if let Some(cert_info) = extract_client_cert_info(tls_stream.get_ref().1) {
                        logger.log(
                            LogEvent::security_audit(
                                Severity::Info,
                                "http.tls_client_verified",
                                "success",
                                format!(
                                    "HTTPS client certificate verified from {peer_addr} (sha256={}, chain={})",
                                    cert_info.sha256, cert_info.chain_len
                                ),
                            )
                            .with_actor("tls")
                            .with_target(peer_addr.to_string()),
                        );
                    }
                    let _ = tx.send((tls_stream, peer_addr)).await;
                });
            }
        });
        Ok(Self {
            local_addr,
            rx,
            accept_task,
        })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::http::{build_runtime_info, serve_tls};
    use crate::logging::{EventForwarder, LogConfig};
    use crate::server::Server;
    use crate::store;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::ClientConfig;
    use tokio_rustls::TlsConnector;

    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn find_openssl() -> PathBuf {
        for candidate in [
            "openssl",
            r"C:\Program Files\Git\usr\bin\openssl.exe",
            r"C:\Program Files\Git\mingw64\bin\openssl.exe",
        ] {
            if std::process::Command::new(candidate)
                .arg("version")
                .output()
                .is_ok_and(|out| out.status.success())
            {
                return PathBuf::from(candidate);
            }
        }
        panic!("openssl executable not found in PATH or Git installation");
    }

    fn run_openssl(openssl: &Path, args: &[&str]) {
        let out = std::process::Command::new(openssl)
            .args(args)
            .output()
            .expect("invoke openssl");
        assert!(
            out.status.success(),
            "openssl {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub(crate) struct TestPki {
        _dir: TempDir,
        pub(crate) ca_cert: PathBuf,
        pub(crate) server_cert: PathBuf,
        pub(crate) server_key: PathBuf,
        pub(crate) client_cert: PathBuf,
        pub(crate) client_key: PathBuf,
        pub(crate) untrusted_cert: PathBuf,
        pub(crate) untrusted_key: PathBuf,
    }

    impl TestPki {
        pub(crate) fn generate() -> Self {
            let dir = TempDir::new("aircd-mtls-test");
            let openssl = find_openssl();

            let ca_key = dir.0.join("ca.key");
            let ca_cert = dir.0.join("ca.crt");
            let server_key = dir.0.join("server.key");
            let server_csr = dir.0.join("server.csr");
            let server_cert = dir.0.join("server.crt");
            let server_ext = dir.0.join("server.ext");
            let client_key = dir.0.join("client.key");
            let client_csr = dir.0.join("client.csr");
            let client_cert = dir.0.join("client.crt");
            let client_ext = dir.0.join("client.ext");
            let bad_ca_key = dir.0.join("bad-ca.key");
            let bad_ca_cert = dir.0.join("bad-ca.crt");
            let untrusted_key = dir.0.join("untrusted.key");
            let untrusted_csr = dir.0.join("untrusted.csr");
            let untrusted_cert = dir.0.join("untrusted.crt");

            std::fs::write(
                &server_ext,
                "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n",
            )
            .unwrap();
            std::fs::write(
                &client_ext,
                "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=clientAuth\n",
            )
            .unwrap();

            fn s(p: &Path) -> &str {
                p.to_str().unwrap()
            }

            run_openssl(
                &openssl,
                &[
                    "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-keyout", s(&ca_key), "-out", s(&ca_cert), "-days", "7", "-nodes",
                    "-subj", "/CN=Aircd-Test-CA",
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-keyout", s(&server_key), "-out", s(&server_csr), "-nodes",
                    "-subj", "/CN=localhost",
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "x509", "-req", "-in", s(&server_csr), "-CA", s(&ca_cert), "-CAkey", s(&ca_key),
                    "-CAcreateserial", "-out", s(&server_cert), "-days", "7", "-extfile", s(&server_ext),
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-keyout", s(&client_key), "-out", s(&client_csr), "-nodes",
                    "-subj", "/CN=worker-mtls",
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "x509", "-req", "-in", s(&client_csr), "-CA", s(&ca_cert), "-CAkey", s(&ca_key),
                    "-CAcreateserial", "-out", s(&client_cert), "-days", "7", "-extfile", s(&client_ext),
                ],
            );

            run_openssl(
                &openssl,
                &[
                    "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-keyout", s(&bad_ca_key), "-out", s(&bad_ca_cert), "-days", "7", "-nodes",
                    "-subj", "/CN=Untrusted-CA",
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-keyout", s(&untrusted_key), "-out", s(&untrusted_csr), "-nodes",
                    "-subj", "/CN=rogue-worker",
                ],
            );
            run_openssl(
                &openssl,
                &[
                    "x509", "-req", "-in", s(&untrusted_csr), "-CA", s(&bad_ca_cert),
                    "-CAkey", s(&bad_ca_key), "-CAcreateserial", "-out", s(&untrusted_cert),
                    "-days", "7", "-extfile", s(&client_ext),
                ],
            );

            Self {
                _dir: dir,
                ca_cert,
                server_cert,
                server_key,
                client_cert,
                client_key,
                untrusted_cert,
                untrusted_key,
            }
        }
    }

    fn build_tls_client_connector(
        ca_path: &Path,
        client_cert: Option<(&Path, &Path)>,
    ) -> TlsConnector {
        ensure_crypto_provider();
        let ca_certs = load_certs(ca_path, "test CA").unwrap();
        let mut roots = RootCertStore::empty();
        for cert in ca_certs {
            roots.add(cert).unwrap();
        }
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let cfg = match client_cert {
            Some((cert_path, key_path)) => {
                let certs = load_certs(cert_path, "client cert").unwrap();
                let key = load_private_key(key_path).unwrap();
                builder.with_client_auth_cert(certs, key).unwrap()
            }
            None => builder.with_no_client_auth(),
        };
        TlsConnector::from(Arc::new(cfg))
    }

    async fn assert_tls_rejected(
        connector: &TlsConnector,
        server_name: ServerName<'static>,
        port: u16,
        label: &str,
    ) {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        match connector.connect(server_name, tcp).await {
            Err(_) => {}
            Ok(mut stream) => {
                // In TLS 1.3 the client finishes connect() before receiving the server's
                // Certificate/Finished verification alert; reading must fail or EOF immediately.
                let mut buf = [0u8; 64];
                match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
                    Ok(Err(_)) | Ok(Ok(0)) => {}
                    other => panic!("expected {label} to be rejected, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn client_auth_mode_parses_valid_and_rejects_invalid() {
        assert_eq!(ClientAuthMode::parse("none").unwrap(), ClientAuthMode::None);
        assert_eq!(ClientAuthMode::parse("OPTIONAL").unwrap(), ClientAuthMode::Optional);
        assert_eq!(ClientAuthMode::parse(" required ").unwrap(), ClientAuthMode::Required);
        assert!(ClientAuthMode::parse("strict").is_err());
    }

    #[test]
    fn build_tls_acceptor_rejects_missing_or_empty_files() {
        let dir = TempDir::new("aircd-tls-bad");
        let empty = dir.0.join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        let missing = dir.0.join("missing.pem");
        assert!(build_tls_acceptor(&TlsServerConfigSpec {
            cert_path: missing.clone(),
            key_path: missing.clone(),
            client_ca_path: None,
            client_auth: ClientAuthMode::None,
        })
        .is_err());
        assert!(build_tls_acceptor(&TlsServerConfigSpec {
            cert_path: empty.clone(),
            key_path: empty,
            client_ca_path: None,
            client_auth: ClientAuthMode::None,
        })
        .is_err());
    }

    #[tokio::test]
    async fn mtls_irc_enforces_client_certificates_and_allows_trusted_clients() -> Result<()> {
        let pki = TestPki::generate();
        let db = store::open(":memory:")?;
        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let server = Server::with_logger(db, logger);
        let (_, token) = server.auth().register_principal("mtls-irc", Some("worker"), None)?;

        let acceptor = build_tls_acceptor(&TlsServerConfigSpec {
            cert_path: pki.server_cert.clone(),
            key_path: pki.server_key.clone(),
            client_ca_path: Some(pki.ca_cert.clone()),
            client_auth: ClientAuthMode::Required,
        })?;

        // Reserve a local port, then drop the probe listener and start Server::listen_tls.
        let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = probe.local_addr()?.port();
        drop(probe);
        let bind_addr = format!("127.0.0.1:{port}");
        let srv = server.clone();
        let task = tokio::spawn(async move { srv.listen_tls(&bind_addr, acceptor).await });
        tokio::time::sleep(Duration::from_millis(80)).await;

        let server_name = ServerName::try_from("localhost").unwrap();

        // 1. No client cert -> rejected.
        let no_cert = build_tls_client_connector(&pki.ca_cert, None);
        assert_tls_rejected(&no_cert, server_name.clone(), port, "missing client cert").await;

        // 2. Untrusted client cert -> rejected.
        let untrusted = build_tls_client_connector(
            &pki.ca_cert,
            Some((&pki.untrusted_cert, &pki.untrusted_key)),
        );
        assert_tls_rejected(&untrusted, server_name.clone(), port, "untrusted client cert").await;

        // 3. Trusted client cert -> IRC login succeeds.
        let trusted = build_tls_client_connector(
            &pki.ca_cert,
            Some((&pki.client_cert, &pki.client_key)),
        );
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        let mut tls_stream = trusted.connect(server_name, tcp).await?;
        tls_stream
            .write_all(format!("PASS {token}\r\nNICK mtls-irc\r\nUSER mtls-irc 0 * :mtls\r\n").as_bytes())
            .await?;
        let mut lines = BufReader::new(tls_stream).lines();
        let mut welcomed = false;
        while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await? {
            if line.contains(" 001 ") {
                welcomed = true;
                break;
            }
        }
        assert!(welcomed, "expected IRC 001 welcome over mTLS");

        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn mtls_https_serve_tls_enforces_client_certs_and_serves_api() -> Result<()> {
        let pki = TestPki::generate();
        let db = store::open(":memory:")?;
        let logger = EventForwarder::new(LogConfig {
            stderr_errors: false,
            ..LogConfig::default()
        })?;
        let server = Server::with_logger(db, logger);
        let (_, token) = server.auth().register_principal("mtls-http", Some("worker"), None)?;

        let acceptor = build_tls_acceptor(&TlsServerConfigSpec {
            cert_path: pki.server_cert.clone(),
            key_path: pki.server_key.clone(),
            client_ca_path: Some(pki.ca_cert.clone()),
            client_auth: ClientAuthMode::Required,
        })?;

        let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = probe.local_addr()?.port();
        drop(probe);
        let bind_addr = format!("127.0.0.1:{port}");

        let mut info = build_runtime_info("127.0.0.1:6667", "127.0.0.1:6680", None);
        info.http_tls_port = Some(port);
        info.tls_client_auth = "required".to_string();
        info.tls_only = true;

        let task = tokio::spawn(async move { serve_tls(server, &bind_addr, info, acceptor).await });
        tokio::time::sleep(Duration::from_millis(80)).await;

        let ca_cert = reqwest::Certificate::from_pem(&std::fs::read(&pki.ca_cert)?)?;
        let mut trusted_pem = std::fs::read(&pki.client_cert)?;
        trusted_pem.push(b'\n');
        trusted_pem.extend_from_slice(&std::fs::read(&pki.client_key)?);
        let trusted_id = reqwest::Identity::from_pem(&trusted_pem)?;

        let mut untrusted_pem = std::fs::read(&pki.untrusted_cert)?;
        untrusted_pem.push(b'\n');
        untrusted_pem.extend_from_slice(&std::fs::read(&pki.untrusted_key)?);
        let untrusted_id = reqwest::Identity::from_pem(&untrusted_pem)?;

        let base = format!("https://127.0.0.1:{port}");

        // 1. Trusted mTLS client succeeds on /healthz and /v1/me.
        let ok_client = reqwest::Client::builder()
            .add_root_certificate(ca_cert.clone())
            .identity(trusted_id)
            .build()?;
        let health: serde_json::Value = ok_client
            .get(format!("{base}/healthz"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(health["ok"], true);
        assert_eq!(health["http_tls_port"], port);
        assert_eq!(health["tls_client_auth"], "required");
        assert_eq!(health["tls_only"], true);

        let me: serde_json::Value = ok_client
            .get(format!("{base}/v1/me"))
            .bearer_auth(&token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(me["principal"]["nick"], "mtls-http");

        // 2. Client without client certificate fails handshake.
        let no_cert_client = reqwest::Client::builder()
            .add_root_certificate(ca_cert.clone())
            .build()?;
        assert!(no_cert_client.get(format!("{base}/healthz")).send().await.is_err());

        // 3. Client with untrusted client certificate fails handshake.
        let bad_cert_client = reqwest::Client::builder()
            .add_root_certificate(ca_cert)
            .identity(untrusted_id)
            .build()?;
        assert!(bad_cert_client.get(format!("{base}/healthz")).send().await.is_err());

        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn optional_mtls_allows_unauthenticated_or_trusted_and_rejects_untrusted() -> Result<()> {
        let pki = TestPki::generate();
        let acceptor = build_tls_acceptor(&TlsServerConfigSpec {
            cert_path: pki.server_cert.clone(),
            key_path: pki.server_key.clone(),
            client_ca_path: Some(pki.ca_cert.clone()),
            client_auth: ClientAuthMode::Optional,
        })?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();

        let srv_task = tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..3 {
                let (tcp, _) = listener.accept().await.unwrap();
                match acceptor.accept(tcp).await {
                    Ok(tls) => {
                        results.push(extract_client_cert_info(tls.get_ref().1).map(|c| c.sha256));
                    }
                    Err(_) => results.push(Some("ERR".to_string())),
                }
            }
            results
        });

        let server_name = ServerName::try_from("localhost").unwrap();

        // 1. No client cert -> allowed, no ClientCertInfo.
        let no_cert = build_tls_client_connector(&pki.ca_cert, None);
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        assert!(no_cert.connect(server_name.clone(), tcp).await.is_ok());

        // 2. Untrusted client cert -> handshake fails.
        let untrusted = build_tls_client_connector(
            &pki.ca_cert,
            Some((&pki.untrusted_cert, &pki.untrusted_key)),
        );
        assert_tls_rejected(&untrusted, server_name.clone(), port, "untrusted client cert").await;

        // 3. Trusted client cert -> allowed, ClientCertInfo has 64-char hex sha256.
        let trusted = build_tls_client_connector(
            &pki.ca_cert,
            Some((&pki.client_cert, &pki.client_key)),
        );
        let tcp = TcpStream::connect(("127.0.0.1", port)).await?;
        assert!(trusted.connect(server_name, tcp).await.is_ok());

        let results = srv_task.await?;
        assert_eq!(results[0], None);
        assert_eq!(results[1].as_deref(), Some("ERR"));
        assert!(results[2].as_ref().is_some_and(|h| h.len() == 64 && h != "ERR"));
        Ok(())
    }
}
